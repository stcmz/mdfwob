use anyhow::{Context, Result, bail};
use fwob_v2::{Codec, CodecSelection, Encoding, EncodingSelection, PagePacking, WriterOptions};

use crate::config::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFormat {
    V1,
    V2,
}

#[derive(Debug, Clone, Copy)]
pub struct FwobOptions {
    pub format: TargetFormat,
    /// True when the caller explicitly requested a format (a `v1`/`v2` token). When false the
    /// `format` is just the default and an existing file's on-disk format is allowed to win; when
    /// true, appending to a file of a different format is an error rather than a silent downgrade.
    pub explicit_format: bool,
    /// True when any v2 writer tuning option was explicitly supplied. Existing v2 files inherit
    /// their stored codec and encoding when this is false.
    pub explicit_v2_options: bool,
    pub page_size: u32,
    pub codec: CodecArg,
    pub encoding: EncodingArg,
    pub zstd_level: i32,
    pub compress_partial_page: bool,
    pub page_packing: PagePacking,
}

impl Default for FwobOptions {
    fn default() -> Self {
        Self {
            format: TargetFormat::V2,
            explicit_format: false,
            explicit_v2_options: false,
            page_size: fwob_v2::DEFAULT_PAGE_SIZE,
            codec: CodecArg::Zstd,
            encoding: EncodingArg::ColumnarBasic,
            zstd_level: fwob_v2::DEFAULT_ZSTD_LEVEL,
            compress_partial_page: false,
            page_packing: fwob_v2::DEFAULT_PAGE_PACKING,
        }
    }
}

impl FwobOptions {
    pub fn v2_writer_options(self, title: impl Into<String>) -> Result<WriterOptions> {
        validate_zstd_level(self.zstd_level)?;
        let mut options = WriterOptions::new(title);
        options.page_size = self.page_size;
        options.codec = self.codec.codec();
        options.codec_selection = self.codec.selection();
        options.encoding = self.encoding.encoding();
        options.encoding_selection = self.encoding.selection();
        options.zstd_level = self.zstd_level;
        options.compress_partial_page = self.compress_partial_page;
        options.page_packing = self.page_packing;
        Ok(options)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecArg {
    None,
    Zstd,
    Lz4,
    Smallest,
}

impl CodecArg {
    fn codec(self) -> Codec {
        match self {
            Self::None => Codec::None,
            Self::Zstd | Self::Smallest => Codec::Zstd,
            Self::Lz4 => Codec::Lz4,
        }
    }

    fn selection(self) -> CodecSelection {
        match self {
            Self::None => CodecSelection::Fixed(Codec::None),
            Self::Zstd => CodecSelection::Fixed(Codec::Zstd),
            Self::Lz4 => CodecSelection::Fixed(Codec::Lz4),
            Self::Smallest => CodecSelection::Smallest,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingArg {
    RowRaw,
    ColumnarBasic,
    ColumnarDelta,
    Smallest,
}

impl EncodingArg {
    fn encoding(self) -> Encoding {
        match self {
            Self::RowRaw => Encoding::RowRawV1,
            Self::ColumnarBasic => Encoding::ColumnarBasicV1,
            Self::ColumnarDelta | Self::Smallest => Encoding::ColumnarDeltaV1,
        }
    }

    fn selection(self) -> EncodingSelection {
        match self {
            Self::RowRaw => EncodingSelection::Fixed(Encoding::RowRawV1),
            Self::ColumnarBasic => EncodingSelection::Fixed(Encoding::ColumnarBasicV1),
            Self::ColumnarDelta => EncodingSelection::Fixed(Encoding::ColumnarDeltaV1),
            Self::Smallest => EncodingSelection::Smallest,
        }
    }
}

#[derive(Debug, Default)]
pub struct ParsedTokens {
    pub symbols: Vec<String>,
    pub options: FwobOptions,
    pub provider: Option<ProviderKind>,
}

pub fn parse_tokens(values: &[String]) -> Result<ParsedTokens> {
    let mut parsed = ParsedTokens {
        symbols: Vec::new(),
        options: FwobOptions::default(),
        provider: None,
    };
    let mut codec_seen = false;
    let mut encoding_seen = false;
    let mut page_size_seen = false;
    for value in values {
        if value == "v1" {
            parsed.options.format = TargetFormat::V1;
            parsed.options.explicit_format = true;
        } else if value == "v2" {
            parsed.options.format = TargetFormat::V2;
            parsed.options.explicit_format = true;
        } else if let Some(provider) = ProviderKind::from_token(value) {
            if parsed.provider.replace(provider).is_some() {
                bail!("provider token specified more than once");
            }
        } else if let Some(page_size) = parse_page_size_token(value) {
            if page_size_seen {
                bail!("page size token specified more than once");
            }
            parsed.options.page_size = page_size?;
            parsed.options.explicit_v2_options = true;
            page_size_seen = true;
        } else if value == "smallest" {
            if !encoding_seen {
                parsed.options.encoding = EncodingArg::Smallest;
                encoding_seen = true;
            } else if !codec_seen {
                parsed.options.codec = CodecArg::Smallest;
                codec_seen = true;
            } else {
                bail!("smallest token could not be applied; codec and encoding are already set");
            }
            parsed.options.explicit_v2_options = true;
        } else if let Some(codec) = parse_codec_token(value) {
            if codec_seen {
                bail!("codec token specified more than once");
            }
            parsed.options.codec = codec;
            parsed.options.explicit_v2_options = true;
            codec_seen = true;
        } else if let Some(encoding) = parse_encoding_token(value) {
            if encoding_seen {
                bail!("encoding token specified more than once");
            }
            parsed.options.encoding = encoding;
            parsed.options.explicit_v2_options = true;
            encoding_seen = true;
        } else if value == "compress-partial-page" {
            parsed.options.compress_partial_page = true;
            parsed.options.explicit_v2_options = true;
        } else if value == "tight-fit" {
            parsed.options.page_packing = PagePacking::TightFit;
            parsed.options.explicit_v2_options = true;
        } else if value == "estimate-shrink" {
            parsed.options.page_packing = PagePacking::EstimateShrink;
            parsed.options.explicit_v2_options = true;
        } else {
            parsed.symbols.push(value.clone());
        }
    }
    Ok(parsed)
}

fn parse_codec_token(value: &str) -> Option<CodecArg> {
    match value {
        "none" => Some(CodecArg::None),
        "zstd" => Some(CodecArg::Zstd),
        "lz4" => Some(CodecArg::Lz4),
        _ => None,
    }
}

fn parse_encoding_token(value: &str) -> Option<EncodingArg> {
    match value {
        "row-raw" => Some(EncodingArg::RowRaw),
        "columnar-basic" => Some(EncodingArg::ColumnarBasic),
        "columnar-delta" => Some(EncodingArg::ColumnarDelta),
        _ => None,
    }
}

pub fn parse_page_size_token(value: &str) -> Option<Result<u32>> {
    const MIN_PAGE_SIZE: u64 = 1024;
    const MAX_PAGE_SIZE: u64 = 16 * 1024 * 1024;

    let (number, multiplier) = [
        ("KiB", 1024u64),
        ("MiB", 1024u64 * 1024),
        ("KB", 1000u64),
        ("MB", 1000u64 * 1000),
        ("B", 1u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(suffix)
            .filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|number| (number, multiplier))
    })?;

    Some((|| {
        let number: u64 = number.parse()?;
        let size = number
            .checked_mul(multiplier)
            .context("page size is too large")?;
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&size) {
            bail!("page size must be between 1KiB and 16MiB");
        }
        Ok(size as u32)
    })())
}

pub fn validate_zstd_level(level: i32) -> Result<()> {
    if !(1..=22).contains(&level) {
        bail!("--zstd-level must be between 1 and 22");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_symbols_from_fwob_tokens() {
        let parsed = parse_tokens(&[
            "AAPL".into(),
            "v2".into(),
            "1MiB".into(),
            "columnar-delta".into(),
            "zstd".into(),
        ])
        .unwrap();
        assert_eq!(parsed.symbols, ["AAPL"]);
        assert_eq!(parsed.options.page_size, 1024 * 1024);
        assert_eq!(parsed.options.encoding, EncodingArg::ColumnarDelta);
        assert_eq!(parsed.options.codec, CodecArg::Zstd);
    }

    #[test]
    fn smallest_prefers_encoding_then_codec() {
        let parsed = parse_tokens(&["AAPL".into(), "smallest".into(), "zstd".into()]).unwrap();
        assert_eq!(parsed.options.encoding, EncodingArg::Smallest);
        assert_eq!(parsed.options.codec, CodecArg::Zstd);

        let parsed =
            parse_tokens(&["AAPL".into(), "columnar-basic".into(), "smallest".into()]).unwrap();
        assert_eq!(parsed.options.encoding, EncodingArg::ColumnarBasic);
        assert_eq!(parsed.options.codec, CodecArg::Smallest);
    }

    #[test]
    fn parses_v1_format_token() {
        let parsed = parse_tokens(&["AAPL".into(), "v1".into()]).unwrap();
        assert_eq!(parsed.options.format, TargetFormat::V1);
        assert!(parsed.options.explicit_format);
    }

    #[test]
    fn format_is_not_explicit_without_a_format_token() {
        let parsed = parse_tokens(&["AAPL".into()]).unwrap();
        assert_eq!(parsed.options.format, TargetFormat::V2);
        assert!(!parsed.options.explicit_format);
        assert!(!parsed.options.explicit_v2_options);
    }

    #[test]
    fn v2_tuning_tokens_are_tracked_as_explicit() {
        for token in [
            "1MiB",
            "zstd",
            "columnar-delta",
            "compress-partial-page",
            "tight-fit",
        ] {
            let parsed = parse_tokens(&["AAPL".into(), token.into()]).unwrap();
            assert!(parsed.options.explicit_v2_options, "token {token}");
        }
    }

    #[test]
    fn tokens_are_case_sensitive() {
        let parsed =
            parse_tokens(&["AAPL".into(), "V2".into(), "ZSTD".into(), "1MIB".into()]).unwrap();

        assert_eq!(parsed.symbols, ["AAPL", "V2", "ZSTD", "1MIB"]);
        assert_eq!(parsed.options.format, TargetFormat::V2);
        assert_eq!(parsed.options.codec, CodecArg::Zstd);
    }

    #[test]
    fn provider_token_is_case_sensitive_and_does_not_steal_ibkr_symbol() {
        let parsed = parse_tokens(&["databento".into(), "IBKR".into(), "AAPL".into()]).unwrap();
        assert_eq!(parsed.provider, Some(ProviderKind::Databento));
        assert_eq!(parsed.symbols, ["IBKR", "AAPL"]);
    }
}
