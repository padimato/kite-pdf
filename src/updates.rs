//! Nouvelles versions : au plus une fois par jour, l'application consulte la dernière version publiée
//! sur GitHub (ou à la demande, depuis les Réglages). Rien n'est téléchargé ni installé : un bandeau
//! propose seulement d'ouvrir sa page.

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

/// Vérification demandée depuis les Réglages (« Rechercher une mise à jour »).
#[derive(Debug, Clone, PartialEq)]
pub enum ManualCheck {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
}

static MANUAL: Mutex<ManualCheck> = Mutex::new(ManualCheck::Idle);

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

/// Dernière version publiée, ou une phrase qui explique pourquoi on ne la connaît pas.
fn fetch_latest() -> Result<LatestRelease, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("kite-pdf/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent
        .get(LATEST_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => "Aucune version n'est encore publiée.".to_string(),
            ureq::Error::Status(code, _) => format!("GitHub n'a pas répondu correctement (code {code}). Réessayez plus tard."),
            ureq::Error::Transport(_) => "Impossible de joindre GitHub : vérifiez la connexion à Internet.".to_string(),
        })?;
    let body = response.into_string().map_err(|_| "La réponse de GitHub est incomplète.".to_string())?;
    serde_json::from_str(&body).map_err(|_| "La réponse de GitHub est illisible.".to_string())
}

/// Version publiée plus récente que celle-ci, s'il y en a une.
fn newer_release(latest: LatestRelease) -> Option<Release> {
    if latest.draft || latest.prerelease {
        return None;
    }
    let version = latest.tag_name.trim_start_matches(['v', 'V']).to_string();
    is_newer(&version, env!("CARGO_PKG_VERSION")).then_some(Release { version, page_url: latest.html_url })
}

/// « Rechercher une mise à jour » : vérifie tout de suite, même si une version a été ignorée.
pub fn check_now(ctx: egui::Context) {
    if let Ok(mut manual) = MANUAL.lock() {
        if *manual == ManualCheck::Checking {
            return;
        }
        *manual = ManualCheck::Checking;
    }
    let _ = std::thread::Builder::new().name("kite-update-check-now".into()).spawn(move || {
        let outcome = match fetch_latest() {
            Ok(latest) => {
                let mut state = CheckState::load();
                state.last_check = now_secs();
                state.save();
                match newer_release(latest) {
                    Some(release) => {
                        if let Ok(mut available) = AVAILABLE.lock() {
                            *available = Some(release.clone());
                        }
                        ManualCheck::Available(release)
                    }
                    None => ManualCheck::UpToDate,
                }
            }
            Err(reason) => ManualCheck::Failed(reason),
        };
        if let Ok(mut manual) = MANUAL.lock() {
            *manual = outcome;
        }
        ctx.request_repaint();
    });
}

/// Résultat de la dernière vérification demandée depuis les Réglages.
pub fn manual_status() -> ManualCheck {
    MANUAL.lock().map(|m| m.clone()).unwrap_or(ManualCheck::Idle)
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
        let Some(release) = latest.ok().and_then(newer_release) else { return };
        if state.skipped_version.as_deref() != Some(release.version.as_str()) {
            if let Ok(mut available) = AVAILABLE.lock() {
                *available = Some(release);
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
    fn test_newer_release_ignores_drafts_and_older() {
        let release = |tag: &str, draft: bool, prerelease: bool| LatestRelease {
            tag_name: tag.to_string(),
            html_url: format!("https://example.invalid/{tag}"),
            draft,
            prerelease,
        };
        let newer = newer_release(release("v99.0.0", false, false)).expect("version plus récente");
        assert_eq!(newer.version, "99.0.0");
        assert_eq!(newer_release(release(concat!("v", env!("CARGO_PKG_VERSION")), false, false)), None);
        assert_eq!(newer_release(release("v99.0.0", true, false)), None);
        assert_eq!(newer_release(release("v99.0.0", false, true)), None);
    }

    #[test]
    fn test_check_state_roundtrip() {
        let state = CheckState::parse("1731300000\n1.2.0\n");
        assert_eq!(state, CheckState { last_check: 1_731_300_000, skipped_version: Some("1.2.0".into()) });
        assert_eq!(CheckState::parse(""), CheckState::default());
    }
}
