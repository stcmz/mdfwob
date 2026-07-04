use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use fwob::{
    FormatVersion, Maintenance, OperationOptions, Reader, ReaderOptions, Writer, detect_format,
};
use fwob_core::Key;
use tracing::warn;

use crate::{
    fwob_options::{FwobOptions, TargetFormat},
    tick::{Tick, tick_schema},
};

pub struct TickStore {
    path: PathBuf,
    title: String,
    options: FwobOptions,
}

impl TickStore {
    pub fn new(output_dir: &Path, title: impl Into<String>, options: FwobOptions) -> Self {
        let title = title.into();
        Self {
            path: output_dir.join(format!("{title}.fwob")),
            title,
            options,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn try_lock(&self) -> Result<TickStoreLock> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_path = self.path.with_extension("fwob.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        FileExt::try_lock_exclusive(&file).with_context(|| {
            format!(
                "{} is already being written by another mdfwob process",
                self.path.display()
            )
        })?;
        Ok(TickStoreLock { file })
    }

    pub fn last_timestamp(&self) -> Result<Option<u32>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let mut reader = Reader::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        reader
            .last_key()?
            .map(|key| key_to_u32(key, &self.path))
            .transpose()
    }

    /// Rejects appending to an existing file whose on-disk format conflicts with an explicitly
    /// requested one (e.g. `download v2` against a file that is already v1). When no format was
    /// explicitly requested the existing file's format wins, so this is a no-op.
    pub fn ensure_compatible_format(&self) -> Result<()> {
        if !self.options.explicit_format || !self.path.exists() {
            return Ok(());
        }
        let actual = detect_format(&self.path)
            .with_context(|| format!("failed to detect format of {}", self.path.display()))?;
        let matches = matches!(
            (actual, self.options.format),
            (FormatVersion::V1, TargetFormat::V1) | (FormatVersion::V2, TargetFormat::V2)
        );
        if !matches {
            bail!(
                "{} already exists as {}, but {} was requested; \
                 append to the existing format or choose a different output",
                self.path.display(),
                format_label(actual),
                target_label(self.options.format),
            );
        }
        Ok(())
    }

    pub fn verify_existing(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let reader_options = ReaderOptions::default();
        if let Err(error) = Maintenance::light_verify(&self.path, reader_options) {
            warn!(
                path = %self.path.display(),
                %error,
                "light FWOB verification failed; repairing committed tail"
            );
            Maintenance::repair(&self.path, reader_options)
                .with_context(|| format!("FWOB repair failed for {}", self.path.display()))?;
        }
        Ok(())
    }

    /// Opens a session that keeps a single writer open across appended batches.
    ///
    /// Avoids the open→append→finish churn of one cycle per batch: callers append every batch into
    /// the same `TickWriter`. Call [`TickWriter::commit`] periodically to durably flush the file
    /// mid-download (the writer stays open, so the file is never reopened) and
    /// [`TickWriter::finish`] once at the end.
    pub fn writer(&self) -> TickWriter<'_> {
        TickWriter {
            store: self,
            writer: None,
        }
    }

    /// Appends a single batch by opening, appending, and finishing in one shot. Convenience for
    /// callers that write exactly one batch (and the unit tests). Multi-batch callers should use
    /// [`TickStore::writer`] to keep the writer open.
    #[allow(dead_code)]
    pub fn append_ticks(&self, ticks: &[Tick]) -> Result<()> {
        let mut writer = self.writer();
        writer.append_ticks(ticks)?;
        writer.finish()
    }

    fn open_or_create_writer(&self) -> Result<Writer> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.path.exists() {
            let open_options = match detect_format(&self.path)? {
                FormatVersion::V1 => OperationOptions::default(),
                FormatVersion::V2 => OperationOptions {
                    v2: self
                        .options
                        .explicit_v2_options
                        .then(|| self.options.v2_writer_options(&self.title))
                        .transpose()?,
                    ..OperationOptions::default()
                },
            };
            Writer::open(&self.path, open_options)
                .with_context(|| format!("failed to open {} for append", self.path.display()))
        } else {
            match self.options.format {
                TargetFormat::V1 => Writer::create_v1(
                    &self.path,
                    tick_schema(),
                    fwob_v1::WriterOptions::new(&self.title),
                    &[],
                )
                .with_context(|| format!("failed to create v1 {}", self.path.display())),
                TargetFormat::V2 => Writer::create_v2(
                    &self.path,
                    tick_schema(),
                    self.options.v2_writer_options(&self.title)?,
                )
                .with_context(|| format!("failed to create v2 {}", self.path.display())),
            }
        }
    }
}

pub struct TickStoreLock {
    file: File,
}

impl Drop for TickStoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// A writer held open across batches for one symbol. The underlying file/writer is created
/// lazily on the first non-empty batch, so a symbol that yields no ticks never creates a file.
pub struct TickWriter<'a> {
    store: &'a TickStore,
    writer: Option<Writer>,
}

impl TickWriter<'_> {
    pub fn append_ticks(&mut self, ticks: &[Tick]) -> Result<()> {
        if ticks.is_empty() {
            return Ok(());
        }
        let writer = match &mut self.writer {
            Some(writer) => writer,
            None => self.writer.insert(self.store.open_or_create_writer()?),
        };

        let mut encoded = Vec::with_capacity(ticks.len() * 12);
        for tick in ticks {
            tick.encode(&mut encoded);
        }
        Ok(writer.append_frames_transactional(&encoded)?)
    }

    /// Durably commits everything appended so far without closing the writer: it flushes the open
    /// writer to disk (via `fwob::Writer::sync`) and keeps it open for the next batch. A commit is a
    /// checkpoint, never a change to the eventual file — the bytes are identical whether or not (and
    /// however often) `commit` is called, so the download's commit cadence cannot affect the output.
    /// Lets a long download advance the on-disk file mid-run so a crash loses at most the ticks
    /// appended since the last commit.
    pub fn commit(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.sync()?;
        }
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        if let Some(writer) = self.writer {
            writer.finish()?;
        }
        Ok(())
    }
}

fn format_label(format: FormatVersion) -> &'static str {
    match format {
        FormatVersion::V1 => "fwob-v1",
        FormatVersion::V2 => "fwob-v2",
    }
}

fn target_label(format: TargetFormat) -> &'static str {
    match format {
        TargetFormat::V1 => "fwob-v1",
        TargetFormat::V2 => "fwob-v2",
    }
}

fn key_to_u32(key: Key, path: &Path) -> Result<u32> {
    match key {
        Key::U32(value) => Ok(value),
        other => bail!("unexpected key type in {}: {other:?}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn appends_and_reads_last_timestamp_v2() {
        let dir = temp_dir("mdfwob-storage-v2");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        append_twice_and_assert(&store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exclusive_lock_prevents_concurrent_writers() {
        let dir = temp_dir("mdfwob-lock");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let first = store.try_lock().unwrap();
        assert!(store.try_lock().is_err());
        drop(first);
        let second = store.try_lock().unwrap();
        drop(second);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn creates_and_resumes_v1() {
        let dir = temp_dir("mdfwob-storage-v1");
        let options = options_for(TargetFormat::V1);
        let store = TickStore::new(&dir, "AAPL", options);
        append_twice_and_assert(&store);
        assert_eq!(detect_format(store.path()).unwrap(), FormatVersion::V1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_v1_format_wins_over_default_v2() {
        let dir = temp_dir("mdfwob-existing-v1");
        let v1_options = options_for(TargetFormat::V1);
        TickStore::new(&dir, "SPOT", v1_options)
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();

        let default_options = FwobOptions {
            zstd_level: 999,
            ..FwobOptions::default()
        };
        let default_store = TickStore::new(&dir, "SPOT", default_options);
        default_store
            .append_ticks(&[Tick::new(11, 1.24, 200).unwrap()])
            .unwrap();
        assert_eq!(default_store.last_timestamp().unwrap(), Some(11));
        assert_eq!(
            detect_format(default_store.path()).unwrap(),
            FormatVersion::V1
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_v2_inherits_codec_and_encoding_without_override_tokens() {
        let dir = temp_dir("mdfwob-existing-v2-options");
        let initial_options = FwobOptions {
            page_size: 1024,
            codec: crate::fwob_options::CodecArg::Lz4,
            encoding: crate::fwob_options::EncodingArg::ColumnarDelta,
            compress_partial_page: true,
            explicit_v2_options: true,
            ..FwobOptions::default()
        };
        let initial_store = TickStore::new(&dir, "AAPL", initial_options);
        initial_store
            .append_ticks(
                &(0..200)
                    .map(|time| Tick::new(time, 1.23, 100).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let resumed_store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        resumed_store
            .append_ticks(
                &(200..400)
                    .map(|time| Tick::new(time, 1.24, 100).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let mut reader = fwob_v2::Reader::open(resumed_store.path()).unwrap();
        assert_eq!(reader.header().page_size, 1024);
        for page_index in 0..reader.header().page_count {
            let page = reader.read_page_header(page_index).unwrap();
            if page.codec != fwob_v2::Codec::None {
                assert_eq!(page.codec, fwob_v2::Codec::Lz4);
                assert_eq!(page.encoding, fwob_v2::Encoding::ColumnarDeltaV1);
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verifies_existing_files() {
        for (label, format) in [("v1", TargetFormat::V1), ("v2", TargetFormat::V2)] {
            let dir = temp_dir(&format!("mdfwob-verify-{label}"));
            let options = options_for(format);
            let store = TickStore::new(&dir, "AAPL", options);
            store
                .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
                .unwrap();
            store.verify_existing().unwrap();
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn repairs_uncommitted_v1_tail_only_after_light_check_fails() {
        let dir = temp_dir("mdfwob-repair-v1");
        let options = options_for(TargetFormat::V1);
        let store = TickStore::new(&dir, "AAPL", options);
        store
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();
        append_garbage(store.path());

        store.verify_existing().unwrap();
        assert_eq!(store.last_timestamp().unwrap(), Some(10));
        Maintenance::verify(store.path(), ReaderOptions::default()).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repairs_uncommitted_v2_tail_only_after_light_check_fails() {
        let dir = temp_dir("mdfwob-repair-v2");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        store
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();
        append_garbage(store.path());

        store.verify_existing().unwrap();
        assert_eq!(store.last_timestamp().unwrap(), Some(10));
        Maintenance::verify(store.path(), ReaderOptions::default()).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_batch_does_not_partially_append() {
        let dir = temp_dir("mdfwob-transactional-append");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        store
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();

        let error = store
            .append_ticks(&[
                Tick::new(12, 1.25, 300).unwrap(),
                Tick::new(11, 1.24, 200).unwrap(),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("key"));
        assert_eq!(store.last_timestamp().unwrap(), Some(10));
        let report = Maintenance::verify(store.path(), ReaderOptions::default()).unwrap();
        assert_eq!(report.frame_count, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_format_conflict_is_rejected_but_default_yields_to_existing() {
        let dir = temp_dir("mdfwob-format-conflict");
        // Create a v1 file.
        let v1_options = FwobOptions {
            format: TargetFormat::V1,
            explicit_format: true,
            ..FwobOptions::default()
        };
        TickStore::new(&dir, "AAPL", v1_options)
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();

        // Explicitly requesting v2 against the existing v1 file is an error.
        let explicit_v2 = FwobOptions {
            explicit_format: true,
            ..FwobOptions::default()
        };
        let error = TickStore::new(&dir, "AAPL", explicit_v2)
            .ensure_compatible_format()
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));

        // The default (non-explicit) V2 yields to the existing v1 format without error.
        let default_v2 = FwobOptions::default();
        assert!(!default_v2.explicit_format);
        TickStore::new(&dir, "AAPL", default_v2)
            .ensure_compatible_format()
            .unwrap();

        // Explicitly requesting the matching v1 format is fine.
        TickStore::new(&dir, "AAPL", v1_options)
            .ensure_compatible_format()
            .unwrap();

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_keeps_one_writer_open_across_batches() {
        let dir = temp_dir("mdfwob-session");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());

        let mut writer = store.writer();
        let mut total = 0u64;
        for batch in 0..5u32 {
            let ticks: Vec<Tick> = (0..10)
                .map(|i| Tick::new(batch * 10 + i, 1.23, 100).unwrap())
                .collect();
            writer.append_ticks(&ticks).unwrap();
            total += ticks.len() as u64;
        }
        writer.finish().unwrap();

        assert_eq!(store.last_timestamp().unwrap(), Some(49));
        let report = Maintenance::verify(store.path(), ReaderOptions::default()).unwrap();
        assert_eq!(report.frame_count, total);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn commit_flushes_mid_session_without_closing_the_writer() {
        let dir = temp_dir("mdfwob-commit");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());

        let mut writer = store.writer();
        writer
            .append_ticks(&[Tick::new(10, 1.23, 100).unwrap()])
            .unwrap();
        // A commit (no final finish yet) must make the first batch durably readable.
        writer.commit().unwrap();
        assert_eq!(store.last_timestamp().unwrap(), Some(10));

        // The same writer keeps going on the still-open handle (no reopen).
        writer
            .append_ticks(&[Tick::new(11, 1.24, 200).unwrap()])
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(store.last_timestamp().unwrap(), Some(11));

        let report = Maintenance::verify(store.path(), ReaderOptions::default()).unwrap();
        assert_eq!(report.frame_count, 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn commit_cadence_does_not_change_the_file() {
        // The on-disk bytes must be identical whether the session commits after every batch or only
        // finishes once at the end — the commit cadence is a durability checkpoint, never a change
        // to the output. Use many batches of noisy ticks so v2 forms several compressed pages plus
        // a raw residual, exercising the reclaim/recompaction path on each commit.
        let batches: Vec<Vec<Tick>> = (0..40)
            .map(|batch: u32| {
                (0..50)
                    .map(|i| {
                        let time = batch * 50 + i;
                        let price = 1.0 + f64::from(time.wrapping_mul(2_654_435_761)) / 1.0e6;
                        Tick::new(time, price, (time as i32).wrapping_mul(7) + 1).unwrap()
                    })
                    .collect()
            })
            .collect();

        let build = |commit_every_batch: bool| -> Vec<u8> {
            let dir = temp_dir(&format!("mdfwob-cadence-{commit_every_batch}"));
            let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
            let mut writer = store.writer();
            for batch in &batches {
                writer.append_ticks(batch).unwrap();
                if commit_every_batch {
                    writer.commit().unwrap();
                }
            }
            writer.finish().unwrap();
            let bytes = fs::read(store.path()).unwrap();
            fs::remove_dir_all(dir).unwrap();
            bytes
        };

        assert_eq!(
            build(true),
            build(false),
            "committing every batch changed the resulting file"
        );
    }

    #[test]
    fn empty_session_creates_no_file() {
        let dir = temp_dir("mdfwob-session-empty");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let writer = store.writer();
        writer.finish().unwrap();
        assert!(!store.path().exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn field_semantics_persist_in_v2_and_drop_in_v1() {
        use fwob_core::{FieldSemantic, TimestampUnit};

        for (label, format, time_expected, price_expected) in [
            (
                "v2",
                TargetFormat::V2,
                FieldSemantic::UnixTimestamp(TimestampUnit::Seconds),
                FieldSemantic::FixedPoint(4),
            ),
            (
                "v1",
                TargetFormat::V1,
                FieldSemantic::None,
                FieldSemantic::None,
            ),
        ] {
            let dir = temp_dir(&format!("mdfwob-semantic-{label}"));
            let options = options_for(format);
            let store = TickStore::new(&dir, "AAPL", options);
            store
                .append_ticks(&[Tick::new(1_761_000_000, 1.23, 100).unwrap()])
                .unwrap();

            let reader = Reader::open(store.path()).unwrap();
            assert_eq!(reader.schema().fields[0].name, "time");
            assert_eq!(reader.schema().fields[0].semantic, time_expected);
            assert_eq!(reader.schema().fields[1].name, "price");
            assert_eq!(reader.schema().fields[1].semantic, price_expected);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    fn append_twice_and_assert(store: &TickStore) {
        store
            .append_ticks(&[
                Tick::new(10, 1.23, 100).unwrap(),
                Tick::new(11, 1.24, 200).unwrap(),
            ])
            .unwrap();
        store
            .append_ticks(&[Tick::new(12, 1.25, 300).unwrap()])
            .unwrap();
        assert_eq!(store.last_timestamp().unwrap(), Some(12));
    }

    fn options_for(format: TargetFormat) -> FwobOptions {
        FwobOptions {
            format,
            ..FwobOptions::default()
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nonce}"))
    }

    fn append_garbage(path: &Path) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&[0xaa, 0xbb, 0xcc])
            .unwrap();
    }
}
