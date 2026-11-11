//! Nouvelles versions : au plus une fois par jour, l'application consulte la dernière version publiée
//! sur GitHub. Rien n'est téléchargé ni installé : un bandeau propose seulement d'ouvrir sa page.

use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/padimato/kite-pdf/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Version plus récente que celle-ci, publiée sur GitHub.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub page_url: String,
}

static AVAILABLE: Mutex<Option<Release>> = Mutex::new(None);

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// `update_check.txt` : date de la dernière vérification (secondes Unix) puis version ignorée.
#[derive(Debug, Default, PartialEq)]
struct CheckState {
    last_check: u64,
    skipped_version: Option<String>,
}

fn state_path() -> std::path::PathBuf {
    crate::settings::app_data_dir().join("update_check.txt")
}

impl CheckState {
    fn load() -> Self {
        Self::parse(&std::fs::read_to_string(state_path()).unwrap_or_default())
    }

    fn parse(text: &str) -> Self {
        let mut lines = text.lines();
        Self {
            last_check: lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0),
            skipped_version: lines.next().map(str::trim).filter(|v| !v.is_empty()).map(String::from),
        }
    }

    fn save(&self) {
        let path = state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let skipped = self.skipped_version.as_deref().unwrap_or("");
        let _ = std::fs::write(path, format!("{}\n{skipped}\n", self.last_check));
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// « v1.2.3 » ou « 1.2 » → (1, 2, 3) / (1, 2, 0).
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(c), Some(v)) if c > v)
}

fn fetch_latest() -> Option<LatestRelease> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("kite-pdf/", env!("CARGO_PKG_VERSION")))
        .build();
    let body = agent
        .get(LATEST_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// Lance la vérification en tâche de fond si la dernière date de plus d'un jour.
pub fn check_in_background(ctx: egui::Context) {
    let mut state = CheckState::load();
    if now_secs().saturating_sub(state.last_check) < CHECK_INTERVAL.as_secs() {
        return;
    }
    let _ = std::thread::Builder::new().name("kite-update-check".into()).spawn(move || {
        let latest = fetch_latest();
        state.last_check = now_secs();
        state.save();
        let Some(latest) = latest.filter(|r| !r.draft && !r.prerelease) else { return };
        let version = latest.tag_name.trim_start_matches(['v', 'V']).to_string();
        let skipped = state.skipped_version.as_deref() == Some(version.as_str());
        if is_newer(&version, env!("CARGO_PKG_VERSION")) && !skipped {
            if let Ok(mut available) = AVAILABLE.lock() {
                *available = Some(Release { version, page_url: latest.html_url });
            }
            ctx.request_repaint();
        }
    });
}

/// Nouvelle version trouvée par la dernière vérification.
pub fn available() -> Option<Release> {
    AVAILABLE.lock().ok().and_then(|a| a.clone())
}

/// « Ignorer cette version » : plus de bandeau tant qu'une version plus récente ne sort pas.
pub fn skip(version: &str) {
    let mut state = CheckState::load();
    state.skipped_version = Some(version.to_string());
    state.save();
    if let Ok(mut available) = AVAILABLE.lock() {
        *available = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_comparison() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.4"), Some((1, 4, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn test_check_state_roundtrip() {
        let state = CheckState::parse("1731300000\n1.2.0\n");
        assert_eq!(state, CheckState { last_check: 1_731_300_000, skipped_version: Some("1.2.0".into()) });
        assert_eq!(CheckState::parse(""), CheckState::default());
    }
}
