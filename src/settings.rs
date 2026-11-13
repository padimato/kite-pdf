//! Réglages gardés d'un lancement à l'autre : fenêtre, barre latérale et préférences de la page Réglages.
//! Fichier texte `clé=valeur` dans `%APPDATA%\KitePDF\settings.txt`.

use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Position (coin haut-gauche) et taille intérieure de la fenêtre, en points
    pub window: Option<[f32; 4]>,
    pub maximized: bool,
    pub sidebar_pinned: Option<bool>,
    pub sidebar_width: Option<f32>,
    pub drawer_height: Option<f32>,
    /// Écran d'accueil déjà montré (premier lancement)
    pub welcome_seen: bool,
    /// Choix de la page Réglages
    pub prefs: Preferences,
}

/// Lecture du texte des images (OCR) pendant la lecture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageOcr {
    /// En arrière-plan, page après page
    #[default]
    Auto,
    /// Seulement pendant une recherche (`Ctrl+F`)
    OnSearch,
    Never,
}

/// Zoom d'un PDF ouvert pour la première fois.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenZoom {
    #[default]
    Actual,
    FitWidth,
    FitPage,
}

/// Place des commandes : barre latérale, ou barre en haut (écran portrait).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Layout {
    /// Barre en haut quand la fenêtre est plus haute que large, barre latérale sinon
    #[default]
    Auto,
    Sidebar,
    TopBar,
}

impl Layout {
    /// Barre en haut pour une fenêtre de cette taille. `current` (disposition affichée) évite les
    /// bascules répétées quand la fenêtre est presque carrée.
    pub fn top_bar(self, size: [f32; 2], current: bool) -> bool {
        let [width, height] = size;
        match self {
            Layout::Sidebar => false,
            Layout::TopBar => true,
            Layout::Auto if current => height > width * 0.95,
            Layout::Auto => height > width * 1.05,
        }
    }
}

/// Préférences modifiables dans la page Réglages.
#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    pub image_ocr: ImageOcr,
    pub open_zoom: OpenZoom,
    pub layout: Layout,
    /// Rouvrir un PDF à la page et au zoom où on l'avait laissé
    pub resume_reading: bool,
    /// Garder la liste des PDF ouverts récemment
    pub keep_history: bool,
    /// Consulter GitHub une fois par jour pour signaler une nouvelle version
    pub check_updates: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            image_ocr: ImageOcr::Auto,
            open_zoom: OpenZoom::Actual,
            layout: Layout::Auto,
            resume_reading: true,
            keep_history: true,
            check_updates: true,
        }
    }
}

/// Variable d'environnement : dossier des réglages et de l'historique à la place de
/// `%APPDATA%\KitePDF` (essais automatisés, installation portable).
pub const DATA_DIR_ENV: &str = "KITE_DATA_DIR";

/// Nom des dossiers de l'application dans `%APPDATA%` et `%LOCALAPPDATA%`.
pub const APP_DIR_NAME: &str = "KitePDF";
/// Nom des dossiers avant que l'application s'appelle Kite PDF.
const LEGACY_APP_DIR_NAME: &str = "SuperLecteurPDF";

fn known_folder(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Dossier des réglages, de l'historique et des préférences de l'utilisateur.
pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    known_folder("APPDATA").join(APP_DIR_NAME)
}

/// Dossier local (non synchronisé) : composants téléchargés.
pub fn local_data_dir() -> PathBuf {
    known_folder("LOCALAPPDATA").join(APP_DIR_NAME)
}

/// Reprend les dossiers de l'ancien nom (« Super Lecteur PDF ») : historique, réglages et composants
/// déjà téléchargés restent ainsi disponibles après le changement de nom.
pub fn migrate_legacy_dirs() {
    for var in ["APPDATA", "LOCALAPPDATA"] {
        let base = known_folder(var);
        let (old, new) = (base.join(LEGACY_APP_DIR_NAME), base.join(APP_DIR_NAME));
        if old.is_dir() && !new.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }
}

/// Les captures automatisées n'écrivent pas dans les réglages réels, sauf dossier de réglages dédié.
pub fn persistence_enabled(automated: bool) -> bool {
    !automated || std::env::var_os(DATA_DIR_ENV).is_some()
}

fn storage() -> PathBuf {
    app_data_dir().join("settings.txt")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(storage()).map(|t| Self::parse(&t)).unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key.trim() {
                "window" => {
                    let numbers: Vec<f32> = value.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                    if let [x, y, w, h] = numbers[..] {
                        settings.window = Some([x, y, w, h]);
                    }
                }
                "maximized" => settings.maximized = value.trim() == "1",
                "sidebar_pinned" => settings.sidebar_pinned = Some(value.trim() == "1"),
                "sidebar_width" => settings.sidebar_width = value.trim().parse().ok(),
                "drawer_height" => settings.drawer_height = value.trim().parse().ok(),
                "welcome_seen" => settings.welcome_seen = value.trim() == "1",
                "image_ocr" => {
                    settings.prefs.image_ocr = match value.trim() {
                        "search" => ImageOcr::OnSearch,
                        "never" => ImageOcr::Never,
                        _ => ImageOcr::Auto,
                    }
                }
                "open_zoom" => {
                    settings.prefs.open_zoom = match value.trim() {
                        "width" => OpenZoom::FitWidth,
                        "page" => OpenZoom::FitPage,
                        _ => OpenZoom::Actual,
                    }
                }
                "layout" => {
                    settings.prefs.layout = match value.trim() {
                        "sidebar" => Layout::Sidebar,
                        "top" => Layout::TopBar,
                        _ => Layout::Auto,
                    }
                }
                "resume_reading" => settings.prefs.resume_reading = value.trim() != "0",
                "keep_history" => settings.prefs.keep_history = value.trim() != "0",
                "check_updates" => settings.prefs.check_updates = value.trim() != "0",
                _ => {}
            }
        }
        settings
    }

    fn render(&self) -> String {
        let mut text = String::new();
        if let Some([x, y, w, h]) = self.window {
            text.push_str(&format!("window={x:.0},{y:.0},{w:.0},{h:.0}\n"));
        }
        text.push_str(&format!("maximized={}\n", u8::from(self.maximized)));
        if let Some(pinned) = self.sidebar_pinned {
            text.push_str(&format!("sidebar_pinned={}\n", u8::from(pinned)));
        }
        if let Some(width) = self.sidebar_width {
            text.push_str(&format!("sidebar_width={width:.0}\n"));
        }
        if let Some(height) = self.drawer_height {
            text.push_str(&format!("drawer_height={height:.0}\n"));
        }
        if self.welcome_seen {
            text.push_str("welcome_seen=1\n");
        }
        let prefs = &self.prefs;
        let image_ocr = match prefs.image_ocr {
            ImageOcr::Auto => "auto",
            ImageOcr::OnSearch => "search",
            ImageOcr::Never => "never",
        };
        let open_zoom = match prefs.open_zoom {
            OpenZoom::Actual => "100",
            OpenZoom::FitWidth => "width",
            OpenZoom::FitPage => "page",
        };
        let layout = match prefs.layout {
            Layout::Auto => "auto",
            Layout::Sidebar => "sidebar",
            Layout::TopBar => "top",
        };
        text.push_str(&format!("image_ocr={image_ocr}\nopen_zoom={open_zoom}\nlayout={layout}\n"));
        for (key, value) in [
            ("resume_reading", prefs.resume_reading),
            ("keep_history", prefs.keep_history),
            ("check_updates", prefs.check_updates),
        ] {
            text.push_str(&format!("{key}={}\n", u8::from(value)));
        }
        text
    }

    pub fn save(&self) {
        let path = storage();
        // Rien à écrire si le fichier dit déjà la même chose (enregistrement régulier).
        if std::fs::read_to_string(&path).is_ok_and(|current| current == self.render()) {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.render());
    }

    /// Fenêtre visible sur au moins un écran (un écran débranché ne doit pas la cacher).
    pub fn window_is_plausible(&self) -> bool {
        self.window.is_some_and(|[x, y, w, h]| {
            w >= 400.0 && h >= 300.0 && x > -w + 80.0 && y > -40.0 && x < 16_000.0 && y < 16_000.0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_roundtrip() {
        let settings = Settings {
            window: Some([120.0, 80.0, 1280.0, 820.0]),
            maximized: true,
            sidebar_pinned: Some(false),
            sidebar_width: Some(320.0),
            drawer_height: Some(180.0),
            welcome_seen: true,
            prefs: Preferences {
                image_ocr: ImageOcr::OnSearch,
                open_zoom: OpenZoom::FitWidth,
                layout: Layout::TopBar,
                resume_reading: false,
                keep_history: true,
                check_updates: false,
            },
        };
        assert_eq!(Settings::parse(&settings.render()), settings);
        assert!(settings.window_is_plausible());
        // Ancien fichier sans préférences : valeurs par défaut (OCR automatique, reprise, historique).
        assert_eq!(Settings::parse("maximized=1\n").prefs, Preferences::default());
        // Fenêtre restée sur un écran débranché : ignorée.
        let lost = Settings { window: Some([-5000.0, 80.0, 1280.0, 820.0]), ..Default::default() };
        assert!(!lost.window_is_plausible());
    }

    #[test]
    fn test_layout_follows_window_shape() {
        // Écran portrait : barre en haut ; paysage : barre latérale.
        assert!(Layout::Auto.top_bar([1080.0, 1870.0], false));
        assert!(!Layout::Auto.top_bar([1920.0, 1040.0], true));
        // Fenêtre presque carrée : la disposition affichée est gardée.
        assert!(Layout::Auto.top_bar([1000.0, 1000.0], true));
        assert!(!Layout::Auto.top_bar([1000.0, 1000.0], false));
        // Choix imposé dans les Réglages.
        assert!(Layout::TopBar.top_bar([1920.0, 1040.0], false));
        assert!(!Layout::Sidebar.top_bar([1080.0, 1870.0], true));
    }
}
