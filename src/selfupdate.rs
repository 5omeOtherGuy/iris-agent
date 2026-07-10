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

/// Plain-stdout output in the instrument voice for `iris update`, shared by
/// both update strategies. One glyph-led line per step from the design
/// language's symbol vocabulary (`●` activity, `◆` settled, `□` skipped),
/// with a fixed verb column. ANSI color (orange glyph, dim verb) only when
/// stdout is a color-capable terminal and `NO_COLOR` is unset; state must
/// survive the monochrome test through glyph + verb alone.
pub mod voice {
    use std::io::IsTerminal;

    const ORANGE: &str = "\x1b[33m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    fn color() -> bool {
        std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
    }

    /// The `I R I S · <surface>` masthead strip, dim.
    pub fn masthead(surface: &str) {
        if color() {
            println!("{DIM}I R I S · {surface}{RESET}");
        } else {
            println!("I R I S · {surface}");
        }
    }

    /// An in-progress line: `● <verb>  <detail>`.
    pub fn step(verb: &str, detail: &str) {
        line("●", verb, detail);
    }

    /// A settled line: `◆ <verb>  <detail>`.
    pub fn done(verb: &str, detail: &str) {
        line("◆", verb, detail);
    }

    /// A refused/no-op line: `□ <verb>  <detail>`.
    pub fn skip(verb: &str, detail: &str) {
        line("□", verb, detail);
    }

    fn line(glyph: &str, verb: &str, detail: &str) {
        if color() {
            println!("{ORANGE}{glyph}{RESET} {DIM}{verb:<9}{RESET} {detail}");
        } else {
            println!("{glyph} {verb:<9} {detail}");
        }
    }
}

/// GitHub "latest release" endpoint queried by the self-replace path.
const RELEASES_API: &str = "https://api.github.com/repos/5omeOtherGuy/iris-agent/releases/latest";

/// Env var that redirects the releases-API query to a local mock server for
/// validation. Intentionally *loopback-only* (see [`ensure_loopback_url`]): it
/// exists so the real download/verify/replace path can be exercised against a
/// local server without cutting a public release, and must never let a stray
/// env var redirect a real user's update to an attacker-controlled host.
const RELEASES_API_ENV: &str = "IRIS_UPDATE_RELEASES_API_URL";

/// Resolve the releases-API URL: the pinned GitHub endpoint by default, or a
/// loopback override from [`RELEASES_API_ENV`] when set. A non-loopback or
/// malformed override is a hard error rather than a silent fallback, so the
/// override cannot be abused to point updates at a remote host.
#[allow(dead_code)]
pub fn releases_api_url() -> anyhow::Result<String> {
    match std::env::var(RELEASES_API_ENV) {
        Ok(url) if !url.is_empty() => {
            ensure_loopback_url(&url)?;
            Ok(url)
        }
        _ => Ok(RELEASES_API.to_owned()),
    }
}

/// Accept only `http`/`https` URLs whose host is loopback (`localhost` or a
/// loopback IP). Used to constrain [`RELEASES_API_ENV`].
#[allow(dead_code)]
fn ensure_loopback_url(url: &str) -> anyhow::Result<()> {
    use anyhow::{bail, ensure};
    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid {RELEASES_API_ENV}: {e}"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "{RELEASES_API_ENV} must be http(s)"
    );
    // Strip the brackets around an IPv6 literal (`[::1]`) so it parses as an IP.
    let host = parsed.host_str().unwrap_or_default();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !is_loopback {
        bail!("{RELEASES_API_ENV} must point to localhost/loopback, got {host:?}");
    }
    Ok(())
}

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

/// What `iris update` should do after comparing the latest release to the
/// running binary. Computed by [`decide_update`] and matched on by the
/// self-replace path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// The release is a strictly-newer stable version: download and replace.
    Update,
    /// The release equals the running version: nothing to do.
    UpToDate,
    /// The running binary is newer than the latest release (e.g. a local dev or
    /// prerelease build): do nothing rather than downgrade.
    Ahead,
    /// The release must not be installed: it is a prerelease/draft, carries a
    /// semver prerelease component, or either version could not be parsed.
    /// `iris update` ships only stable releases to users, so it is skipped.
    Skip,
}

/// Parse a release tag or package version as semver, tolerating a leading `v`
/// (`v1.2.3` and `1.2.3` both parse). Returns `None` for anything that is not a
/// valid semver version.
#[allow(dead_code)]
fn parse_semver(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// Decide whether to replace the running binary (`current`, this build's
/// `CARGO_PKG_VERSION`) with the latest GitHub release (`tag`, plus its
/// `prerelease`/`draft` flags).
///
/// The rules enforce "only ever install the latest *stable* release, and never
/// downgrade", so a build we cut for testing is never pushed onto users:
/// - a `prerelease`/`draft` release, or a tag carrying a semver prerelease
///   component (e.g. `1.2.0-rc.1`), is [`UpdateAction::Skip`];
/// - an unparsable `current` or `tag` is [`UpdateAction::Skip`] (refuse rather
///   than risk a wrong or downgrade replacement);
/// - otherwise compare by semver precedence: strictly-newer is
///   [`UpdateAction::Update`], equal is [`UpdateAction::UpToDate`], and an older
///   release than the running binary is [`UpdateAction::Ahead`] (no downgrade).
#[allow(dead_code)]
pub fn decide_update(current: &str, tag: &str, prerelease: bool, draft: bool) -> UpdateAction {
    if prerelease || draft {
        return UpdateAction::Skip;
    }
    let (Some(cur), Some(rel)) = (parse_semver(current), parse_semver(tag)) else {
        return UpdateAction::Skip;
    };
    if !rel.pre.is_empty() {
        return UpdateAction::Skip;
    }
    match rel.cmp(&cur) {
        std::cmp::Ordering::Greater => UpdateAction::Update,
        std::cmp::Ordering::Equal => UpdateAction::UpToDate,
        std::cmp::Ordering::Less => UpdateAction::Ahead,
    }
}

/// A GitHub release as returned by the releases API. Shared by both update
/// paths: the self-replace path reads `assets`; the cargo-install fallback
/// reads the version and prerelease/draft flags. Fields unused in a given build
/// configuration are allowed dead.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
pub struct Release {
    pub tag_name: String,
    // GitHub's `releases/latest` never returns prerelease/draft releases, but
    // parse the flags anyway so `decide_update` can enforce "stable only" as an
    // explicit invariant even under the loopback API override.
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    // Read only by the feature-gated self-replace path.
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// User-Agent sent to the GitHub API (GitHub requires one).
const USER_AGENT: &str = concat!("iris-agent/", env!("CARGO_PKG_VERSION"));

/// Query GitHub for the latest published release. `releases/latest` never
/// returns a prerelease or draft, so the result is the latest *stable* release
/// (and [`decide_update`] re-checks that). Compiled unconditionally because the
/// cargo-install fallback (source builds, no `self-update` feature) also targets
/// the latest release, not `main` — otherwise `iris update` would ship
/// bleeding-edge/testing commits to source-build users.
pub fn latest_release() -> anyhow::Result<Release> {
    use anyhow::Context;
    use std::time::Duration;
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(120))
        .build()?;
    let url = releases_api_url()?;
    let release = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()
        .context("failed to query the latest release")?
        .json()
        .context("failed to parse the latest release metadata")?;
    Ok(release)
}

#[cfg(feature = "self-update")]
pub use imp::run;

#[cfg(feature = "self-update")]
mod imp {
    use std::io::Read;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail};
    use reqwest::blocking::Client;

    use super::{
        Asset, TARGET, USER_AGENT, UpdateAction, asset_name, checksum_name, decide_update,
        latest_release, parse_expected_sha256, sha256_matches,
    };

    /// Download the latest release archive for this target, verify its SHA-256,
    /// extract the `iris` binary, and atomically replace the running executable.
    pub fn run() -> Result<()> {
        let archive = asset_name(TARGET);
        let checksum = checksum_name(&archive);
        let current = env!("CARGO_PKG_VERSION");
        super::voice::masthead("update");
        super::voice::step("check", &format!("{TARGET} · running v{current}"));

        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .build()?;

        let release = latest_release()?;
        // When the releases API is redirected to a loopback mock (validation
        // only), constrain the asset download URLs to loopback too, so the
        // override cannot be used to pull the archive/checksum from a remote
        // host. The default (unset) path trusts GitHub's real download URLs.
        let override_active = std::env::var(super::RELEASES_API_ENV)
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        match decide_update(
            current,
            &release.tag_name,
            release.prerelease,
            release.draft,
        ) {
            UpdateAction::UpToDate => {
                super::voice::done(
                    "current",
                    &format!("already on the latest release (v{current})"),
                );
                return Ok(());
            }
            UpdateAction::Ahead => {
                super::voice::done(
                    "ahead",
                    &format!(
                        "running v{current}, newer than the latest release ({}) · nothing to do",
                        release.tag_name
                    ),
                );
                return Ok(());
            }
            UpdateAction::Skip => {
                super::voice::skip(
                    "skipped",
                    &format!(
                        "latest release ({}) is not stable · iris update installs stable releases only",
                        release.tag_name
                    ),
                );
                return Ok(());
            }
            UpdateAction::Update => {}
        }

        let archive_url = asset_url(&release.assets, &archive)
            .with_context(|| format!("release {} has no asset {archive}", release.tag_name))?;
        let checksum_url = asset_url(&release.assets, &checksum)
            .with_context(|| format!("release {} has no asset {checksum}", release.tag_name))?;
        if override_active {
            super::ensure_loopback_url(archive_url)?;
            super::ensure_loopback_url(checksum_url)?;
        }

        super::voice::step("download", &format!("{} · {archive}", release.tag_name));
        let archive_bytes = download(&client, archive_url)?;
        let checksum_body = String::from_utf8(download(&client, checksum_url)?)
            .context("checksum file is not valid UTF-8")?;
        let expected = parse_expected_sha256(&checksum_body)
            .ok_or_else(|| anyhow!("checksum file has no SHA-256 digest"))?;

        if !sha256_matches(&archive_bytes, &expected) {
            bail!("checksum mismatch for {archive}; refusing to install");
        }
        super::voice::step("verify", "sha-256 ok");

        let binary = extract_binary(&archive_bytes)
            .context("failed to extract the iris binary from the release archive")?;

        let current_exe = std::env::current_exe().context("cannot locate the running binary")?;
        write_and_replace(&current_exe, &binary)?;

        super::voice::done("updated", &format!("v{current} → {}", release.tag_name));
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
    fn decide_update_installs_only_strictly_newer_stable_release() {
        // Strictly newer stable release -> update; tolerate a leading `v` on
        // either side.
        assert_eq!(
            decide_update("0.1.0", "v0.2.0", false, false),
            UpdateAction::Update
        );
        assert_eq!(
            decide_update("0.1.0", "0.1.1", false, false),
            UpdateAction::Update
        );
        // Equal version -> already up to date.
        assert_eq!(
            decide_update("0.1.0", "v0.1.0", false, false),
            UpdateAction::UpToDate
        );
    }

    #[test]
    fn decide_update_never_downgrades() {
        // The running binary is newer than the latest release (a local/dev build
        // or a prerelease build ahead of the stable line): do nothing.
        assert_eq!(
            decide_update("0.2.0", "v0.1.0", false, false),
            UpdateAction::Ahead
        );
        // A dev prerelease build is still ahead of the older stable release, not
        // downgraded onto it.
        assert_eq!(
            decide_update("0.2.0-dev", "v0.1.0", false, false),
            UpdateAction::Ahead
        );
    }

    #[test]
    fn decide_update_refuses_prerelease_and_draft_and_unparsable() {
        // Never ship a testing build to users, however the tag is flagged.
        assert_eq!(
            decide_update("0.1.0", "v0.2.0", true, false),
            UpdateAction::Skip
        );
        assert_eq!(
            decide_update("0.1.0", "v0.2.0", false, true),
            UpdateAction::Skip
        );
        // A semver prerelease component marks a testing build even if the
        // prerelease/draft flags are unset.
        assert_eq!(
            decide_update("0.1.0", "v0.2.0-rc.1", false, false),
            UpdateAction::Skip
        );
        // Unparsable versions are refused rather than risking a wrong replace.
        assert_eq!(
            decide_update("0.1.0", "nightly", false, false),
            UpdateAction::Skip
        );
        assert_eq!(
            decide_update("not-semver", "v0.2.0", false, false),
            UpdateAction::Skip
        );
    }

    #[test]
    fn sha256_matches_verifies_content() {
        // Known SHA-256 of the empty input.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(sha256_matches(b"", empty));
        assert!(sha256_matches(b"", &empty.to_ascii_uppercase()));
        assert!(!sha256_matches(b"tampered", empty));
    }

    #[test]
    fn releases_api_url_defaults_to_github_when_unset() {
        // Serialize env access across tests that mutate IRIS_UPDATE_RELEASES_API_URL.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded under ENV_LOCK; restored by the test.
        unsafe {
            std::env::remove_var(RELEASES_API_ENV);
        }
        assert_eq!(releases_api_url().unwrap(), RELEASES_API);
    }

    #[test]
    fn releases_api_url_accepts_loopback_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for url in [
            "http://127.0.0.1:8080/latest",
            "http://localhost:9000/x",
            "http://[::1]:7000/y",
        ] {
            // SAFETY: single-threaded under ENV_LOCK.
            unsafe {
                std::env::set_var(RELEASES_API_ENV, url);
            }
            assert_eq!(releases_api_url().unwrap(), url, "loopback URL must pass");
        }
        unsafe {
            std::env::remove_var(RELEASES_API_ENV);
        }
    }

    #[test]
    fn releases_api_url_rejects_non_loopback_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for url in [
            "https://evil.example.com/releases/latest",
            "http://169.254.169.254/latest", // link-local, not loopback
            "ftp://127.0.0.1/latest",        // wrong scheme
            "not-a-url",
        ] {
            // SAFETY: single-threaded under ENV_LOCK.
            unsafe {
                std::env::set_var(RELEASES_API_ENV, url);
            }
            assert!(
                releases_api_url().is_err(),
                "non-loopback override must be rejected: {url}"
            );
        }
        unsafe {
            std::env::remove_var(RELEASES_API_ENV);
        }
    }

    /// The release pipeline builds prebuilt binaries with the pinned `dist`
    /// version from `.github/workflows/release.yml` (`DIST_VERSION`), while the
    /// dist config lives in `Cargo.toml` (`cargo-dist-version`). If they drift,
    /// CI installs a different dist than the config targets and the generated
    /// artifacts can diverge from what `install.sh` / this module expect. Lock
    /// them together so the divergence fails the local gate, not a release.
    #[test]
    fn dist_version_matches_cargo_dist_version() {
        let root = env!("CARGO_MANIFEST_DIR");
        let cargo_toml = std::fs::read_to_string(format!("{root}/Cargo.toml")).unwrap();
        let release_yml =
            std::fs::read_to_string(format!("{root}/.github/workflows/release.yml")).unwrap();

        let cargo_dist_version = cargo_toml
            .lines()
            .find_map(|l| l.trim().strip_prefix("cargo-dist-version = "))
            .map(|v| v.trim().trim_matches('"'))
            .expect("cargo-dist-version in [workspace.metadata.dist]");
        let dist_version = release_yml
            .lines()
            .find_map(|l| l.trim().strip_prefix("DIST_VERSION: "))
            .map(|v| v.trim().trim_matches('"'))
            .expect("DIST_VERSION in release.yml env");

        assert_eq!(
            dist_version, cargo_dist_version,
            "release.yml DIST_VERSION ({dist_version}) must equal Cargo.toml cargo-dist-version ({cargo_dist_version})"
        );
    }

    /// `install.sh` downloads `$PKG-$target.tar.gz` with `PKG=iris-agent`; this
    /// module downloads `asset_name(target)`. Both must equal cargo-dist's
    /// package-named archive, or a release built by dist would be un-installable
    /// by one of the two paths. Guard the shared `iris-agent-` prefix + suffix.
    #[test]
    fn install_sh_and_asset_name_agree_on_archive_scheme() {
        let root = env!("CARGO_MANIFEST_DIR");
        let install_sh = std::fs::read_to_string(format!("{root}/install.sh")).unwrap();
        assert!(
            install_sh.contains("PKG=\"iris-agent\""),
            "install.sh must name archives after package iris-agent"
        );
        assert!(
            install_sh.contains("archive=\"$PKG-$target.tar.gz\""),
            "install.sh archive name must be $PKG-$target.tar.gz"
        );
        let asset = asset_name("x86_64-unknown-linux-gnu");
        assert_eq!(asset, "iris-agent-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(checksum_name(&asset), format!("{asset}.sha256"));
    }

    use std::sync::Mutex;
    /// Serializes tests that mutate the process-wide `IRIS_UPDATE_RELEASES_API_URL`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
