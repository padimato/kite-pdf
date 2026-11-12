//! Historique des PDF ouverts et recherche « intelligente » utilisée par la barre centrale (`Ctrl+T`).
//!
//! L'historique est un simple fichier texte (`date\tnombre d'ouvertures\tchemin` par ligne) dans
//! `%APPDATA%\KitePDF\recent.tsv`, sans dépendance de sérialisation.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Nombre maximal de fichiers gardés dans l'historique.
const MAX_RECENT_FILES: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct RecentFile {
    pub path: PathBuf,
    /// Dernière ouverture, en secondes depuis l'époque Unix.
    pub last_opened: u64,
    pub open_count: u32,
    /// Page (index) et zoom à la dernière fermeture, pour reprendre la lecture au même endroit
    pub reading: Option<(usize, f32)>,
}

impl RecentFile {
    /// Nom du fichier tel qu'affiché (les `%E2%80%90` des fichiers téléchargés sont décodés).
    pub fn file_name(&self) -> String {
        let raw = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned());
        decode_percent(&raw)
    }

    /// Dossier parent, abrégé pour l'affichage.
    pub fn folder_label(&self) -> String {
        let parent = self
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(home) = std::env::var("USERPROFILE") {
            if let Some(rest) = parent.strip_prefix(&home) {
                return format!("~{rest}");
            }
        }
        parent
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecentFiles {
    pub items: Vec<RecentFile>,
    storage: Option<PathBuf>,
}

impl RecentFiles {
    /// Charge l'historique depuis le profil de l'utilisateur ; les fichiers disparus sont ignorés.
    pub fn load_default() -> Self {
        let storage = crate::settings::app_data_dir()
            .join("recent.tsv");
        let mut recent = Self::load_from(&storage);
        recent.items.retain(|f| f.path.is_file());
        recent
    }

    pub fn load_from(storage: &Path) -> Self {
        let items = std::fs::read_to_string(storage)
            .map(|text| text.lines().filter_map(parse_line).collect())
            .unwrap_or_default();
        Self { items, storage: Some(storage.to_path_buf()) }
    }

    /// Mémoire seule, sans fichier (tests, captures automatisées).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Note l'ouverture d'un fichier et enregistre l'historique.
    pub fn record_open(&mut self, path: &Path) {
        let path = std::fs::canonicalize(path)
            .map(strip_verbatim_prefix)
            .unwrap_or_else(|_| path.to_path_buf());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (previous_count, reading) = self
            .items
            .iter()
            .position(|f| same_path(&f.path, &path))
            .map(|i| {
                let previous = self.items.remove(i);
                (previous.open_count, previous.reading)
            })
            .unwrap_or((0, None));
        self.items.insert(
            0,
            RecentFile { path, last_opened: now, open_count: previous_count.saturating_add(1), reading },
        );
        self.items.truncate(MAX_RECENT_FILES);
        self.save();
    }

    /// Note l'endroit où l'utilisateur s'est arrêté dans un fichier de l'historique.
    pub fn remember_reading(&mut self, path: &Path, page: usize, zoom: f32) {
        let Some(item) = self.find_mut(path) else { return };
        if item.reading == Some((page, zoom)) {
            return;
        }
        item.reading = Some((page, zoom));
        self.save();
    }

    /// Page et zoom à la dernière fermeture de ce fichier.
    pub fn reading_for(&self, path: &Path) -> Option<(usize, f32)> {
        let canonical = canonical(path);
        self.items.iter().find(|f| same_path(&f.path, &canonical))?.reading
    }

    fn find_mut(&mut self, path: &Path) -> Option<&mut RecentFile> {
        let canonical = canonical(path);
        self.items.iter_mut().find(|f| same_path(&f.path, &canonical))
    }

    /// Efface tout l'historique (et les pages où chaque lecture s'était arrêtée).
    pub fn clear(&mut self) {
        self.items.clear();
        self.save();
    }

    pub fn remove(&mut self, path: &Path) {
        self.items.retain(|f| !same_path(&f.path, path));
        self.save();
    }

    fn save(&self) {
        let Some(storage) = &self.storage else { return };
        let mut text = String::new();
        for f in &self.items {
            let (page, zoom) = match f.reading {
                Some((page, zoom)) => (page.to_string(), format!("{zoom:.3}")),
                None => ("-".to_string(), "-".to_string()),
            };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                f.last_opened,
                f.open_count,
                page,
                zoom,
                f.path.display()
            ));
        }
        if let Some(dir) = storage.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(storage, text);
    }

    /// Fichiers correspondant à la requête, du plus pertinent au moins pertinent (avec leur score).
    /// Sans requête : les plus récents d'abord.
    pub fn search(&self, query: &str, now: u64) -> Vec<(f32, &RecentFile)> {
        let query = normalize(query);
        let mut found: Vec<(f32, &RecentFile)> = self
            .items
            .iter()
            .filter_map(|f| {
                let name = normalize(&f.file_name());
                let folder = normalize(&f.folder_label());
                let relevance = if query.is_empty() {
                    0.0
                } else {
                    match_score(&query, &name, &folder)?
                };
                Some((relevance + recency_bonus(f, now), f))
            })
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        found
    }
}

/// `date  ouvertures  page  zoom  chemin` (ou l'ancien format `date  ouvertures  chemin`).
fn parse_line(line: &str) -> Option<RecentFile> {
    let parts: Vec<&str> = line.split('\t').collect();
    let last_opened = parts.first()?.trim().parse().ok()?;
    let open_count = parts.get(1)?.trim().parse().ok()?;
    let (reading, path) = match parts.len() {
        3 => (None, parts[2]),
        5 => {
            let page = parts[2].trim().parse::<usize>().ok();
            let zoom = parts[3].trim().parse::<f32>().ok();
            (page.zip(zoom), parts[4])
        }
        _ => return None,
    };
    let path = path.trim();
    (!path.is_empty()).then(|| RecentFile { path: PathBuf::from(path), last_opened, open_count, reading })
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).map(strip_verbatim_prefix).unwrap_or_else(|_| path.to_path_buf())
}

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Bonus léger pour les fichiers récents et souvent ouverts : départage les résultats proches.
fn recency_bonus(f: &RecentFile, now: u64) -> f32 {
    let age_days = now.saturating_sub(f.last_opened) as f32 / 86_400.0;
    let recency = 1.0 / (1.0 + age_days / 7.0);
    let frequency = (f.open_count as f32).ln_1p() * 0.15;
    recency + frequency
}

/// Décode les séquences `%XX` d'un nom de fichier (UTF-8) ; laisse le texte intact s'il n'en est pas.
pub fn decode_percent(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Les traits d'union typographiques (U+2010, U+2011) manquent dans la police d'egui.
    String::from_utf8(out)
        .unwrap_or_else(|_| text.to_string())
        .replace(['\u{2010}', '\u{2011}'], "-")
}

/// Minuscules, sans accents, séparateurs (`_ - . %` etc.) remplacés par des espaces.
pub fn normalize(text: &str) -> String {
    let decoded = decode_percent(text);
    let mut out = String::with_capacity(decoded.len());
    for c in decoded.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            'ÿ' => 'y',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        };
        if c == ' ' && out.ends_with(' ') {
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// Pertinence d'un texte (déjà normalisé) pour une requête (déjà normalisée).
/// Chaque mot de la requête doit être trouvé, dans le texte principal ou dans le secondaire
/// (dossier, mots-clés). `None` si un mot manque.
pub fn match_score(query: &str, primary: &str, secondary: &str) -> Option<f32> {
    let mut total = 0.0;
    for token in query.split(' ').filter(|t| !t.is_empty()) {
        total += token_score(token, primary)
            .map(|s| s * 2.0)
            .or_else(|| token_score(token, secondary))?;
    }
    Some(total)
}

fn token_score(token: &str, text: &str) -> Option<f32> {
    if text.split(' ').any(|word| word == token) {
        return Some(4.0);
    }
    if text.split(' ').any(|word| word.starts_with(token)) {
        return Some(3.0);
    }
    if text.contains(token) {
        return Some(2.0);
    }
    // Lettres dans l'ordre et rapprochées, pour les abréviations (« thrmq » → thermiques), à partir
    // de 3 lettres. Sans la limite d'écart, un long nom de fichier contiendrait presque tout.
    if token.chars().count() >= 3 && compact_subsequence(token, text) {
        return Some(0.75);
    }
    None
}

/// Les lettres de `needle` apparaissent dans l'ordre dans un même mot de `haystack`, sur une
/// étendue d'au plus deux fois leur nombre.
fn compact_subsequence(needle: &str, haystack: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    let max_span = needle.len() * 2;
    haystack.split(' ').any(|word| {
        let word: Vec<char> = word.chars().collect();
        (0..word.len()).filter(|&s| word[s] == needle[0]).any(|start| {
            let mut n = 0;
            for (offset, c) in word[start..].iter().enumerate() {
                if offset >= max_span {
                    break;
                }
                if *c == needle[n] {
                    n += 1;
                    if n == needle.len() {
                        return true;
                    }
                }
            }
            false
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, age_days: u64, count: u32) -> RecentFile {
        RecentFile { path: PathBuf::from(path), last_opened: 1_000_000 - age_days * 86_400, open_count: count, reading: None }
    }

    #[test]
    fn test_recent_files_smart_search() {
        let recent = RecentFiles {
            items: vec![
                file(r"C:\Cours\Électronique\TP3_capteurs_thermiques.pdf", 1, 3),
                file(r"C:\Articles\Battery Energy - 2025 - Saidi - Advancements in Silicon Anodes.pdf", 10, 1),
                file(r"C:\Cours\Maths\Résumé_algèbre.pdf", 0, 1),
            ],
            storage: None,
        };
        let now = 1_000_000;
        let names = |q: &str| -> Vec<String> {
            recent.search(q, now).iter().map(|(_, f)| f.file_name()).collect()
        };

        // Sans requête : le plus récent d'abord.
        assert!(names("")[0].starts_with("Résumé"));
        // Accents et casse ignorés, plusieurs mots dans le désordre.
        assert_eq!(names("resume ALGEBRE").len(), 1);
        assert_eq!(names("silicon battery").len(), 1);
        // Le dossier compte aussi : « electronique » trouve le TP.
        assert!(names("electronique")[0].starts_with("TP3"));
        // Abréviation : lettres dans l'ordre.
        assert!(names("thrmq")[0].starts_with("TP3"));
        // Un mot introuvable exclut le fichier.
        assert!(names("silicon zzz").is_empty());
    }

    #[test]
    fn test_recent_files_persist_and_reorder() {
        let dir = std::env::temp_dir().join("kite_recent_test");
        let _ = std::fs::remove_dir_all(&dir);
        let storage = dir.join("recent.tsv");
        let a = dir.join("a.pdf");
        let b = dir.join("b.pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&a, b"%PDF").unwrap();
        std::fs::write(&b, b"%PDF").unwrap();

        let mut recent = RecentFiles::load_from(&storage);
        recent.record_open(&a);
        recent.record_open(&b);
        recent.record_open(&a);

        recent.remember_reading(&b, 41, 1.5);
        let reloaded = RecentFiles::load_from(&storage);
        assert_eq!(reloaded.items.len(), 2);
        assert_eq!(reloaded.items[0].file_name(), "a.pdf");
        assert_eq!(reloaded.items[0].open_count, 2);
        // L'endroit où l'on s'est arrêté survit au rechargement et à une réouverture.
        assert_eq!(reloaded.reading_for(&b), Some((41, 1.5)));
        let mut reopened = reloaded.clone();
        reopened.record_open(&b);
        assert_eq!(reopened.reading_for(&b), Some((41, 1.5)));
        // L'ancien format (sans page ni zoom) reste lisible.
        assert_eq!(parse_line("100\t3\tC:\\x.pdf").map(|f| f.open_count), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
