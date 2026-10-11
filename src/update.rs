//! Self-update from GitHub Releases: check the latest release, download its zip, swap the .app in place.
//! Uses the system's curl and ditto. Files curl downloads aren't quarantined, so updates skip Gatekeeper.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASES: &str = "https://api.github.com/repos/Debuggedd/Masthead/releases/latest";
const ASSET: &str = "Masthead-macos-arm64.zip";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// The release page (fallback when not running from an .app bundle).
    pub page: String,
    pub zip: String,
}

/// The latest release when it's newer than this build. MASTHEAD_RELEASES_URL overrides the API URL (tests).
pub fn check() -> Result<Option<Release>> {
    let url = std::env::var("MASTHEAD_RELEASES_URL").unwrap_or(RELEASES.into());
    let out = curl(&["-H", "Accept: application/vnd.github+json", &url])?;
    let json: serde_json::Value = serde_json::from_slice(&out)?;
    let tag = json["tag_name"].as_str().context("no tag_name")?;
    if !newer(tag, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let zip = json["assets"].as_array().into_iter().flatten().find(|a| a["name"] == ASSET);
    Ok(Some(Release {
        version: tag.trim_start_matches('v').into(),
        page: json["html_url"].as_str().unwrap_or_default().into(),
        zip: zip
            .and_then(|a| a["browser_download_url"].as_str())
            .context("release has no app zip")?
            .into(),
    }))
}

/// The .app bundle this binary runs from (None for `cargo run`).
pub fn current_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?; // Masthead.app/Contents/MacOS/Masthead
    (app.extension()? == "app").then(|| app.to_path_buf())
}

/// Downloads the release zip and replaces `app` with the bundle inside it. The old bundle is kept until
/// the new one is in place, so a failure leaves the current install untouched.
pub fn install(zip_url: &str, app: &Path) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("masthead-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let zip = tmp.join(ASSET);
    curl(&["-o", zip.to_str().unwrap(), zip_url])?;
    run(Command::new("ditto").args(["-x", "-k"]).arg(&zip).arg(&tmp))?;
    let new = tmp.join("Masthead.app");
    if !new.join("Contents/MacOS/Masthead").exists() {
        bail!("the download doesn't contain Masthead.app");
    }
    // Copy next to the current app first (same volume), then swap with two renames.
    let dir = app.parent().context("app has no parent folder")?;
    let staged = dir.join(".Masthead.app.new");
    let old = dir.join(".Masthead.app.old");
    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&old);
    run(Command::new("ditto").arg(&new).arg(&staged)).with_context(|| format!("can't write to {}", dir.display()))?;
    std::fs::rename(app, &old)?;
    if let Err(e) = std::fs::rename(&staged, app) {
        let _ = std::fs::rename(&old, app);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Opens `app` once this process has exited (the caller quits right after).
pub fn relaunch(app: &Path) -> Result<()> {
    let script = format!("while kill -0 {} 2>/dev/null; do sleep 0.2; done; open \"$0\"", std::process::id());
    Command::new("sh").args(["-c", &script]).arg(app).spawn()?;
    Ok(())
}

/// `a` is a newer version than `b` (both like "1.2.3", optional leading "v"; pre-release suffixes ignored).
fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a) > parse(b)
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "120"]).args(args).output()?;
    if !out.status.success() {
        bail!("curl: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[test]
fn versions() {
    assert!(newer("v0.2.0", "0.1.0"));
    assert!(newer("0.10.0", "0.9.9"));
    assert!(!newer("v0.1.0", "0.1.0"));
    assert!(!newer("0.0.9", "0.1.0"));
}

/// Full update round trip on a copy of dist/Masthead.app (run `MASTHEAD_RELEASE=1 scripts/bundle.sh` first).
#[test]
#[ignore]
fn install_swaps_bundle() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join("masthead-install-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("Applications")).unwrap();
    let app = dir.join("Applications/Masthead.app");
    run(Command::new("ditto").arg(root.join("dist/Masthead.app")).arg(&app)).unwrap();
    std::fs::write(app.join("Contents/marker"), "old").unwrap();
    let zip = dir.join(ASSET);
    run(Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(root.join("dist/Masthead.app"))
        .arg(&zip))
    .unwrap();
    install(&format!("file://{}", zip.display()), &app).unwrap();
    assert!(app.join("Contents/MacOS/Masthead").exists());
    assert!(!app.join("Contents/marker").exists(), "old bundle still in place");
    assert!(!dir.join("Applications/.Masthead.app.old").exists());
    run(Command::new("codesign").args(["--verify", "--deep"]).arg(&app)).unwrap();
}
