//! Replace the running binary with the latest GitHub release.
//!
//! Every step fails closed: the new binary is fetched, checked against the
//! release's SHA256SUMS, written beside the current one, and made to open the
//! user's real corpus before it is swapped in. The old binary is renamed, not
//! deleted, until the next start. What this does not defend against is a
//! compromised GitHub account publishing a release: the checksum ships with
//! the same release it verifies.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::fetch;

/// Hardcoded on purpose: a configurable source would let a corpus or an
/// environment variable point the updater at somebody else's binaries.
const RELEASES_URL: &str = "https://api.github.com/repos/KenKaiii/agent-steroids/releases/latest";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Release binaries are ~10 MB; anything near this is not one of ours.
const MAX_ASSET_BYTES: u64 = 100 << 20;
const NUDGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A stalled network must not hold up the command the user actually ran.
const NUDGE_DEADLINE: Duration = Duration::from_secs(3);
const STAMP_FILE: &str = "last-upgrade-check";
/// Set to anything non-empty to keep every command off the network: CI,
/// tests, and the smoke test of a freshly downloaded binary.
const OPT_OUT_ENV: &str = "STEROIDS_NO_UPGRADE";

pub enum Outcome {
    UpToDate,
    /// A newer release exists; `--check` stops here.
    Available(String),
    Upgraded {
        from: String,
        to: String,
    },
    Skipped(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Whether `STEROIDS_NO_UPGRADE` turns the updater off for this process.
pub fn disabled() -> bool {
    std::env::var_os(OPT_OUT_ENV).is_some_and(|value| !value.is_empty())
}

/// Fetch the latest release, or replace the binary with it.
pub fn upgrade(root: &Path, check_only: bool) -> Result<Outcome> {
    if disabled() {
        return Ok(Outcome::Skipped(format!("{OPT_OUT_ENV} is set")));
    }
    let release = latest(None)?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&release.tag_name, CURRENT) {
        return Ok(Outcome::UpToDate);
    }
    if check_only {
        return Ok(Outcome::Available(latest));
    }
    let Some(target) = target() else {
        return Ok(Outcome::Skipped(
            "no prebuilt binary for this platform; cargo install --git \
             https://github.com/KenKaiii/agent-steroids"
                .into(),
        ));
    };
    let wanted = format!("steroids-{target}.tar.gz");
    let url_of = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.clone())
            .with_context(|| format!("release {} has no asset {name}", release.tag_name))
    };
    let archive_url = url_of(&wanted)?;
    let sums_url = url_of("SHA256SUMS")?;

    let exe = std::env::current_exe()?.canonicalize()?;
    let staged = sibling(&exe, "new");
    // Creating the staging file is the writability check, and it happens
    // before the download so a root-owned install fails in milliseconds.
    let mut file = match std::fs::File::create(&staged) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(Outcome::Skipped(format!(
                "{} is not writable; rerun with sudo or reinstall",
                exe.display()
            )));
        }
        Err(error) => return Err(error).with_context(|| staged.display().to_string()),
    };

    let result = (|| {
        let archive = fetch::get_bytes(&archive_url, MAX_ASSET_BYTES)?;
        let sums = String::from_utf8(fetch::get_bytes(&sums_url, 1 << 16)?)?;
        verify_sha256(&archive, &sums, &wanted)?;
        let binary = extract_binary(&archive)?;
        std::io::Write::write_all(&mut file, &binary)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        smoke_test(&staged, &latest, root)?;
        swap(&exe, &staged)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error).context("upgrade aborted; the current binary is untouched");
    }
    Ok(Outcome::Upgraded {
        from: CURRENT.into(),
        to: latest,
    })
}

/// Once a day, mention a newer release on stderr. Never fails, never blocks
/// for long, never touches the network when the stamp cannot be written.
pub fn nudge(root: &Path) {
    if disabled() {
        return;
    }
    let stamp = root.join(STAMP_FILE);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last: u64 = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < NUDGE_INTERVAL.as_secs() {
        return;
    }
    // Written before the check so an offline machine asks once a day, not
    // once a command. A root we cannot write to gets no check at all.
    if std::fs::write(&stamp, now.to_string()).is_err() {
        return;
    }
    if let Ok(release) = latest(Some(NUDGE_DEADLINE))
        && is_newer(&release.tag_name, CURRENT)
    {
        eprintln!(
            "  new version {} available: steroids upgrade",
            release.tag_name.trim_start_matches('v')
        );
    }
}

/// Remove the binary a previous upgrade renamed aside. Windows cannot delete
/// a running executable, so the swap leaves it and the next start does this.
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, "old"));
    }
}

fn latest(deadline: Option<Duration>) -> Result<Release> {
    let text = fetch::get_json_within(RELEASES_URL, deadline)?;
    serde_json::from_str(&text).context("parsing the latest release")
}

/// `steroids` → `steroids.new`, `steroids.exe` → `steroids.new.exe`, so the
/// staged copy is still something Windows will execute.
fn sibling(exe: &Path, label: &str) -> PathBuf {
    let stem = exe.file_stem().unwrap_or_default().to_string_lossy();
    let name = match exe.extension() {
        Some(ext) => format!("{stem}.{label}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{label}"),
    };
    exe.with_file_name(name)
}

/// The release asset built for this binary's platform. Linux always takes
/// the static musl build, which runs on any distro whatever this one was
/// linked against.
fn target() -> Option<&'static str> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };
    let os = if cfg!(target_os = "linux") {
        "unknown-linux-musl"
    } else if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(target_os = "windows") {
        "pc-windows-msvc"
    } else {
        return None;
    };
    match (arch, os) {
        ("x86_64", "unknown-linux-musl") => Some("x86_64-unknown-linux-musl"),
        ("aarch64", "unknown-linux-musl") => Some("aarch64-unknown-linux-musl"),
        ("x86_64", "apple-darwin") => Some("x86_64-apple-darwin"),
        ("aarch64", "apple-darwin") => Some("aarch64-apple-darwin"),
        ("x86_64", "pc-windows-msvc") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// `v1.2.3` or `1.2.3` as a triple. Anything else, including pre-releases,
/// is not a version this updater will install.
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.trim().trim_start_matches('v').split('.');
    let triple = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(triple)
}

/// Strictly newer only: a re-published old tag must never roll a user back.
fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(candidate), Some(running)) => candidate > running,
        _ => false,
    }
}

/// Check `archive` against the `sha256sum`-style line for `name` in `sums`.
fn verify_sha256(archive: &[u8], sums: &str, name: &str) -> Result<()> {
    let expected = sums
        .lines()
        .filter_map(|line| {
            let (digest, file) = line.split_once(' ')?;
            (file.trim_start_matches('*').trim() == name).then_some(digest.trim())
        })
        .next()
        .with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("{name} does not match its published checksum; not installing it");
    }
    Ok(())
}

/// The one executable inside a release tarball.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if !matches!(name.as_deref(), Some("steroids" | "steroids.exe")) {
            continue;
        }
        // The compressed size was capped at download; cap the inflated size
        // too, or a crafted archive could still exhaust memory.
        let mut binary = Vec::new();
        entry.take(MAX_ASSET_BYTES + 1).read_to_end(&mut binary)?;
        if binary.len() as u64 > MAX_ASSET_BYTES {
            bail!("release binary is larger than {MAX_ASSET_BYTES} bytes");
        }
        return Ok(binary);
    }
    bail!("release archive contains no steroids binary")
}

/// Prove the new binary runs and reads the user's actual corpus before it
/// replaces anything. A schema it cannot read fails here, not after the swap.
fn smoke_test(staged: &Path, version: &str, root: &Path) -> Result<()> {
    let output = Command::new(staged)
        .arg("--version")
        .env(OPT_OUT_ENV, "1")
        .output()
        .context("running the downloaded binary")?;
    let printed = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !printed.contains(version) {
        bail!(
            "downloaded binary reports {:?}, expected {version}",
            printed.trim()
        );
    }
    let status = Command::new(staged)
        .arg("--root")
        .arg(root)
        .arg("stats")
        .env(OPT_OUT_ENV, "1")
        .stdout(std::process::Stdio::null())
        .status()
        .context("running the downloaded binary against the corpus")?;
    if !status.success() {
        bail!("downloaded binary cannot open {}", root.display());
    }
    Ok(())
}

/// Two renames: the running binary aside, the staged one into place. If the
/// second fails the first is undone, so there is always a `steroids` to run.
fn swap(exe: &Path, staged: &Path) -> Result<()> {
    let old = sibling(exe, "old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(error) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(error).with_context(|| format!("installing {}", exe.display()));
    }
    // Unix can delete a running binary; the inode lives on until exit.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_strictly_newer_and_tolerates_v_prefix() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "v0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("v0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("v0.2", "0.1.0"));
    }

    #[test]
    fn asset_name_matches_the_release_matrix() {
        let target = target().expect("tests run on a released platform");
        assert!(
            [
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-musl",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
            ]
            .contains(&target)
        );
    }

    #[test]
    fn checksum_mismatch_is_refused() {
        let archive = b"not really a tarball";
        let good = format!("{:x}", Sha256::digest(archive));
        let sums = format!("{good}  steroids-x.tar.gz\n{good}  other.tar.gz\n");
        assert!(verify_sha256(archive, &sums, "steroids-x.tar.gz").is_ok());
        let upper = format!("{}  steroids-x.tar.gz\n", good.to_uppercase());
        assert!(verify_sha256(archive, &upper, "steroids-x.tar.gz").is_ok());
        assert!(verify_sha256(b"tampered", &sums, "steroids-x.tar.gz").is_err());
        assert!(verify_sha256(archive, &sums, "missing.tar.gz").is_err());
        assert!(verify_sha256(archive, "", "steroids-x.tar.gz").is_err());
    }

    #[test]
    fn extracts_only_the_binary_from_a_tarball() -> Result<()> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_cksum();
        builder.append_data(&mut header, "README", &b"readme"[..])?;
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_cksum();
        builder.append_data(&mut header, "steroids", &b"ELF!"[..])?;
        let archive = builder.into_inner()?.finish()?;
        assert_eq!(extract_binary(&archive)?, b"ELF!");
        Ok(())
    }

    #[test]
    fn staged_and_old_names_keep_the_windows_extension() {
        assert_eq!(
            sibling(Path::new("/usr/bin/steroids"), "new"),
            Path::new("/usr/bin/steroids.new")
        );
        assert_eq!(
            sibling(Path::new("steroids.exe"), "old"),
            Path::new("steroids.old.exe")
        );
    }
}
