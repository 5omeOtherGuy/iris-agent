//! Binary self-update for `iris update`.
//!
//! Two strategies exist. Prebuilt release binaries (built by cargo-dist with
//! the `self-update` feature) download the latest GitHub release archive for
//! the current target, verify its SHA-256 checksum, and atomically replace the
//! running executable. Source builds (plain `cargo install`, feature off) have
//! no matching prebuilt to trust, so they fall back to `cargo install`.
//!
//! The decision logic and all pure helpers are compiled unconditionally so they
//! stay unit-testable; only the network download / archive-extract / replace
//! path is gated behind the `self-update` feature.

/// How `iris update` should upgrade the running binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStrategy {
    /// Download the prebuilt release binary and replace the running one.
    SelfReplace,
    /// Re-run `cargo install` (source builds without a prebuilt to trust).
    CargoInstall,
}

/// Select the update strategy for this build. Prebuilt release binaries carry
/// both the `self-update` feature *and* the `iris_dist` build marker (set only
/// by the release pipeline via `IRIS_DIST=1`, see `build.rs`), so they
/// self-replace. Any other build — including a source build with
/// `--all-features`, which turns the feature on but leaves the marker unset —
/// falls back to the cargo path.
pub fn update_strategy() -> UpdateStrategy {
    select_strategy(cfg!(iris_dist), cfg!(feature = "self-update"))
}

/// Pure decision function behind [`update_strategy`], split out so the gating
/// logic is unit-testable without depending on this build's cfg flags. A
/// prebuilt release binary requires both the dist marker and the compiled
/// self-update path; everything else uses the cargo fallback.
fn select_strategy(dist_build: bool, self_update_feature: bool) -> UpdateStrategy {
    if dist_build && self_update_feature {
        UpdateStrategy::SelfReplace
    } else {
        UpdateStrategy::CargoInstall
    }
}

/// Compile-time target triple, e.g. `x86_64-unknown-linux-gnu` (set by
/// `build.rs`). Used to pick the matching release asset.
#[allow(dead_code)]
pub const TARGET: &str = env!("IRIS_TARGET");

/// Release archive name for a target triple. cargo-dist names archives after
/// the cargo package (`iris-agent`), not the binary (`iris`), so this matches
/// its `unix-archive = ".tar.gz"` output (`iris-agent-<target>.tar.gz`).
// Consumed by the feature-gated updater and the unit tests; unused in a plain
// source build, which is expected.
#[allow(dead_code)]
pub fn asset_name(target: &str) -> String {
    format!("iris-agent-{target}.tar.gz")
}

/// Checksum sidecar name for an archive, matching cargo-dist's
/// `checksum = "sha256"` output (`<archive>.sha256`).
#[allow(dead_code)]
pub fn checksum_name(archive: &str) -> String {
    format!("{archive}.sha256")
}

/// Parse the expected lowercase-hex SHA-256 from a `.sha256` sidecar. Accepts
/// both the bare-digest form and the `<hex>  <filename>` coreutils form, and is
/// case-insensitive. Returns `None` when no 64-char hex digest is present.
#[allow(dead_code)]
pub fn parse_expected_sha256(contents: &str) -> Option<String> {
    let token = contents.split_whitespace().next()?;
    let is_hex = token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    is_hex.then(|| token.to_ascii_lowercase())
}

/// Whether `bytes` hash to `expected` (lowercase-hex SHA-256). The comparison is
/// case-insensitive on the expected side.
#[allow(dead_code)]
pub fn sha256_matches(bytes: &[u8], expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let actual = Sha256::digest(bytes);
    let actual_hex: String = actual.iter().map(|b| format!("{b:02x}")).collect();
    actual_hex.eq_ignore_ascii_case(expected)
}

#[cfg(feature = "self-update")]
pub use imp::run;

#[cfg(feature = "self-update")]
mod imp {
    use std::io::Read;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail};
    use reqwest::blocking::Client;
    use serde::Deserialize;

    use super::{TARGET, asset_name, checksum_name, parse_expected_sha256, sha256_matches};

    const RELEASES_API: &str =
        "https://api.github.com/repos/5omeOtherGuy/iris-agent/releases/latest";
    const USER_AGENT: &str = concat!("iris-agent/", env!("CARGO_PKG_VERSION"));

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

    /// Download the latest release archive for this target, verify its SHA-256,
    /// extract the `iris` binary, and atomically replace the running executable.
    pub fn run() -> Result<()> {
        let archive = asset_name(TARGET);
        let checksum = checksum_name(&archive);
        println!("Checking for the latest Iris release ({TARGET}) ...");

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .build()?;

        let release: Release = client
            .get(RELEASES_API)
            .header("Accept", "application/vnd.github+json")
            .send()?
            .error_for_status()
            .context("failed to query the latest release")?
            .json()
            .context("failed to parse the latest release metadata")?;

        let current = concat!("v", env!("CARGO_PKG_VERSION"));
        if release.tag_name == current {
            println!("Already on the latest version ({current}).");
            return Ok(());
        }

        let archive_url = asset_url(&release.assets, &archive)
            .with_context(|| format!("release {} has no asset {archive}", release.tag_name))?;
        let checksum_url = asset_url(&release.assets, &checksum)
            .with_context(|| format!("release {} has no asset {checksum}", release.tag_name))?;

        println!("Downloading {} ...", release.tag_name);
        let archive_bytes = download(&client, archive_url)?;
        let checksum_body = String::from_utf8(download(&client, checksum_url)?)
            .context("checksum file is not valid UTF-8")?;
        let expected = parse_expected_sha256(&checksum_body)
            .ok_or_else(|| anyhow!("checksum file has no SHA-256 digest"))?;

        if !sha256_matches(&archive_bytes, &expected) {
            bail!("checksum mismatch for {archive}; refusing to install");
        }

        let binary = extract_binary(&archive_bytes)
            .context("failed to extract the iris binary from the release archive")?;

        let current_exe = std::env::current_exe().context("cannot locate the running binary")?;
        write_and_replace(&current_exe, &binary)?;

        println!("Updated Iris to {}.", release.tag_name);
        Ok(())
    }

    fn asset_url<'a>(assets: &'a [Asset], name: &str) -> Option<&'a str> {
        assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
    }

    fn download(client: &Client, url: &str) -> Result<Vec<u8>> {
        let bytes = client
            .get(url)
            .send()?
            .error_for_status()
            .with_context(|| format!("failed to download {url}"))?
            .bytes()?;
        Ok(bytes.to_vec())
    }

    /// Extract the `iris` executable from a gzip tarball. cargo-dist archives
    /// place the binary at the archive root or under a single top-level dir.
    fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let is_iris = path
                .file_name()
                .is_some_and(|name| name == "iris" || name == "iris.exe");
            if is_iris {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
        bail!("no iris binary found in the release archive")
    }

    /// Write the new binary to a temp file beside the target, mark it
    /// executable, then atomically swap it over the running executable via
    /// `self-replace` (which handles the running-process case per platform).
    fn write_and_replace(current_exe: &std::path::Path, binary: &[u8]) -> Result<()> {
        let dir = current_exe
            .parent()
            .ok_or_else(|| anyhow!("running binary has no parent directory"))?;
        let tmp = dir.join(format!(".iris-update-{}", std::process::id()));

        let write_result = (|| -> Result<()> {
            std::fs::write(&tmp, binary)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
            }
            self_replace::self_replace(&tmp).context("failed to replace the running binary")?;
            Ok(())
        })();

        // Best-effort cleanup of the staged file whether or not the swap ran.
        let _ = std::fs::remove_file(&tmp);
        write_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_strategy_requires_dist_marker_and_feature() {
        // Only a prebuilt release binary (dist marker + self-update feature)
        // self-replaces. A source build with `--all-features` sets the feature
        // but not the marker, so it must still use the cargo fallback.
        assert_eq!(select_strategy(false, false), UpdateStrategy::CargoInstall);
        assert_eq!(select_strategy(false, true), UpdateStrategy::CargoInstall);
        assert_eq!(select_strategy(true, false), UpdateStrategy::CargoInstall);
        assert_eq!(select_strategy(true, true), UpdateStrategy::SelfReplace);
    }

    #[test]
    fn update_strategy_matches_build_configuration() {
        // In every build config, `update_strategy()` must agree with the pure
        // gate over this build's actual cfg flags.
        let expected = if cfg!(all(iris_dist, feature = "self-update")) {
            UpdateStrategy::SelfReplace
        } else {
            UpdateStrategy::CargoInstall
        };
        assert_eq!(update_strategy(), expected);
    }

    #[test]
    fn target_triple_is_populated() {
        assert!(!TARGET.is_empty(), "build.rs must set IRIS_TARGET");
        assert!(
            TARGET.contains('-'),
            "expected a target triple, got {TARGET}"
        );
    }

    #[test]
    fn asset_and_checksum_names_match_cargo_dist() {
        // cargo-dist names archives after the package (`iris-agent`), not the
        // binary (`iris`); the checksum sidecar appends `.sha256`.
        let archive = asset_name("x86_64-unknown-linux-gnu");
        assert_eq!(archive, "iris-agent-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(
            checksum_name(&archive),
            "iris-agent-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
    }

    #[test]
    fn parse_expected_sha256_accepts_bare_and_coreutils_forms() {
        let digest = "a".repeat(64);
        assert_eq!(parse_expected_sha256(&digest), Some(digest.clone()));

        let coreutils = format!("{digest}  iris-linux.tar.gz\n");
        assert_eq!(parse_expected_sha256(&coreutils), Some(digest.clone()));

        let upper = digest.to_ascii_uppercase();
        assert_eq!(parse_expected_sha256(&upper), Some(digest));
    }

    #[test]
    fn parse_expected_sha256_rejects_non_digests() {
        assert_eq!(parse_expected_sha256(""), None);
        assert_eq!(parse_expected_sha256("not-a-hash filename"), None);
        assert_eq!(parse_expected_sha256(&"a".repeat(63)), None);
        assert_eq!(parse_expected_sha256(&"g".repeat(64)), None);
    }

    #[test]
    fn sha256_matches_verifies_content() {
        // Known SHA-256 of the empty input.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(sha256_matches(b"", empty));
        assert!(sha256_matches(b"", &empty.to_ascii_uppercase()));
        assert!(!sha256_matches(b"tampered", empty));
    }
}
