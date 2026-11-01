//! `anvil update`: fetch the latest release, verify it, swap the binary in.
//!
//! release.yml tags every merge to `main` as `<date>-<short sha>` and writes
//! two sections into the notes: `## Changes`, the merged ticket titles, and
//! `## SHA256`, one line per asset. The installed binary knows its own sha
//! (`ANVIL_BUILD_SHA`, baked in by the workflow), so the update stops when
//! nothing is newer and prints the changes of every release in between.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The commit this binary was built from, when release.yml built it.
pub const BUILD_SHA: Option<&str> = option_env!("ANVIL_BUILD_SHA");

/// The repository release.yml publishes to, when it built this binary.
pub const RELEASE_REPO: Option<&str> = option_env!("ANVIL_RELEASE_REPO");

/// The target triple this binary was built for.
pub const TARGET: &str = env!("ANVIL_TARGET");

/// Environment variable that overrides the GitHub API base, for tests.
pub const API_ENV: &str = "ANVIL_GITHUB_API_URL";

/// Where the real API lives.
const DEFAULT_API: &str = "https://api.github.com";

/// Releases fetched when assembling the changelog.
const RELEASES_PAGE: usize = 50;

/// What `anvil update` needs.
pub struct UpdateOptions {
    /// `owner/repo`.
    pub repo: String,
    /// The API base URL.
    pub api: String,
    /// The sha this binary was built from, if known.
    pub installed_sha: Option<String>,
    /// The binary to replace.
    pub exe: PathBuf,
    /// The target triple, which names the asset.
    pub target: String,
}

impl UpdateOptions {
    /// Options for the running binary.
    ///
    /// # Arguments
    ///
    /// * `repo_from_config` - `[github] repo` from a `factory.toml` in the
    ///   current repository, used when the binary was not built by release.yml.
    /// * `exe` - The binary to replace; `None` means this executable.
    ///
    /// # Returns
    ///
    /// The options.
    ///
    /// # Errors
    ///
    /// When no repository is known or the executable path cannot be found.
    pub fn for_this_binary(
        repo_from_config: Option<&str>,
        exe: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let repo = RELEASE_REPO
            .map(str::to_owned)
            .or_else(|| repo_from_config.map(str::to_owned))
            .context(
                "this anvil was built from source without a release repository; run `anvil update` inside the monorepo (factory.toml names it) or install a release build",
            )?;
        let exe = match exe {
            Some(exe) => exe,
            None => std::env::current_exe().context("finding this executable")?,
        };
        Ok(Self {
            repo,
            api: std::env::var(API_ENV).unwrap_or_else(|_| DEFAULT_API.to_owned()),
            installed_sha: BUILD_SHA.map(str::to_owned),
            exe,
            target: TARGET.to_owned(),
        })
    }
}

/// One release as the API lists it.
#[derive(Debug, Clone, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Runs the update.
///
/// # Arguments
///
/// * `options` - What to update from and to.
///
/// # Returns
///
/// `0` when the binary is current or was replaced.
///
/// # Errors
///
/// When the API cannot be reached, the asset is missing, the checksum does
/// not match, or the binary cannot be replaced.
pub async fn run(options: UpdateOptions) -> anyhow::Result<i32> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("anvil/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")?;
    let releases = list_releases(&client, &options.api, &options.repo).await?;
    let Some(latest) = releases.first() else {
        println!("no releases yet in {}", options.repo);
        return Ok(0);
    };
    let newer = releases_since(&releases, options.installed_sha.as_deref());
    if newer.is_empty() {
        println!(
            "anvil is up to date ({}, built from {})",
            latest.tag_name,
            options.installed_sha.as_deref().unwrap_or("unknown")
        );
        return Ok(0);
    }

    let asset_name = asset_name(&options.target);
    let asset = latest
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .with_context(|| {
            format!(
                "release {} has no asset `{asset_name}` for this platform",
                latest.tag_name
            )
        })?;
    let expected = expected_sha256(latest.body.as_deref().unwrap_or(""), &asset_name)
        .with_context(|| {
            format!(
                "release {} does not list a SHA256 for `{asset_name}`; refusing to install it",
                latest.tag_name
            )
        })?;
    println!("downloading {} from {}", asset.name, latest.tag_name);
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("downloading {}", asset.browser_download_url))?
        .bytes()
        .await
        .context("reading the download")?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!(
            "checksum mismatch for {asset_name}: release notes say {expected}, download is {actual}; nothing was changed"
        );
    }
    swap_binary(&options.exe, &bytes)?;
    println!(
        "installed {} ({} bytes) to {}",
        latest.tag_name,
        bytes.len(),
        options.exe.display()
    );
    println!("\n{}", changelog(&newer));
    Ok(0)
}

/// Fetches the releases, newest first, without drafts and prereleases.
async fn list_releases(
    client: &reqwest::Client,
    api: &str,
    repo: &str,
) -> anyhow::Result<Vec<Release>> {
    let url = format!(
        "{}/repos/{repo}/releases?per_page={RELEASES_PAGE}",
        api.trim_end_matches('/')
    );
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !response.status().is_success() {
        bail!(
            "GitHub answered HTTP {} for {url}",
            response.status().as_u16()
        );
    }
    let releases: Vec<Release> = response.json().await.context("parsing the release list")?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .collect())
}

/// The releases newer than the installed one, newest first.
///
/// A tag ends with the short sha of its commit; the installed sha is a
/// prefix match on that. Without an installed sha, only the latest release
/// counts as new, since there is nothing to walk back to.
fn releases_since(releases: &[Release], installed_sha: Option<&str>) -> Vec<Release> {
    let Some(installed) = installed_sha.filter(|sha| sha.len() >= 7) else {
        return releases.iter().take(1).cloned().collect();
    };
    let mut newer = Vec::new();
    for release in releases {
        if tag_matches_sha(&release.tag_name, installed) {
            return newer;
        }
        newer.push(release.clone());
    }
    newer
}

/// Whether a `<date>-<short sha>` tag names the given commit.
fn tag_matches_sha(tag: &str, sha: &str) -> bool {
    let Some(short) = tag.rsplit('-').next() else {
        return false;
    };
    !short.is_empty() && sha.starts_with(short)
}

/// The asset name for a target triple.
fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("anvil-{target}.exe")
    } else {
        format!("anvil-{target}")
    }
}

/// The SHA256 the release notes list for an asset.
fn expected_sha256(notes: &str, asset: &str) -> Option<String> {
    notes.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        let name = parts.next()?.trim_start_matches("./");
        let is_hex = hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit());
        (is_hex && name == asset).then(|| hex.to_ascii_lowercase())
    })
}

/// Lowercase hex SHA256 of the bytes.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `## Changes` sections of the releases, newest first.
fn changelog(releases: &[Release]) -> String {
    let mut text = String::from("Changes since the installed version:\n");
    for release in releases {
        text.push_str(&format!("\n{}\n", release.tag_name));
        let body = release.body.as_deref().unwrap_or("");
        let changes = changes_section(body);
        if changes.is_empty() {
            text.push_str("  (no notes)\n");
        }
        for line in changes {
            text.push_str(&format!("  {line}\n"));
        }
    }
    text
}

/// The lines under `## Changes`, up to the next heading.
fn changes_section(notes: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut inside = false;
    for line in notes.lines() {
        if line.starts_with("## ") {
            inside = line.trim() == "## Changes";
            continue;
        }
        if inside && !line.trim().is_empty() {
            lines.push(line.trim().to_owned());
        }
    }
    lines
}

/// Writes the new binary next to the old one, then renames both so a
/// failure half way leaves a working `anvil`.
///
/// Windows cannot delete a running executable, so the old binary stays as
/// `anvil.exe.old` until the next update removes it.
fn swap_binary(exe: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let new = exe.with_extension("new");
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::write(&new, bytes).with_context(|| format!("writing {}", new.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", new.display()))?;
    }
    std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(error) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(error).with_context(|| format!("installing {}", exe.display()));
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, body: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            body: Some(body.to_owned()),
            draft: false,
            prerelease: false,
            assets: Vec::new(),
        }
    }

    #[test]
    fn releases_since_stop_at_the_installed_sha() {
        let releases = vec![
            release("2026-11-02-ccc3333", ""),
            release("2026-11-01-bbb2222", ""),
            release("2026-10-31-aaa1111", ""),
        ];
        let newer = releases_since(&releases, Some("bbb2222f00d"));
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].tag_name, "2026-11-02-ccc3333");
        assert!(releases_since(&releases, Some("ccc3333abcd")).is_empty());
        assert_eq!(releases_since(&releases, Some("unknown0")).len(), 3);
        assert_eq!(
            releases_since(&releases, None).len(),
            1,
            "a source build sees the latest only"
        );
        assert!(tag_matches_sha("2026-10-31-aaa1111", "aaa1111deadbeef"));
        assert!(!tag_matches_sha("2026-10-31-aaa1111", "bbb2222"));
    }

    #[test]
    fn asset_names_and_checksums_follow_the_release_notes() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            "anvil-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "anvil-x86_64-pc-windows-msvc.exe"
        );
        let hex = "a".repeat(64);
        let notes = format!(
            "## Changes\n\n- Add a grep tool\n\n## SHA256\n\n{hex}  ./anvil-x86_64-unknown-linux-gnu\n{}  anvil-x86_64-pc-windows-msvc.exe\n",
            "B".repeat(64)
        );
        assert_eq!(
            expected_sha256(&notes, "anvil-x86_64-unknown-linux-gnu").as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            expected_sha256(&notes, "anvil-x86_64-pc-windows-msvc.exe"),
            Some("b".repeat(64))
        );
        assert_eq!(expected_sha256(&notes, "anvil-other"), None);
        assert_eq!(
            expected_sha256("## SHA256\n\nnothex  anvil-other\n", "anvil-other"),
            None
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            changes_section(&notes),
            vec!["- Add a grep tool".to_owned()]
        );
        let text = changelog(&[release("2026-11-02-ccc3333", &notes), release("x", "")]);
        assert!(
            text.contains("2026-11-02-ccc3333\n  - Add a grep tool\n"),
            "{text}"
        );
        assert!(text.contains("x\n  (no notes)\n"), "{text}");
    }

    #[test]
    fn swap_replaces_the_binary_and_keeps_a_fallback_on_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("anvil.exe");
        std::fs::write(&exe, b"old").expect("write");
        swap_binary(&exe, b"new binary").expect("swaps");
        assert_eq!(std::fs::read(&exe).expect("read"), b"new binary");
        assert!(!dir.path().join("anvil.new").exists());
    }
}
