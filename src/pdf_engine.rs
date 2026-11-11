use crate::ocr_engine::{parse_ocr_scanned_pages, OCR_DEEP_PAGES_INFO_KEY, OCR_SCANNED_PAGES_INFO_KEY};
use crate::text_search::{PageTextLayer, PdfTopDownRect, PositionedChar};
use anyhow::{Context, Result};
use egui::ColorImage;
use image::RgbImage;
#[cfg(test)]
use lopdf::content::{Content, Operation};
#[cfg(test)]
use lopdf::{dictionary, Document, Object, Stream};
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static PDFIUM_SINGLETON: OnceLock<PdfEngine> = OnceLock::new();

/// Au-delà, l'ouverture ne charge plus chaque page : charger une page coûte environ 25 ms
/// (analyse de son contenu), soit plus de 10 s pour 460 pages. Les détails des pages (annotations,
/// rotation, champs de formulaire) sont alors lus en tâche de fond.
pub const FULL_SCAN_MAX_PAGES: usize = 40;

/// Ce que l'ouverture apprend d'une page en la chargeant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageDetails {
    pub dims: PageDimensions,
    pub has_app_drawn_annotations: bool,
    pub has_form_widgets: bool,
}
static PDFIUM_GUARD: Mutex<()> = Mutex::new(());

/// Dimensions d'une page PDF en points (1 pt = 1/72 pouce)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageDimensions {
    /// Largeur et hauteur telles qu'affichées (rotation de la page comprise)
    pub width_pt: f32,
    pub height_pt: f32,
    /// Coin bas-gauche de la zone visible (`/CropBox`) dans l'espace utilisateur du PDF
    pub box_left: f32,
    pub box_bottom: f32,
    /// `/Rotate` de la page : 0, 90, 180 ou 270
    pub rotation_deg: u32,
}

impl PageDimensions {
    /// Page sans rotation dont la zone visible commence à l'origine.
    pub fn simple(width_pt: f32, height_pt: f32) -> Self {
        Self {
            width_pt,
            height_pt,
            box_left: 0.0,
            box_bottom: 0.0,
            rotation_deg: 0,
        }
    }

    /// Taille de la zone visible avant application de `/Rotate`.
    pub fn unrotated_size(&self) -> (f32, f32) {
        if self.rotation_deg == 90 || self.rotation_deg == 270 {
            (self.height_pt, self.width_pt)
        } else {
            (self.width_pt, self.height_pt)
        }
    }

    /// Convertit un point de l'espace utilisateur PDF vers le repère affiché (origine en haut à gauche).
    pub fn pdf_to_view(&self, x: f32, y: f32) -> (f32, f32) {
        let (box_w, box_h) = self.unrotated_size();
        let (ux, uy) = (x - self.box_left, y - self.box_bottom);
        match self.rotation_deg {
            90 => (uy, ux),
            180 => (box_w - ux, uy),
            270 => (box_h - uy, box_w - ux),
            _ => (ux, box_h - uy),
        }
    }

    /// Opération inverse de [`Self::pdf_to_view`].
    pub fn view_to_pdf(&self, x: f32, y: f32) -> (f32, f32) {
        let (box_w, box_h) = self.unrotated_size();
        let (ux, uy) = match self.rotation_deg {
            90 => (y, x),
            180 => (box_w - x, y),
            270 => (box_w - y, box_h - x),
            _ => (x, box_h - y),
        };
        (ux + self.box_left, uy + self.box_bottom)
    }

    #[allow(dead_code)]
    pub fn aspect_ratio(&self) -> f32 {
        if self.height_pt > 0.0 {
            self.width_pt / self.height_pt
        } else {
            1.0
        }
    }
}

/// Portion d'une page à rendre, en pixels à l'échelle demandée (origine en haut à gauche de la page).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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
    /// Le document contient de vrais champs de formulaire (un `/AcroForm` vide ne compte pas)
    has_form_widgets: bool,
    /// Au moins une annotation que l'application reprend à son compte
    pub has_app_drawn_annotations: bool,
    /// Rotation, zone visible, annotations et champs de chaque page connus. Sinon (gros document),
    /// seules les tailles sont exactes et le reste est à lire avec [`LoadedPdfDocument::page_details`].
    pub page_details_complete: bool,
    /// Le document déclare un formulaire (`/AcroForm`), peut-être vide.
    declares_form: bool,
    /// Pages déjà passées à l'OCR lors d'un enregistrement précédent (marqueur du dictionnaire `/Info`)
    pub ocr_scanned_pages: Vec<bool>,
    /// Pages déjà lues par l'OCR approfondi lors d'un enregistrement précédent
    pub deep_ocr_pages: Vec<bool>,
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
        // Dossier de l'exécutable et ses parents, puis dossier des dépendances de l'utilisateur
        // (jamais le dossier courant : il pourrait fournir une DLL étrangère).
        let installed = crate::deps::by_file_name("pdfium.dll").and_then(|d| crate::deps::locate(&d));
        if let Some(path) = installed {
            if let Ok(b) = Pdfium::bind_to_library(&path) {
                return Ok(b);
            }
        }
        Pdfium::bind_to_system_library().context(
            "Impossible de charger pdfium.dll : lancez `super_pdf_reader --fetch-deps` pour l'installer.",
        )
    }

    /// Ouvre un document sans lire les dimensions de chaque page (ouvertures répétées des threads d'arrière-plan).
    pub fn open_lightweight<'a>(&'a self, path: &Path) -> Result<LoadedPdfDocument<'a>> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let doc = self
            .pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("Échec d'ouverture du fichier PDF : {}", path.display()))?;
        Ok(LoadedPdfDocument {
            path: path.to_path_buf(),
            title: String::new(),
            page_dimensions: Vec::new(),
            has_form_widgets: false,
            has_app_drawn_annotations: false,
            page_details_complete: false,
            declares_form: false,
            ocr_scanned_pages: Vec::new(),
            deep_ocr_pages: Vec::new(),
            doc,
        })
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
        let declares_form = doc.form().is_some();
        let page_details_complete = (page_count as usize) <= FULL_SCAN_MAX_PAGES;
        // Gros document : formulaire supposé présent tant que les pages n'ont pas été lues (prudent).
        let mut has_form_widgets = declares_form && !page_details_complete;
        let mut has_app_drawn_annotations = false;

        for idx in 0..page_count {
            if page_details_complete {
                let page = pages
                    .get(idx)
                    .with_context(|| format!("Impossible de lire la page {}", idx + 1))?;
                let details = page_details(&page, declares_form);
                has_form_widgets |= details.has_form_widgets;
                has_app_drawn_annotations |= details.has_app_drawn_annotations;
                page_dimensions.push(details.dims);
            } else {
                // Taille affichée (rotation comprise) sans analyser le contenu de la page.
                let size = pages
                    .page_size(idx)
                    .with_context(|| format!("Impossible de lire la page {}", idx + 1))?;
                page_dimensions.push(PageDimensions::simple(size.width().value, size.height().value));
            }
        }

        let title = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Document.pdf")
            .to_string();

        let ocr_scanned_pages = parse_ocr_scanned_pages(
            &self
                .read_info_text(path, OCR_SCANNED_PAGES_INFO_KEY)
                .unwrap_or_default(),
            page_dimensions.len(),
        );
        let deep_ocr_pages = parse_ocr_scanned_pages(
            &self
                .read_info_text(path, OCR_DEEP_PAGES_INFO_KEY)
                .unwrap_or_default(),
            page_dimensions.len(),
        );

        Ok(LoadedPdfDocument {
            path: path.to_path_buf(),
            title,
            page_dimensions,
            has_form_widgets,
            has_app_drawn_annotations,
            page_details_complete,
            declares_form,
            ocr_scanned_pages,
            deep_ocr_pages,
            doc,
        })
    }

    /// Lit une entrée quelconque du dictionnaire `/Info` (pdfium-render n'expose que les clés standard).
    /// PDFium n'analyse que la table des références et le trailer : l'appel reste instantané.
    /// À appeler verrou PDFium tenu.
    fn read_info_text(&self, path: &Path, key: &str) -> Option<String> {
        let bindings = self.pdfium.bindings();
        let handle = bindings.FPDF_LoadDocument(path.to_str()?, None);
        if handle.is_null() {
            return None;
        }
        let len = bindings.FPDF_GetMetaText(handle, key, std::ptr::null_mut(), 0);
        let mut buffer = vec![0u8; len as usize];
        if len > 2 {
            bindings.FPDF_GetMetaText(handle, key, buffer.as_mut_ptr().cast(), len);
        }
        bindings.FPDF_CloseDocument(handle);
        let units: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        (!units.is_empty()).then(|| String::from_utf16_lossy(&units))
    }
}

fn page_details(page: &PdfPage<'_>, declares_form: bool) -> PageDetails {
    let annotations = page.annotations();
    PageDetails {
        dims: page_view_dimensions(page),
        has_app_drawn_annotations: annotations
            .iter()
            .any(|a| annotation_is_drawn_by_app(a.annotation_type())),
        has_form_widgets: declares_form
            && annotations
                .iter()
                .any(|a| a.annotation_type() == PdfPageAnnotationType::Widget),
    }
}

fn page_view_dimensions(page: &PdfPage<'_>) -> PageDimensions {
    let (box_left, box_bottom) = page
        .boundaries()
        .bounding()
        .map(|b| (b.bounds.left().value, b.bounds.bottom().value))
        .unwrap_or((0.0, 0.0));
    PageDimensions {
        width_pt: page.width().value,
        height_pt: page.height().value,
        box_left,
        box_bottom,
        rotation_deg: page.rotation().map_or(0, |r| r.as_degrees() as u32),
    }
}

/// Supprime, dans la copie en mémoire du thread de rendu, les annotations que l'application dessine
/// elle-même. Tampons, soulignés, polygones... restent dessinés par PDFium avec leur apparence d'origine.
fn remove_annotations_drawn_by_app(page: &mut PdfPage<'_>) {
    let annotations = page.annotations_mut();
    let mut index = annotations.len();
    while index > 0 {
        index -= 1;
        let Ok(annotation) = annotations.get(index) else {
            continue;
        };
        if annotation_is_drawn_by_app(annotation.annotation_type()) {
            let _ = annotations.delete_annotation(annotation);
        }
    }
}

fn annotation_is_drawn_by_app(annotation_type: PdfPageAnnotationType) -> bool {
    matches!(
        annotation_type,
        PdfPageAnnotationType::Highlight
            | PdfPageAnnotationType::FreeText
            | PdfPageAnnotationType::Ink
            | PdfPageAnnotationType::Square
            | PdfPageAnnotationType::Circle
            | PdfPageAnnotationType::Line
            | PdfPageAnnotationType::Text
            | PdfPageAnnotationType::Popup
    )
}

/// Boîtes (points PDF, origine bas-gauche) des objets texte situés hors page ou hors de leur masque de découpe.
/// Ces textes n'apparaissent jamais à l'écran mais PDFium les extrait quand même.
fn hidden_text_boxes(page: &PdfPage<'_>, dim: PageDimensions) -> Vec<[f32; 4]> {
    let (box_w, box_h) = dim.unrotated_size();
    let mut hidden = Vec::new();
    for obj in page.objects().iter() {
        if obj.as_text_object().is_none() {
            continue;
        }
        let Ok(bounds) = obj.bounds() else {
            continue;
        };
        let (left, right) = (bounds.left().value, bounds.right().value);
        let (bottom, top) = (bounds.bottom().value, bounds.top().value);
        let (x0, x1) = (left.min(right), left.max(right));
        let (y0, y1) = (bottom.min(top), bottom.max(top));
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);

        let off_page = x1 < dim.box_left
            || x0 > dim.box_left + box_w
            || y1 < dim.box_bottom
            || y0 > dim.box_bottom + box_h;
        let clipped_out = obj.get_clip_path().is_some_and(|clip| {
            (0..clip.len()).any(|i| {
                let Ok(path) = clip.get(i) else {
                    return false;
                };
                let mut path_box = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                for s in 0..path.len() {
                    if let Ok(segment) = path.get(s) {
                        let (x, y) = (segment.x().value, segment.y().value);
                        path_box = [
                            path_box[0].min(x),
                            path_box[1].min(y),
                            path_box[2].max(x),
                            path_box[3].max(y),
                        ];
                    }
                }
                // Un masque sans point (masque textuel) ne permet pas de conclure.
                path_box[0] <= path_box[2]
                    && (cx < path_box[0] - 1.0
                        || cx > path_box[2] + 1.0
                        || cy < path_box[1] - 1.0
                        || cy > path_box[3] + 1.0)
            })
        });
        if off_page || clipped_out {
            hidden.push([x0, y0, x1, y1]);
        }
    }
    hidden
}

/// Parcourt un objet de page en descendant dans les formulaires et appelle `visit` pour chaque image,
/// avec son rectangle en coordonnées de page (les enfants d'un formulaire sont exprimés dans son repère).
fn visit_page_images<F>(
    obj: &PdfPageObject<'_>,
    form_matrices: &mut Vec<PdfMatrix>,
    dim: PageDimensions,
    visit: &mut F,
) where
    F: FnMut(&PdfPageObject<'_>, PdfTopDownRect),
{
    if let Some(form) = obj.as_x_object_form_object() {
        let Ok(matrix) = obj.matrix() else {
            return;
        };
        form_matrices.push(matrix);
        for i in 0..form.len() {
            if let Ok(child) = form.get(i) {
                visit_page_images(&child, form_matrices, dim, visit);
            }
        }
        form_matrices.pop();
        return;
    }
    if obj.as_image_object().is_none() {
        return;
    }
    let Ok(bounds) = obj.bounds() else {
        return;
    };
    let mut xs = [f32::MAX, f32::MIN];
    let mut ys = [f32::MAX, f32::MIN];
    for (x, y) in [
        (bounds.left(), bounds.top()),
        (bounds.right(), bounds.top()),
        (bounds.left(), bounds.bottom()),
        (bounds.right(), bounds.bottom()),
    ] {
        let (mut px, mut py) = (x, y);
        for m in form_matrices.iter().rev() {
            (px, py) = m.apply_to_points(px, py);
        }
        let (view_x, view_y) = dim.pdf_to_view(px.value, py.value);
        xs = [xs[0].min(view_x), xs[1].max(view_x)];
        ys = [ys[0].min(view_y), ys[1].max(view_y)];
    }
    visit(
        obj,
        PdfTopDownRect {
            x_min: xs[0].clamp(0.0, dim.width_pt),
            x_max: xs[1].clamp(0.0, dim.width_pt),
            y_min: ys[0].clamp(0.0, dim.height_pt),
            y_max: ys[1].clamp(0.0, dim.height_pt),
            angle_deg: 0.0,
        },
    );
}

impl<'a> LoadedPdfDocument<'a> {
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.page_dimensions.len()
    }

    /// Effectue le rendu matriciel haute qualité d'une page donnée à la taille cible en pixels.
    pub fn render_page(&self, page_index: usize, target_width_px: i32, target_height_px: i32) -> Result<ColorImage> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut page = self
            .doc
            .pages()
            .get(page_index as u16)
            .with_context(|| format!("Page {} hors limites", page_index))?;
        remove_annotations_drawn_by_app(&mut page);

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

    /// Rend uniquement `region` de la page à `scale` pixels par point PDF.
    /// La taille de la page virtuelle n'est pas limitée : seule la tuile occupe de la mémoire.
    pub fn render_page_tile(
        &self,
        page_index: usize,
        scale: f32,
        region: TileRegion,
    ) -> Result<ColorImage> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut page = self
            .doc
            .pages()
            .get(page_index as u16)
            .with_context(|| format!("Page {} hors limites", page_index))?;
        remove_annotations_drawn_by_app(&mut page);

        let config = PdfRenderConfig::new()
            .scale_page_by_factor(scale)
            .translate(
                PdfPoints::new(-(region.x as f32) / scale),
                PdfPoints::new(-(region.y as f32) / scale),
            )
            .context("Transformation de tuile invalide")?
            .clip(0, 0, region.width, region.height)
            .render_annotations(true)
            .use_lcd_text_rendering(true);

        let mut bitmap = PdfBitmap::empty(
            region.width,
            region.height,
            PdfBitmapFormat::BGRA,
            self.doc.bindings(),
        )
        .context("Allocation de la tuile impossible")?;
        page.render_into_bitmap_with_config(&mut bitmap, &config)
            .with_context(|| format!("Erreur de rendu de tuile sur la page {}", page_index + 1))?;

        let rgba = bitmap.as_rgba_bytes();
        Ok(ColorImage::from_rgba_unmultiplied(
            [region.width as usize, region.height as usize],
            &rgba,
        ))
    }

    /// Les champs de formulaire ne peuvent pas être dessinés dans une tuile (limite de PDFium).
    pub fn has_form_fields(&self) -> bool {
        self.has_form_widgets
    }

    /// Charge une page pour en lire la rotation, la zone visible, les annotations et les champs.
    pub fn page_details(&self, page_index: usize) -> Option<PageDetails> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let page = self.doc.pages().get(page_index as u16).ok()?;
        Some(page_details(&page, self.declares_form || self.doc.form().is_some()))
    }

    /// Extrait la couche de texte natif d'une seule page à la volée (< 1 ms, idéal pour les très gros PDF).
    pub fn extract_single_page_text_layer(&self, page_idx: usize) -> PageTextLayer {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let pages = self.doc.pages();
        let mut positioned_chars = Vec::new();
        if let Ok(page) = pages.get(page_idx as u16) {
            let dim = page_view_dimensions(&page);
            let hidden_boxes = hidden_text_boxes(&page, dim);
            if let Ok(text_page) = page.text() {
                for ch_item in text_page.chars().iter() {
                    let Some(ch) = ch_item.unicode_char() else {
                        continue;
                    };
                    if ch == '\0' {
                        continue;
                    }
                    let bounds_res = ch_item
                        .loose_bounds()
                        .or_else(|_| ch_item.tight_bounds());
                    let rect = if let Ok(b) = bounds_res {
                        let left = b.left().value;
                        let right = b.right().value;
                        let top = b.top().value;
                        let bottom = b.bottom().value;

                        let (cx, cy) = ((left + right) * 0.5, (top + bottom) * 0.5);
                        if hidden_boxes
                            .iter()
                            .any(|h| cx >= h[0] && cx <= h[2] && cy >= h[1] && cy <= h[3])
                        {
                            continue;
                        }

                        let (view_a_x, view_a_y) = dim.pdf_to_view(left, top);
                        let (view_b_x, view_b_y) = dim.pdf_to_view(right, bottom);
                        let x_min = view_a_x.min(view_b_x).clamp(0.0, dim.width_pt);
                        let x_max = view_a_x.max(view_b_x).clamp(0.0, dim.width_pt);
                        let y_min = view_a_y.min(view_b_y).clamp(0.0, dim.height_pt);
                        let y_max = view_a_y.max(view_b_y).clamp(0.0, dim.height_pt);
                        let raw_angle = (ch_item.angle_degrees().unwrap_or(0.0)
                            + dim.rotation_deg as f32)
                            .rem_euclid(360.0);
                        let angle_deg = if raw_angle > 180.0 {
                            raw_angle - 360.0
                        } else {
                            raw_angle
                        };

                        PdfTopDownRect {
                            x_min,
                            y_min,
                            x_max,
                            y_max,
                            angle_deg,
                        }
                    } else {
                        PdfTopDownRect {
                            x_min: 0.0,
                            y_min: 0.0,
                            x_max: 0.0,
                            y_max: 0.0,
                            angle_deg: 0.0,
                        }
                    };

                    positioned_chars.push(PositionedChar {
                        ch,
                        rect,
                        is_ocr: false,
                    });
                }
            }
        }
        PageTextLayer::new(page_idx, positioned_chars)
    }

    /// Liste les images embarquées d'une page (formulaires inclus) : rectangle en points PDF top-down et taille en pixels.
    #[allow(dead_code)]
    pub fn list_page_image_regions(&self, page_idx: usize) -> Vec<(PdfTopDownRect, usize, usize)> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut regions = Vec::new();
        let Ok(page) = self.doc.pages().get(page_idx as u16) else {
            return regions;
        };
        let dim = page_view_dimensions(&page);
        let mut visit = |obj: &PdfPageObject<'_>, rect: PdfTopDownRect| {
            if rect.width() < 12.0 || rect.height() < 12.0 {
                return;
            }
            if let Some(image) = obj.as_image_object() {
                let px_w = image.width().map_or(rect.width().round() as usize, |w| w as usize);
                let px_h = image.height().map_or(rect.height().round() as usize, |h| h as usize);
                regions.push((rect, px_w, px_h));
            }
        };
        for obj in page.objects().iter() {
            visit_page_images(&obj, &mut Vec::new(), dim, &mut visit);
        }
        regions
    }

    /// Extrait les pixels d'origine de chaque image embarquée (formulaires inclus), sans ré-échantillonnage.
    pub fn extract_page_embedded_images(&self, page_idx: usize) -> Vec<(PdfTopDownRect, RgbImage)> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut results = Vec::new();
        let Ok(page) = self.doc.pages().get(page_idx as u16) else {
            return results;
        };
        let dim = page_view_dimensions(&page);
        let mut visit = |obj: &PdfPageObject<'_>, rect: PdfTopDownRect| {
            if rect.width() < 20.0 || rect.height() < 20.0 {
                return;
            }
            let Some(image) = obj.as_image_object() else {
                return;
            };
            let Ok(bitmap) = image.get_raw_bitmap() else {
                return;
            };
            let (w, h) = (bitmap.width() as usize, bitmap.height() as usize);
            let rgba = bitmap.as_rgba_bytes();
            if w < 16 || h < 16 || rgba.len() != w * h * 4 {
                return;
            }
            // Les zones transparentes sont composées sur blanc, le fond attendu par la détection.
            let rgb: Vec<u8> = rgba
                .chunks_exact(4)
                .flat_map(|p| {
                    let alpha = p[3] as u32;
                    [0, 1, 2].map(|c| ((p[c] as u32 * alpha + 255 * (255 - alpha)) / 255) as u8)
                })
                .collect();
            if let Some(pixels) = RgbImage::from_raw(w as u32, h as u32, rgb) {
                // L'OCR travaille dans le repère affiché : l'image suit la rotation de la page.
                let oriented = match dim.rotation_deg {
                    90 => image::imageops::rotate90(&pixels),
                    180 => image::imageops::rotate180(&pixels),
                    270 => image::imageops::rotate270(&pixels),
                    _ => pixels,
                };
                results.push((rect, oriented));
            }
        };
        for obj in page.objects().iter() {
            visit_page_images(&obj, &mut Vec::new(), dim, &mut visit);
        }
        results
    }

    /// Extrait la couche de texte natif de toutes les pages du document (avec boîtes englobantes et angles de rotation).
    #[allow(dead_code)]
    pub fn extract_all_pages_text_layers(&self) -> Vec<PageTextLayer> {
        let count = self.page_dimensions.len();
        let mut layers = Vec::with_capacity(count);
        for page_idx in 0..count {
            layers.push(self.extract_single_page_text_layer(page_idx));
        }
        layers
    }
}

#[cfg(test)]
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

#[cfg(test)]
/// Génère le fichier PDF de démonstration par défaut.
pub fn ensure_sample_pdf(path: &Path) -> Result<()> {
    ensure_custom_sample_pdf(path, "SUPER LECTEUR PDF", (0.10, 0.24, 0.48), 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_large_document_opens_with_sizes_then_reads_details_lazily() {
        use crate::annotations::{save_annotations_and_ocr_to_pdf, AnnotationKind, PdfAnnotation};
        let src = PathBuf::from("test_pdfs/unit_test_large_src.pdf");
        let dst = PathBuf::from("test_pdfs/unit_test_large_annotated.pdf");
        let pages = FULL_SCAN_MAX_PAGES + 5;
        ensure_custom_sample_pdf(&src, "GROS DOCUMENT", (0.2, 0.3, 0.5), pages).expect("Doit créer le PDF");
        let engine = PdfEngine::instance();
        let dims = engine.load_document(&src).expect("Doit charger").page_dimensions.clone();
        let rect = PdfTopDownRect { x_min: 100.0, y_min: 100.0, x_max: 300.0, y_max: 120.0, angle_deg: 0.0 };
        let highlight = PdfAnnotation {
            meta: Default::default(),
            id: 1,
            page_index: pages - 2,
            rect,
            color_rgb: (255, 226, 52),
            kind: AnnotationKind::Highlight { quads: vec![rect] },
        };
        save_annotations_and_ocr_to_pdf(&src, &dst, &[highlight], &[], &dims).expect("Doit enregistrer");

        let doc = engine.load_document(&dst).expect("Doit charger");
        // Gros document : ouverture sans charger les pages, donc sans les annotations.
        assert!(!doc.page_details_complete);
        assert!(!doc.has_app_drawn_annotations);
        assert_eq!(doc.page_dimensions.len(), pages);
        // Les tailles lues sans charger les pages sont les tailles exactes.
        for (fast, exact) in doc.page_dimensions.iter().zip(&dims) {
            assert!((fast.width_pt - exact.width_pt).abs() < 0.01 && (fast.height_pt - exact.height_pt).abs() < 0.01);
        }
        // Les détails, lus page par page, retrouvent l'annotation.
        let details = doc.page_details(pages - 2).expect("Doit lire la page");
        assert!(details.has_app_drawn_annotations);
        assert!(!doc.page_details(0).unwrap().has_app_drawn_annotations);
    }

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

    #[test]
    fn test_view_and_pdf_coordinates_are_inverse_on_rotated_cropped_pages() {
        for rotation_deg in [0, 90, 180, 270] {
            let (unrotated_w, unrotated_h) = (400.0_f32, 600.0_f32);
            let (width_pt, height_pt) = if rotation_deg % 180 == 0 {
                (unrotated_w, unrotated_h)
            } else {
                (unrotated_h, unrotated_w)
            };
            let dim = PageDimensions {
                width_pt,
                height_pt,
                box_left: 30.0,
                box_bottom: 50.0,
                rotation_deg,
            };
            let (view_x, view_y) = dim.pdf_to_view(130.0, 250.0);
            let (pdf_x, pdf_y) = dim.view_to_pdf(view_x, view_y);
            assert!((pdf_x - 130.0).abs() < 1e-3 && (pdf_y - 250.0).abs() < 1e-3);
        }
        let upright = PageDimensions {
            width_pt: 400.0,
            height_pt: 600.0,
            box_left: 30.0,
            box_bottom: 50.0,
            rotation_deg: 0,
        };
        assert_eq!(upright.pdf_to_view(30.0, 650.0), (0.0, 0.0));
        let turned = PageDimensions {
            rotation_deg: 90,
            width_pt: 600.0,
            height_pt: 400.0,
            ..upright
        };
        assert_eq!(turned.pdf_to_view(30.0, 50.0), (0.0, 0.0), "le coin bas-gauche monte en haut à gauche");
    }

    #[test]
    fn test_tile_render_matches_same_area_of_full_page() {
        let sample_path = PathBuf::from("test_pdfs/unit_test_sample.pdf");
        ensure_sample_pdf(&sample_path).expect("Doit créer le PDF de test");
        let engine = PdfEngine::instance();
        let doc = engine.load_document(&sample_path).expect("Doit charger le PDF");
        let dim = doc.page_dimensions[0];

        let scale = 3.0_f32;
        let (full_w, full_h) = (
            (dim.width_pt * scale).round() as i32,
            (dim.height_pt * scale).round() as i32,
        );
        let full = doc.render_page(0, full_w, full_h).expect("Doit rendre la page");
        let region = TileRegion {
            x: 300,
            y: 500,
            width: 512,
            height: 256,
        };
        let tile = doc.render_page_tile(0, scale, region).expect("Doit rendre la tuile");
        assert_eq!(tile.size, [512, 256]);

        let mut total_difference = 0_u64;
        let mut dark_pixels = 0_u32;
        for y in 0..region.height as usize {
            for x in 0..region.width as usize {
                let from_tile = tile.pixels[y * 512 + x];
                let index = (y + region.y as usize) * full.width() + x + region.x as usize;
                let from_page = full.pixels[index];
                total_difference += from_tile.r().abs_diff(from_page.r()) as u64;
                dark_pixels += u32::from(from_tile.r() < 128);
            }
        }
        let mean_difference = total_difference as f32 / (512.0 * 256.0);
        assert!(dark_pixels > 100, "la tuile doit contenir du contenu de la page");
        assert!(
            mean_difference < 8.0,
            "la tuile doit être alignée sur la page, écart moyen {mean_difference}"
        );
    }

    #[test]
    fn test_native_text_extraction_search_and_selection_including_diagonal_text() {
        use crate::text_search::SearchBarState;

        let sample_path = PathBuf::from("test_pdfs/unit_test_text_search.pdf");
        ensure_sample_pdf(&sample_path).expect("Doit créer le PDF de test");

        let engine = PdfEngine::instance();
        let doc = engine
            .load_document(&sample_path)
            .expect("Doit charger le PDF de test");

        let layers = doc.extract_all_pages_text_layers();
        assert_eq!(layers.len(), 5);
        assert!(
            layers[0].chars.len() > 100,
            "La page 1 doit contenir plus de 100 caractères extraits"
        );

        // 1. Test Recherche Ctrl+F sur le texte incliné à +30° en diagonale ("Diagonal +30")
        let mut search = SearchBarState::default();
        search.query = "Diagonal +30".to_string();
        search.execute_search(&layers);
        assert_eq!(
            search.matches.len(),
            5,
            "Doit trouver 'Diagonal +30' sur chacune des 5 pages"
        );
        assert!(
            !search.matches[0].highlight_boxes.is_empty(),
            "Les boîtes de surbrillance de l'occurrence diagonale doivent être calculées"
        );

        // 2. Test Recherche Ctrl+F sur le texte vertical à 90° ("Consommation RAM")
        search.query = "Consommation RAM".to_string();
        search.execute_search(&layers);
        assert_eq!(
            search.matches.len(),
            5,
            "Doit trouver le texte vertical 'Consommation RAM' sur les 5 pages"
        );

        // 3. Test Sélection par bloc rectangulaire (Alt + Glisser)
        let selection_zone = PdfTopDownRect {
            x_min: 40.0,
            y_min: 30.0,
            x_max: 550.0,
            y_max: 115.0,
            angle_deg: 0.0,
        };
        let (selected_text, boxes) = layers[0].select_in_rect(&selection_zone);
        assert!(
            selected_text.contains("SUPER LECTEUR PDF"),
            "Le texte sélectionné dans le bandeau supérieur doit contenir le titre, reçu : '{}'",
            selected_text
        );
        assert!(!boxes.is_empty());

        // 4. Test Sélection au fil des phrases (Curseur I : Glisser simple d'un mot de la ligne 1 à la ligne 3)
        let (stream_text, stream_boxes) = layers[0].select_char_stream(0, 65);
        assert!(
            stream_text.len() >= 50,
            "La sélection en flux continu (Curseur I) doit capturer toute la séquence de caractères"
        );
        assert!(!stream_boxes.is_empty());

        // 5. Test Double-clic sur un mot
        let first_char_center = layers[0].chars[1].rect.center();
        let (_, _, word_txt, word_boxes) = layers[0]
            .select_word_at_point(first_char_center.0, first_char_center.1)
            .expect("Le double-clic doit sélectionner le mot sous le pointeur");
        assert!(
            !word_txt.is_empty() && !word_txt.contains(' '),
            "Le double-clic doit isoler un mot complet : '{}'",
            word_txt
        );
        assert!(!word_boxes.is_empty());
    }
}

