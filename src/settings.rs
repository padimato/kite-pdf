//! Réglages gardés d'un lancement à l'autre : position et taille de la fenêtre, barre latérale.
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
        };
        assert_eq!(Settings::parse(&settings.render()), settings);
        assert!(settings.window_is_plausible());
        // Fenêtre restée sur un écran débranché : ignorée.
        let lost = Settings { window: Some([-5000.0, 80.0, 1280.0, 820.0]), ..Default::default() };
        assert!(!lost.window_is_plausible());
    }
}
