//! The server's filesystem sandbox.
//!
//! Every path an MCP client can reach is resolved beneath a single root directory. Tools take
//! **symbols** (`"AAPL"`) rather than paths, so a model works in the vocabulary `ls` already gives
//! it; a relative path is still accepted for convenience, but anything that escapes the root after
//! canonicalization is rejected. This is the whole reason the server can be handed to an agent
//! without also handing it the filesystem.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use mdfwob_core::analysis::config::AnalysisConfig;

/// A canonicalized directory that bounds every path the server will open.
#[derive(Debug, Clone)]
pub(crate) struct Root {
    dir: PathBuf,
}

impl Root {
    /// Resolves the root: an explicit `--root`, else the config's `output_dir`, else the current
    /// directory. The directory must exist — a typo'd root that silently resolved to nothing would
    /// look identical to "you have no data".
    pub(crate) fn new(explicit: Option<PathBuf>, acfg: &AnalysisConfig) -> Result<Self> {
        let candidate = explicit
            .or_else(|| acfg.output_dir.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = candidate
            .canonicalize()
            .with_context(|| format!("failed to resolve root {}", candidate.display()))?;
        if !dir.is_dir() {
            bail!("root {} is not a directory", candidate.display());
        }
        Ok(Self { dir })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Renders `path` for display, relative to the root when possible, so responses never leak the
    /// absolute layout of the host filesystem.
    pub(crate) fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Resolves one caller token to a set of `.fwob` files under the root.
    ///
    /// A token is tried as a file, then a directory (its immediate `*.fwob`), then a bare symbol
    /// (`<root>/<symbol>.fwob`). Absolute paths and parent-directory components are refused
    /// outright rather than canonicalized and checked, so the rejection does not depend on whether
    /// the escaping path happens to exist.
    pub(crate) fn resolve(&self, token: &str) -> Result<Vec<PathBuf>> {
        let raw = Path::new(token);
        if raw.is_absolute() {
            bail!("{token:?} is an absolute path; pass a symbol or a path relative to the root");
        }
        if raw
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            bail!("{token:?} escapes the server root");
        }

        let direct = self.dir.join(raw);
        if direct.is_dir() {
            return self.collect_dir(&direct);
        }
        let file = if direct.is_file() {
            direct
        } else {
            let with_ext = self.dir.join(format!("{token}.fwob"));
            if !with_ext.is_file() {
                bail!("no file or symbol {token:?} under the server root");
            }
            with_ext
        };
        // Belt and braces: a symlink inside the root could still point outside it.
        self.ensure_contained(&file)?;
        Ok(vec![file])
    }

    /// Resolves a whole selection. An empty selection means "every `.fwob` directly under the
    /// root", matching what `mdfwob ls` does with no arguments.
    pub(crate) fn resolve_all(&self, tokens: &[String]) -> Result<Vec<PathBuf>> {
        if tokens.is_empty() {
            return self.collect_dir(&self.dir);
        }
        let mut out = Vec::new();
        for token in tokens {
            for path in self.resolve(token)? {
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
        Ok(out)
    }

    /// The immediate `*.fwob` files of `dir`, sorted so responses are deterministic.
    fn collect_dir(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("failed to read {}", self.display(dir)))?;
        for entry in entries {
            let path = entry
                .with_context(|| format!("failed to read {}", self.display(dir)))?
                .path();
            if path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("fwob"))
            {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Rejects a resolved path that canonicalizes outside the root (the symlink case).
    fn ensure_contained(&self, path: &Path) -> Result<()> {
        let real = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", self.display(path)))?;
        if !real.starts_with(&self.dir) {
            bail!("{} escapes the server root", self.display(path));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A unique scratch directory, matching the convention in `tests/analysis_e2e.rs` (the crate
    /// carries no dev-dependencies, so no `tempfile`).
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn root_with(files: &[&str]) -> (Scratch, Root) {
        static NONCE: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mdfwob-mcp-root-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        for name in files {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        let root = Root::new(Some(dir.clone()), &AnalysisConfig::default()).expect("root resolves");
        (Scratch(dir), root)
    }

    #[test]
    fn resolves_a_bare_symbol_to_a_fwob_file() {
        let (_scratch, root) = root_with(&["AAPL.fwob"]);
        let paths = root.resolve("AAPL").expect("symbol resolves");
        assert_eq!(paths.len(), 1);
        assert_eq!(root.display(&paths[0]), "AAPL.fwob");
    }

    #[test]
    fn empty_selection_lists_only_fwob_files_in_the_root() {
        let (_scratch, root) = root_with(&["AAPL.fwob", "MSFT.fwob", "notes.txt"]);
        let paths = root.resolve_all(&[]).expect("root lists");
        let names: Vec<String> = paths.iter().map(|p| root.display(p)).collect();
        assert_eq!(names, vec!["AAPL.fwob", "MSFT.fwob"]);
    }

    #[test]
    fn rejects_parent_directory_escapes() {
        let (_scratch, root) = root_with(&["AAPL.fwob"]);
        let error = root.resolve("../secrets").expect_err("must reject");
        assert!(
            format!("{error:#}").contains("escapes the server root"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let (_scratch, root) = root_with(&["AAPL.fwob"]);
        let absolute = if cfg!(windows) {
            "C:\\Windows\\System32\\drivers\\etc\\hosts"
        } else {
            "/etc/hosts"
        };
        let error = root.resolve(absolute).expect_err("must reject");
        assert!(
            format!("{error:#}").contains("absolute path"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn unknown_symbol_is_an_error() {
        let (_scratch, root) = root_with(&["AAPL.fwob"]);
        let error = root.resolve("NOPE").expect_err("must reject");
        assert!(
            format!("{error:#}").contains("no file or symbol"),
            "unexpected error: {error:#}"
        );
    }
}
