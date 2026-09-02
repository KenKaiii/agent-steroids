//! Where the corpus lives.
//!
//! The one setting that cannot be stored in the corpus, since it says where
//! the corpus is. Kept as a plain path in `~/.steroids/root` so a portable
//! drive can hold the corpus while the pointer stays with the machine.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// `~/.steroids`. Windows sets USERPROFILE rather than HOME, and without this
/// the corpus would land in whatever directory the command happened to run
/// from.
pub fn default() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match home {
        Some(home) => PathBuf::from(home).join(".steroids"),
        None => PathBuf::from("./corpus-data"),
    }
}

fn pointer(default: &Path) -> PathBuf {
    default.join("root")
}

/// The path `set` stored, if any.
pub fn stored(default: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(pointer(default)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Precedence: `--root`, then `STEROIDS_ROOT`, then the stored path, then
/// `~/.steroids`. The stored path fails rather than falls back when its drive
/// is absent: opening the default location instead would show an empty corpus
/// and invite re-indexing onto the internal disk.
pub fn resolve(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("STEROIDS_ROOT") {
        return Ok(PathBuf::from(path));
    }
    let default = default();
    match stored(&default) {
        // `set` created the directory, so its absence means the drive is gone.
        Some(path) if !path.is_dir() => bail!(
            "corpus root {} is not reachable (drive unplugged?). Plug it in, \
             or run `steroids config root default` to go back to {}.",
            path.display(),
            default.display()
        ),
        Some(path) => Ok(path),
        None => Ok(default),
    }
}

/// What `set` did, for the caller to report.
pub struct Changed {
    pub root: PathBuf,
    /// The default location still holds a corpus that was not carried over.
    pub left_behind: Option<PathBuf>,
}

/// Point future runs at `value`: an absolute path, or `default` to go back.
/// Creates the directory so `resolve` can tell "unplugged" from "new".
pub fn set(default: &Path, value: &str) -> Result<Changed> {
    let value = value.trim();
    let path = PathBuf::from(value);
    if value.is_empty() || value == "default" || path == default {
        let pointer = pointer(default);
        if pointer.exists() {
            std::fs::remove_file(&pointer)?;
        }
        return Ok(Changed {
            root: default.to_path_buf(),
            left_behind: None,
        });
    }
    if !path.is_absolute() {
        bail!("root must be an absolute path, got {value:?}");
    }
    // A drive root has no parent; the drive itself is then the check.
    if !path.parent().map_or(path.exists(), Path::exists) {
        bail!("{} does not exist; is the drive mounted?", path.display());
    }
    std::fs::create_dir_all(&path)?;
    std::fs::create_dir_all(default)?;
    std::fs::write(pointer(default), path.to_string_lossy().as_bytes())?;
    let left_behind = (default.join("corpus.db").exists() && !path.join("corpus.db").exists())
        .then(|| default.to_path_buf());
    Ok(Changed {
        root: path,
        left_behind,
    })
}

/// How to carry a corpus from `from` to `to`, for the user to run.
pub fn move_hint(from: &Path, to: &Path) -> String {
    format!(
        "The corpus at {} was not moved. To carry it over:\n    mv {}/corpus.db {}/blobs.bin {}/",
        from.display(),
        from.display(),
        from.display(),
        to.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_resolve_and_reset() -> Result<()> {
        let scratch = crate::store::scratch_dir("root");
        let _ = std::fs::remove_dir_all(&scratch);
        let default = scratch.join("home");
        let ssd = scratch.join("ssd").join("corpus");

        assert!(set(&default, "relative/x").is_err());
        assert!(set(&default, "/nonexistent-drive/steroids").is_err());
        std::fs::create_dir_all(ssd.parent().unwrap())?;

        let changed = set(&default, &ssd.display().to_string())?;
        assert_eq!(changed.root, ssd);
        assert!(changed.left_behind.is_none());
        assert!(ssd.is_dir());
        assert_eq!(stored(&default), Some(ssd.clone()));

        // A corpus at the default location is flagged, not silently orphaned.
        std::fs::write(default.join("corpus.db"), b"")?;
        let changed = set(&default, &ssd.display().to_string())?;
        assert_eq!(changed.left_behind.as_deref(), Some(default.as_path()));

        let back = set(&default, "default")?;
        assert_eq!(back.root, default);
        assert_eq!(stored(&default), None);

        let _ = std::fs::remove_dir_all(&scratch);
        Ok(())
    }

    /// `resolve` is tested through `stored` rather than end to end, since
    /// the real default lives under $HOME and the env is process-global.
    #[test]
    fn unplugged_drive_is_not_a_fresh_corpus() -> Result<()> {
        let scratch = crate::store::scratch_dir("root-gone");
        let _ = std::fs::remove_dir_all(&scratch);
        let default = scratch.join("home");
        let ssd = scratch.join("ssd").join("corpus");
        std::fs::create_dir_all(ssd.parent().unwrap())?;
        set(&default, &ssd.display().to_string())?;
        std::fs::remove_dir_all(ssd.parent().unwrap())?;
        let stored = stored(&default).expect("pointer written");
        assert!(!stored.is_dir(), "the check resolve relies on");
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(())
    }
}
