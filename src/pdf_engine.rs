use anyhow::{Context, Result};
use egui::ColorImage;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static PDFIUM_SINGLETON: OnceLock<PdfEngine> = OnceLock::new();
static PDFIUM_GUARD: Mutex<()> = Mutex::new(());

/// Dimensions d'une page PDF en points (1 pt = 1/72 pouce)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageDimensions {
    pub width_pt: f32,
    pub height_pt: f32,
}

impl PageDimensions {
    #[allow(dead_code)]
    pub fn aspect_ratio(&self) -> f32 {
        if self.height_pt > 0.0 {
            self.width_pt / self.height_pt
        } else {
            1.0
        }
    }
}

/// Métadonnées et accès au document PDF via PDFium
pub struct PdfEngine {
    pdfium: Pdfium,
}

// PDFium est protégé par PDFIUM_GUARD pour la sécurité inter-threads dans les tests parallèles
unsafe impl Send for PdfEngine {}
unsafe impl Sync for PdfEngine {}

pub struct LoadedPdfDocument<'a> {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub title: String,
    pub page_dimensions: Vec<PageDimensions>,
    doc: PdfDocument<'a>,
}

impl PdfEngine {
    pub fn lock_guard() -> std::sync::MutexGuard<'static, ()> {
        PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Retourne l'instance singleton globale de `PdfEngine` (garantit que `FPDF_InitLibrary` n'est appelé qu'une seule fois
    /// par processus et que `FPDF_DestroyLibrary` n'est jamais appelé prématurément pendant un test parallèle).
    pub fn instance() -> &'static PdfEngine {
        PDFIUM_SINGLETON
            .get_or_init(|| Self::new_internal().expect("Impossible d'initialiser pdfium.dll"))
    }

    fn new_internal() -> Result<Self> {
        let bindings = Self::bind_pdfium()?;
        Ok(Self {
            pdfium: Pdfium::new(bindings),
        })
    }

    fn bind_pdfium() -> Result<Box<dyn PdfiumLibraryBindings>> {
        // 1. Dossier courant
        if let Ok(b) = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./")) {
            return Ok(b);
        }
        // 2. Dossier de l'exécutable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                if let Ok(b) = Pdfium::bind_to_library(
                    Pdfium::pdfium_platform_library_name_at_path(exe_dir),
                ) {
                    return Ok(b);
                }
            }
        }
        // 3. Bibliothèque système
        Pdfium::bind_to_system_library().context(
            "Impossible de charger pdfium.dll. Assurez-vous que pdfium.dll est présent à la racine du projet.",
        )
    }

    /// Charge un document PDF depuis le disque et extrait uniquement les métadonnées légères des pages.
    pub fn load_document<'a>(&'a self, path: &Path) -> Result<LoadedPdfDocument<'a>> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let doc = self
            .pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("Échec d'ouverture du fichier PDF : {}", path.display()))?;

        let pages = doc.pages();
        let page_count = pages.len();
        let mut page_dimensions = Vec::with_capacity(page_count as usize);

        for idx in 0..page_count {
            let page = pages
                .get(idx)
                .with_context(|| format!("Impossible de lire la page {}", idx + 1))?;
            page_dimensions.push(PageDimensions {
                width_pt: page.width().value,
                height_pt: page.height().value,
            });
        }

        let title = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Document.pdf")
            .to_string();

        Ok(LoadedPdfDocument {
            path: path.to_path_buf(),
            title,
            page_dimensions,
            doc,
        })
    }
}

impl<'a> LoadedPdfDocument<'a> {
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.page_dimensions.len()
    }

    /// Effectue le rendu matriciel haute qualité d'une page donnée à la taille cible en pixels.
    pub fn render_page(&self, page_index: usize, target_width_px: i32, target_height_px: i32) -> Result<ColorImage> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let page = self
            .doc
            .pages()
            .get(page_index as u16)
            .with_context(|| format!("Page {} hors limites", page_index))?;

        let width_px = target_width_px.clamp(64, 4096);
        let height_px = target_height_px.clamp(64, 4096);

        let config = PdfRenderConfig::new()
            .set_target_width(width_px)
            .set_target_height(height_px)
            .render_form_data(true)
            .render_annotations(true)
            .use_lcd_text_rendering(true);

        let bitmap = page
            .render_with_config(&config)
            .with_context(|| format!("Erreur de rendu PDFium sur la page {}", page_index + 1))?;

        let rgba = bitmap.as_rgba_bytes();
        let actual_w = bitmap.width() as usize;
        let actual_h = bitmap.height() as usize;

        Ok(ColorImage::from_rgba_unmultiplied([actual_w, actual_h], &rgba))
    }
}

/// Génère un fichier PDF de démonstration personnalisé (titre, sous-titre, palette de couleur, nombre de pages).
pub fn ensure_custom_sample_pdf(
    path: &Path,
    doc_title: &str,
    accent_rgb: (f32, f32, f32),
    total_pages: usize,
) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();

    let font_helvetica = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_bold = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_courier = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier-Bold",
        "Encoding" => "WinAnsiEncoding",
    });

    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_helvetica,
            "F2" => font_bold,
            "F3" => font_courier,
        },
    });

    let mut page_ids = Vec::with_capacity(total_pages);

    for page_num in 1..=total_pages {
        let mut ops: Vec<Operation> = Vec::new();

        // Bandeau supérieur coloré dans la page PDF
        ops.push(Operation::new("q", vec![]));
        ops.push(Operation::new(
            "rg",
            vec![accent_rgb.0.into(), accent_rgb.1.into(), accent_rgb.2.into()],
        ));
        ops.push(Operation::new(
            "re",
            vec![36.into(), 730.into(), 523.into(), 76.into()],
        ));
        ops.push(Operation::new("f", vec![]));
        ops.push(Operation::new("Q", vec![]));

        // Titre dans le bandeau
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("rg", vec![1.0.into(), 1.0.into(), 1.0.into()]));
        ops.push(Operation::new("Tf", vec!["F2".into(), 18.into()]));
        ops.push(Operation::new("Td", vec![52.into(), 774.into()]));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal(format!(
                "{} - PAGE {}/{}",
                doc_title, page_num, total_pages
            ))],
        ));
        ops.push(Operation::new("ET", vec![]));

        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("rg", vec![0.88.into(), 0.94.into(), 1.0.into()]));
        ops.push(Operation::new("Tf", vec!["F1".into(), 11.into()]));
        ops.push(Operation::new("Td", vec![52.into(), 748.into()]));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal(
                "Moteur Rust + PDFium + Virtualisation RAM 15+ PDF + Trackpad Windows Precision",
            )],
        ));
        ops.push(Operation::new("ET", vec![]));

        // Corps de texte explicatif
        let lines = [
            format!("Document : {} (Section {}.1)", doc_title, page_num),
            "Architecture Super Lecteur PDF - Haute Performance & Econome en RAM :".to_string(),
            "  * Barre laterale gauche epinglable (Ctrl+B) ou auto-masquee au survol du bord gauche.".to_string(),
            "  * Zero barre superieure : 100% de la hauteur d'ecran dediee au document PDF.".to_string(),
            "  * Cache de textures GPU LRU plafonne (48 Mo) + Hibernation des onglets inactifs (0 Mo).".to_string(),
            "  * Navigation Trackpad 2D (smooth_scroll_delta) et Pinch-to-Zoom centre sur le pointeur.".to_string(),
            "  * Ouverture simultanee de 15+ fichiers PDF sans saturation de la memoire vive.".to_string(),
        ];

        for (i, line) in lines.iter().enumerate() {
            ops.push(Operation::new("BT", vec![]));
            if i == 0 {
                ops.push(Operation::new(
                    "rg",
                    vec![accent_rgb.0.into(), accent_rgb.1.into(), accent_rgb.2.into()],
                ));
                ops.push(Operation::new("Tf", vec!["F2".into(), 14.into()]));
            } else {
                ops.push(Operation::new("rg", vec![0.15.into(), 0.18.into(), 0.22.into()]));
                ops.push(Operation::new("Tf", vec!["F1".into(), 11.into()]));
            }
            let y = 695 - (i as i32 * 22);
            ops.push(Operation::new("Td", vec![52.into(), y.into()]));
            ops.push(Operation::new("Tj", vec![Object::string_literal(line.as_str())]));
            ops.push(Operation::new("ET", vec![]));
        }

        // Cadre de graphique technique simulé (axes + courbes + textes inclinés en diagonale)
        ops.push(Operation::new("q", vec![]));
        ops.push(Operation::new("rg", vec![0.96.into(), 0.97.into(), 0.99.into()]));
        ops.push(Operation::new("RG", vec![0.70.into(), 0.76.into(), 0.85.into()]));
        ops.push(Operation::new("w", vec![1.5.into()]));
        ops.push(Operation::new(
            "re",
            vec![52.into(), 250.into(), 490.into(), 260.into()],
        ));
        ops.push(Operation::new("B", vec![]));

        // Grille interne du graphique
        ops.push(Operation::new("w", vec![0.5.into()]));
        ops.push(Operation::new("RG", vec![0.85.into(), 0.88.into(), 0.93.into()]));
        for gx in (100..520).step_by(60) {
            ops.push(Operation::new("m", vec![gx.into(), 280.into()]));
            ops.push(Operation::new("l", vec![gx.into(), 480.into()]));
            ops.push(Operation::new("S", vec![]));
        }
        for gy in (280..=480).step_by(40) {
            ops.push(Operation::new("m", vec![90.into(), gy.into()]));
            ops.push(Operation::new("l", vec![520.into(), gy.into()]));
            ops.push(Operation::new("S", vec![]));
        }

        // Courbe de performance en couleur
        ops.push(Operation::new("w", vec![2.5.into()]));
        ops.push(Operation::new(
            "RG",
            vec![accent_rgb.0.into(), accent_rgb.1.into(), accent_rgb.2.into()],
        ));
        ops.push(Operation::new("m", vec![90.into(), 300.into()]));
        ops.push(Operation::new("l", vec![170.into(), 345.into()]));
        ops.push(Operation::new("l", vec![250.into(), 330.into()]));
        ops.push(Operation::new("l", vec![330.into(), 410.into()]));
        ops.push(Operation::new("l", vec![410.into(), 440.into()]));
        ops.push(Operation::new("l", vec![500.into(), 470.into()]));
        ops.push(Operation::new("S", vec![]));
        ops.push(Operation::new("Q", vec![]));

        // Texte orienté en diagonale (30 degrés : cos=0.866, sin=0.5)
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("rg", vec![0.80.into(), 0.22.into(), 0.18.into()]));
        ops.push(Operation::new("Tf", vec!["F2".into(), 13.into()]));
        ops.push(Operation::new(
            "Tm",
            vec![
                0.866.into(),
                0.500.into(),
                (-0.500).into(),
                0.866.into(),
                180.into(),
                340.into(),
            ],
        ));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal(
                "Axe Diagonal +30 deg : Debit Rendu GPU (FPS)",
            )],
        ));
        ops.push(Operation::new("ET", vec![]));

        // Texte vertical (90 degrés : cos=0, sin=1) pour l'axe Y
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("rg", vec![0.20.into(), 0.30.into(), 0.50.into()]));
        ops.push(Operation::new("Tf", vec!["F2".into(), 11.into()]));
        ops.push(Operation::new(
            "Tm",
            vec![
                0.0.into(),
                1.0.into(),
                (-1.0).into(),
                0.0.into(),
                75.into(),
                310.into(),
            ],
        ));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal("Consommation RAM (Mo)")],
        ));
        ops.push(Operation::new("ET", vec![]));

        // Encadré récapitulatif en bas de page
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("rg", vec![0.25.into(), 0.28.into(), 0.35.into()]));
        ops.push(Operation::new("Tf", vec!["F3".into(), 10.into()]));
        ops.push(Operation::new("Td", vec![52.into(), 210.into()]));
        ops.push(Operation::new(
            "Tj",
            vec![Object::string_literal(format!(
                "[{} - PAGE {}] Format A4 (595x842 pt) | Trackpad 2D Scroll & Virtualisation LRU",
                doc_title, page_num
            ))],
        ));
        ops.push(Operation::new("ET", vec![]));

        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => resources_id,
            "Contents" => content_id,
        });
        page_ids.push(page_id.into());
    }

    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => page_ids,
        "Count" => total_pages as i32,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();
    doc.save(path)?;
    Ok(())
}

/// Génère le fichier PDF de démonstration par défaut.
pub fn ensure_sample_pdf(path: &Path) -> Result<()> {
    ensure_custom_sample_pdf(path, "SUPER LECTEUR PDF", (0.10, 0.24, 0.48), 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pdfium_engine_loads_and_renders_sample_pdf() {
        let sample_path = PathBuf::from("test_pdfs/unit_test_sample.pdf");
        ensure_sample_pdf(&sample_path).expect("Doit créer le PDF de test");

        let engine = PdfEngine::instance();
        let doc = engine
            .load_document(&sample_path)
            .expect("Doit charger le PDF de test");

        assert_eq!(doc.page_count(), 5);
        assert!((doc.page_dimensions[0].width_pt - 595.0).abs() < 1.0);
        assert!((doc.page_dimensions[0].height_pt - 842.0).abs() < 1.0);

        let img = doc
            .render_page(0, 800, 1132)
            .expect("Doit rendre la page 1 en RGBA");
        assert_eq!(img.width(), 800);
        assert_eq!(img.height(), 1132);
    }
}

