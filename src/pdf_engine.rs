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

/// Mots de passe saisis pendant la session, par fichier. Jamais écrits sur le disque. PDFium les
/// garde le temps de vie du document : ils sont conservés jusqu'à la fin du processus.
static PASSWORDS: OnceLock<Mutex<std::collections::HashMap<String, &'static str>>> = OnceLock::new();

fn password_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

/// Mot de passe à utiliser pour ce fichier (saisi plus tôt dans la session).
pub fn set_document_password(path: &Path, password: &str) {
    let leaked: &'static str = Box::leak(password.to_string().into_boxed_str());
    PASSWORDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(password_key(path), leaked);
}

fn document_password(path: &Path) -> Option<&'static str> {
    PASSWORDS
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&password_key(path))
        .copied()
}

/// Le document est protégé : mot de passe absent ou faux.
#[derive(Debug)]
pub struct PasswordRequired {
    pub wrong_password: bool,
}

impl std::fmt::Display for PasswordRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.wrong_password {
            write!(f, "mot de passe incorrect")
        } else {
            write!(f, "ce document est protégé par un mot de passe")
        }
    }
}

impl std::error::Error for PasswordRequired {}

/// Au-delà, l'ouverture ne charge plus chaque page : charger une page coûte environ 25 ms
/// (analyse de son contenu), soit plus de 10 s pour 460 pages. Les détails des pages (annotations,
/// rotation, champs de formulaire) sont alors lus en tâche de fond.
pub const FULL_SCAN_MAX_PAGES: usize = 40;

/// Nature d'un champ de formulaire que l'application sait remplir.
#[derive(Debug, Clone, PartialEq)]
pub enum FormWidgetKind {
    Text { multiline: bool },
    Checkbox,
    /// Bouton radio : position de ce bouton dans son groupe
    Radio { index: u32 },
    /// Liste déroulante
    Choice { options: Vec<String> },
}

/// Un champ de formulaire tel qu'il apparaît sur une page (un même champ peut avoir plusieurs
/// emplacements, par exemple les boutons d'un groupe radio).
#[derive(Debug, Clone, PartialEq)]
pub struct FormWidget {
    /// Nom complet du champ (`parent.enfant`)
    pub name: String,
    pub kind: FormWidgetKind,
    /// Emplacement en coordonnées de la page affichée
    pub rect: PdfTopDownRect,
    /// Valeur enregistrée dans le fichier (texte, choix)
    pub value: String,
    /// Case cochée / bouton radio sélectionné
    pub checked: bool,
    pub read_only: bool,
}

/// Entrée du sommaire (signets) d'un document.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineEntry {
    pub title: String,
    /// Page de destination, si le signet en a une dans ce document
    pub page: Option<usize>,
    /// Hauteur visée dans la page (points, depuis le haut de la page affichée), si le signet la précise
    pub y: Option<f32>,
    /// Profondeur dans l'arborescence (0 = premier niveau)
    pub depth: usize,
}

/// Au-delà, le sommaire est tronqué (documents mal formés aux signets en boucle).
const MAX_OUTLINE_ENTRIES: usize = 5000;
const MAX_OUTLINE_DEPTH: usize = 16;

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
    /// Ouvre le fichier avec le mot de passe saisi dans la session, s'il y en a un.
    /// À appeler verrou PDFium tenu.
    fn open_with_password<'a>(&'a self, path: &Path) -> Result<PdfDocument<'a>> {
        let password = document_password(path);
        match self.pdfium.load_pdf_from_file(path, password) {
            Ok(doc) => Ok(doc),
            Err(PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)) => {
                Err(anyhow::Error::new(PasswordRequired { wrong_password: password.is_some() }))
            }
            Err(e) => Err(e).with_context(|| format!("Échec d'ouverture du fichier PDF : {}", path.display())),
        }
    }

    pub fn open_lightweight<'a>(&'a self, path: &Path) -> Result<LoadedPdfDocument<'a>> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let doc = self.open_with_password(path)?;
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
        let doc = self.open_with_password(path)?;

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

    /// Ouvre le fichier avec l'API brute de PDFium, le temps de `work` (impression).
    pub fn with_raw_document<R>(
        &self,
        path: &Path,
        work: impl FnOnce(&RawDocument<'_>) -> std::result::Result<R, String>,
    ) -> std::result::Result<R, String> {
        let render = render_page_to_dc_fn().ok_or("FPDF_RenderPage absent de pdfium.dll")?;
        let path_text = path.to_str().ok_or("chemin de fichier illisible")?;
        let handle = {
            let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            self.pdfium.bindings().FPDF_LoadDocument(path_text, document_password(path))
        };
        if handle.is_null() {
            return Err(format!("impossible d'ouvrir {}", path.display()));
        }
        let document = RawDocument { engine: self, handle, render };
        let result = work(&document);
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        self.pdfium.bindings().FPDF_CloseDocument(handle);
        result
    }

    /// Lit une entrée quelconque du dictionnaire `/Info` (pdfium-render n'expose que les clés standard).
    /// PDFium n'analyse que la table des références et le trailer : l'appel reste instantané.
    /// À appeler verrou PDFium tenu.
    fn read_info_text(&self, path: &Path, key: &str) -> Option<String> {
        let bindings = self.pdfium.bindings();
        let handle = bindings.FPDF_LoadDocument(path.to_str()?, document_password(path));
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

/// `FPDF_RenderPage` (Windows) : dessine une page sur un contexte graphique, ici une imprimante.
/// pdfium-render ne l'expose qu'avec une dépendance lourde : on le prend directement dans pdfium.dll.
type RenderPageToDc = unsafe extern "system" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, i32, i32, i32, i32, i32, i32);

fn render_page_to_dc_fn() -> Option<RenderPageToDc> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
        fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
    }
    let module_name: Vec<u16> = "pdfium.dll".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let module = GetModuleHandleW(module_name.as_ptr());
        if module.is_null() {
            return None;
        }
        let address = GetProcAddress(module, b"FPDF_RenderPage\0".as_ptr());
        (!address.is_null()).then(|| std::mem::transmute::<*mut std::ffi::c_void, RenderPageToDc>(address))
    }
}

/// Document ouvert avec l'API brute de PDFium (voir [`PdfEngine::with_raw_document`]).
pub struct RawDocument<'a> {
    engine: &'a PdfEngine,
    handle: FPDF_DOCUMENT,
    render: RenderPageToDc,
}

impl RawDocument<'_> {
    /// Dessine la page sur le contexte d'imprimante `dc`, centrée et agrandie au maximum dans la zone
    /// imprimable (`area_w` x `area_h` pixels). Une page en paysage sur une feuille en portrait (et
    /// inversement) est tournée d'un quart de tour pour occuper la feuille.
    pub fn render_page_to_dc(
        &self,
        page_index: usize,
        dc: *mut std::ffi::c_void,
        area_w: i32,
        area_h: i32,
        flags: i32,
    ) -> std::result::Result<(), String> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let bindings = self.engine.pdfium.bindings();
        let page = bindings.FPDF_LoadPage(self.handle, page_index as i32);
        if page.is_null() {
            return Err(format!("page {} illisible", page_index + 1));
        }
        let (w, h) = (bindings.FPDF_GetPageWidthF(page), bindings.FPDF_GetPageHeightF(page));
        let (rotate, page_w, page_h) = if (w > h) != (area_w > area_h) { (1, h, w) } else { (0, w, h) };
        let scale = (area_w as f32 / page_w.max(1.0)).min(area_h as f32 / page_h.max(1.0));
        let (size_x, size_y) = ((page_w * scale) as i32, (page_h * scale) as i32);
        let (x, y) = ((area_w - size_x) / 2, (area_h - size_y) / 2);
        unsafe { (self.render)(dc, page.cast(), x, y, size_x, size_y, rotate, flags) };
        bindings.FPDF_ClosePage(page);
        Ok(())
    }
}

/// Haut (points) de la première ligne de la page qui contient `title`, espaces ignorés.
fn find_title_top(layer: &PageTextLayer, title: &str) -> Option<f32> {
    let compact = |c: char| !c.is_whitespace();
    let wanted: Vec<char> = title.chars().filter(|c| compact(*c)).flat_map(char::to_lowercase).collect();
    if wanted.len() < 2 {
        return None;
    }
    let chars: Vec<&PositionedChar> = layer.chars.iter().filter(|c| compact(c.ch)).collect();
    let text: Vec<char> = chars.iter().map(|c| c.ch.to_lowercase().next().unwrap_or(c.ch)).collect();
    (0..text.len().saturating_sub(wanted.len() - 1))
        .find(|&start| text[start..start + wanted.len()] == wanted[..])
        .map(|start| chars[start].rect.y_min)
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

    /// Le document déclare un formulaire.
    pub fn declares_form(&self) -> bool {
        self.declares_form || self.doc.form().is_some()
    }

    /// Champs de formulaire de la page que l'application sait remplir.
    pub fn page_form_widgets(&self, page_index: usize) -> Vec<FormWidget> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(page) = self.doc.pages().get(page_index as u16) else {
            return Vec::new();
        };
        let dims = page_view_dimensions(&page);
        let mut widgets = Vec::new();
        for annotation in page.annotations().iter() {
            let PdfPageAnnotation::Widget(widget) = &annotation else { continue };
            let Some(field) = widget.form_field() else { continue };
            let Ok(bounds) = annotation.bounds() else { continue };
            let (x0, y0) = dims.pdf_to_view(bounds.left().value, bounds.bottom().value);
            let (x1, y1) = dims.pdf_to_view(bounds.right().value, bounds.top().value);
            let rect = PdfTopDownRect {
                x_min: x0.min(x1),
                y_min: y0.min(y1),
                x_max: x0.max(x1),
                y_max: y0.max(y1),
                angle_deg: 0.0,
            };
            let (kind, value, checked) = if let Some(text) = field.as_text_field() {
                (FormWidgetKind::Text { multiline: text.is_multiline() }, text.value().unwrap_or_default(), false)
            } else if let Some(checkbox) = field.as_checkbox_field() {
                (FormWidgetKind::Checkbox, String::new(), checkbox.is_checked().unwrap_or(false))
            } else if let Some(radio) = field.as_radio_button_field() {
                (
                    FormWidgetKind::Radio { index: radio.index_in_group() },
                    String::new(),
                    radio.is_checked().unwrap_or(false),
                )
            } else if let Some(combo) = field.as_combo_box_field() {
                let options = combo.options();
                let labels = (0..options.len())
                    .filter_map(|i| options.get(i).ok().and_then(|o| o.label().cloned()))
                    .collect();
                (FormWidgetKind::Choice { options: labels }, combo.value().unwrap_or_default(), false)
            } else {
                continue; // listes à choix multiples, boutons, signatures : non pris en charge
            };
            widgets.push(FormWidget {
                name: field.name().unwrap_or_default(),
                kind,
                rect,
                value,
                checked,
                read_only: field.is_read_only(),
            });
        }
        widgets
    }

    /// Sommaire avec, pour les signets qui ne donnent que leur page, la hauteur de leur titre
    /// retrouvée dans le texte de cette page. Lit le texte des pages concernées : à appeler hors du
    /// thread de l'interface.
    pub fn outline_with_positions(&self) -> Vec<OutlineEntry> {
        let mut entries = self.outline();
        let mut pages: std::collections::HashMap<usize, PageTextLayer> = std::collections::HashMap::new();
        for entry in entries.iter_mut().filter(|e| e.y.is_none()) {
            let Some(page) = entry.page else { continue };
            let layer = pages.entry(page).or_insert_with(|| self.extract_single_page_text_layer(page));
            entry.y = find_title_top(layer, &entry.title);
        }
        entries
    }

    /// Sommaire du document, à plat, dans l'ordre de lecture.
    pub fn outline(&self) -> Vec<OutlineEntry> {
        let _lock = PDFIUM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let bookmarks = self.doc.bookmarks();
        let mut entries = Vec::new();
        // Parcours en profondeur : (signet, profondeur), le premier enfant avant le suivant.
        let mut stack: Vec<(PdfBookmark<'_>, usize)> = bookmarks.root().map(|b| (b, 0)).into_iter().collect();
        while let Some((bookmark, depth)) = stack.pop() {
            if entries.len() >= MAX_OUTLINE_ENTRIES {
                break;
            }
            // Page et hauteur du titre visé : sans la hauteur, plusieurs parties d'une même page
            // renverraient toutes en haut de la page.
            let target = |d: &PdfDestination<'_>| {
                let page = d.page_index().ok().map(|p| p as usize);
                let top = match d.view_settings().ok() {
                    Some(PdfDestinationViewSettings::SpecificCoordinatesAndZoom(_, y, _)) => y,
                    Some(PdfDestinationViewSettings::FitPageHorizontallyToWindow(y)) => y,
                    Some(PdfDestinationViewSettings::FitBoundsHorizontallyToWindow(y)) => y,
                    _ => None,
                };
                (page, top)
            };
            let (page, top) = match bookmark.destination() {
                Some(d) => target(&d),
                None => bookmark
                    .action()
                    .and_then(|action| {
                        let d = action.as_local_destination_action()?.destination().ok()?;
                        Some(target(&d))
                    })
                    .unwrap_or((None, None)),
            };
            let y = page.zip(top).and_then(|(p, top)| {
                let dims = self.page_dimensions.get(p)?;
                Some(dims.pdf_to_view(dims.box_left, top.value).1.clamp(0.0, dims.height_pt))
            });
            entries.push(OutlineEntry {
                y,
                // Espaces typographiques (U+2002…) absents de la police : ramenés à des espaces simples.
                title: bookmark
                    .title()
                    .unwrap_or_default()
                    .split(char::is_whitespace)
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                page,
                depth,
            });
            if let Some(sibling) = bookmark.next_sibling() {
                stack.push((sibling, depth));
            }
            if depth < MAX_OUTLINE_DEPTH {
                if let Some(child) = bookmark.first_child() {
                    stack.push((child, depth + 1));
                }
            }
        }
        entries
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
    fn test_outline_is_read_in_reading_order_with_depth() {
        let path = PathBuf::from("test_pdfs/unit_test_outline.pdf");
        ensure_sample_pdf(&path).expect("Doit créer le PDF");
        // Signets : « Introduction » (p. 1) > « Contexte » (p. 2), puis « Résultats » (p. 4).
        let mut doc = Document::load(&path).expect("Doit charger");
        let pages = doc.get_pages();
        let page_ref = |n: u32| Object::Reference(pages[&n]);
        let outlines_id = doc.new_object_id();
        let intro_id = doc.new_object_id();
        let context_id = doc.new_object_id();
        let results_id = doc.new_object_id();
        let dest = |n: u32| Object::Array(vec![page_ref(n), "XYZ".into(), 0.into(), 600.into(), Object::Null]);
        doc.objects.insert(context_id, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("Contexte"), "Parent" => intro_id, "Dest" => dest(2),
        }));
        doc.objects.insert(intro_id, Object::Dictionary(dictionary! {
            "Title" => Object::string_literal("Introduction"), "Parent" => outlines_id, "Dest" => dest(1),
            "First" => context_id, "Last" => context_id, "Count" => 1, "Next" => results_id,
        }));
        doc.objects.insert(results_id, Object::Dictionary(dictionary! {
            "Title" => Object::String(b"R\xE9sultats".to_vec(), lopdf::StringFormat::Literal), "Parent" => outlines_id, "Dest" => dest(4),
            "Prev" => intro_id,
        }));
        doc.objects.insert(outlines_id, Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => intro_id, "Last" => results_id, "Count" => 3,
        }));
        let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference).expect("Racine");
        doc.get_object_mut(catalog_id)
            .and_then(Object::as_dict_mut)
            .expect("Catalogue")
            .set("Outlines", Object::Reference(outlines_id));
        let with_outline = PathBuf::from("test_pdfs/unit_test_outline_marks.pdf");
        doc.save(&with_outline).expect("Doit enregistrer");

        let loaded = PdfEngine::instance().load_document(&with_outline).expect("Doit charger");
        let outline: Vec<(String, Option<usize>, usize)> =
            loaded.outline().into_iter().map(|e| (e.title, e.page, e.depth)).collect();
        // Titre visé à 600 pt du bas, soit 242 pt du haut d'une page A4.
        assert!(loaded.outline().iter().all(|e| e.y.is_some_and(|y| (y - 242.0).abs() < 1.0)));
        assert_eq!(
            outline,
            vec![
                ("Introduction".to_string(), Some(0), 0),
                ("Contexte".to_string(), Some(1), 1),
                ("Résultats".to_string(), Some(3), 0),
            ]
        );
    }

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
