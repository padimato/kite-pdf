use crate::annotations::{load_annotations_from_pdf, AnnotationManager};
use crate::memory_manager::LruTextureCache;
use crate::ocr_engine::{HybridOcrEngine, OcrSpeedMode, TabOcrState};
use crate::pdf_engine::{LoadedPdfDocument, PageDimensions, PdfEngine};
use crate::recent_files::{match_score, normalize, RecentFiles};
use crate::text_search::{PageTextLayer, TextSelectionState};
use crate::viewport::ViewportState;
use crate::worker::{BackgroundWorkers, OcrJob, OcrPageOutcome};
use anyhow::Result;
use egui::Color32;
use std::path::PathBuf;

pub const SIDEBAR_WIDTH: f32 = 268.0;
pub const HOVER_TRIGGER_EDGE_PX: f32 = 18.0;
/// Vitesse de glissement de la barre latérale (plus grand = plus vif), en 1/s.
const SIDEBAR_ANIM_SPEED: f32 = 16.0;
/// Pas de temps maximal pris en compte par image pour l'animation, en secondes.
const SIDEBAR_ANIM_MAX_STEP_S: f32 = 1.0 / 30.0;
pub const HOVER_KEEP_OPEN_PX: f32 = SIDEBAR_WIDTH + 18.0;

/// Outils disponibles dans la grille supérieure de la barre latérale (et activables à 1 touche)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTool {
    Select,
    Pan,
    Highlight,
    FreeText,
    Ink,
    Shape,
    StickyNote,
    Measure,
}

impl ActiveTool {
    pub fn all() -> &'static [ActiveTool] {
        &[
            ActiveTool::Select,
            ActiveTool::Pan,
            ActiveTool::Highlight,
            ActiveTool::FreeText,
            ActiveTool::Ink,
            ActiveTool::Shape,
            ActiveTool::StickyNote,
            ActiveTool::Measure,
        ]
    }

    pub fn icon_and_label(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            ActiveTool::Select => ("↖", "Sélection", "V"),
            ActiveTool::Pan => ("✋", "Main / Pan", "Espace"),
            ActiveTool::Highlight => ("✒", "Surligner", "H"),
            ActiveTool::FreeText => ("🅰", "Texte", "T"),
            ActiveTool::Ink => ("✏", "Dessin", "D"),
            ActiveTool::Shape => ("↗", "Flèche (Alt : rectangle)", "R"),
            ActiveTool::StickyNote => ("💬", "Note", "N"),
            ActiveTool::Measure => ("📏", "Mesure", "M"),
        }
    }
}

/// État de la barre latérale gauche escamotable
#[derive(Debug, Clone)]
pub struct SidebarState {
    /// Si vrai, la barre latérale est épinglée à gauche et décale le canevas PDF.
    /// Si faux, le PDF prend 100% de la largeur et la barre glisse en surimpression au survol du bord gauche.
    pub pinned: bool,
    /// Indique si la barre latérale auto-masquée est actuellement déployée par survol du trackpad
    pub hover_revealed: bool,
    /// Progression d'animation fluide entre 0.0 (masquée) et 1.0 (déployée)
    pub anim_progress: f32,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            pinned: true,
            hover_revealed: false,
            anim_progress: 1.0,
        }
    }
}

impl SidebarState {
    /// Bascule entre le mode Épinglé et le mode Auto-masqué (`Ctrl+B`)
    pub fn toggle_pinned(&mut self) {
        self.pinned = !self.pinned;
        if self.pinned {
            self.hover_revealed = false;
        }
    }

    /// Met à jour la détection du survol du bord gauche au Trackpad avec hystérésis
    /// pour éviter que la barre ne clignote quand on déplace le curseur vers les onglets.
    pub fn update_hover_hysteresis(&mut self, pointer_x: Option<f32>) {
        if self.pinned {
            self.hover_revealed = false;
            return;
        }
        if let Some(x) = pointer_x {
            if !self.hover_revealed && x <= HOVER_TRIGGER_EDGE_PX {
                self.hover_revealed = true;
            } else if self.hover_revealed && x > HOVER_KEEP_OPEN_PX {
                self.hover_revealed = false;
            }
        }
    }

    /// Indique si la barre latérale doit être visible (épinglée ou déployée au survol)
    pub fn is_expanded(&self) -> bool {
        self.pinned || self.hover_revealed
    }

    /// Fait avancer l'animation fluide d'ouverture/fermeture
    pub fn step_animation(&mut self, dt: f32) -> bool {
        let target = if self.is_expanded() { 1.0 } else { 0.0 };
        let diff = target - self.anim_progress;
        if diff.abs() < 0.005 {
            self.anim_progress = target;
            false
        } else {
            // Approche exponentielle indépendante de la cadence d'affichage. Le pas de temps est borné :
            // la première image après une période d'inactivité ne doit pas faire sauter la barre.
            let dt = dt.clamp(0.001, SIDEBAR_ANIM_MAX_STEP_S);
            let blend = 1.0 - (-SIDEBAR_ANIM_SPEED * dt).exp();
            self.anim_progress = (self.anim_progress + diff * blend).clamp(0.0, 1.0);
            true
        }
    }
}

/// Représente un onglet PDF avec support d'hibernation mémoire automatique.
/// Lorsqu'un onglet est en arrière-plan (`document == None`), son handle C++ PDFium et ses bitmaps
/// sont intégralement libérés de la RAM ; seules ses métadonnées légères (~200 octets) et sa position de lecture sont conservées.
pub struct DocumentTab {
    pub id: u64,
    pub path: PathBuf,
    pub title: String,
    pub page_dimensions: Vec<PageDimensions>,
    pub document: Option<LoadedPdfDocument<'static>>,
    pub text_layers: Vec<PageTextLayer>,
    /// Pages dont la couche de texte a déjà été demandée au thread d'arrière-plan
    text_requested: Vec<bool>,
    pub text_selection: TextSelectionState,
    pub annotations: AnnotationManager,
    pub ocr: TabOcrState,
    pub viewport: ViewportState,
    pub is_modified: bool,
    pub badge_color: Color32,
    /// Faux si le PDF contient un formulaire : ses champs n'apparaîtraient pas dans les tuiles
    pub supports_tiled_rendering: bool,
}

impl DocumentTab {
    pub fn new_active(
        id: u64,
        doc: LoadedPdfDocument<'static>,
        badge_color: Color32,
    ) -> Self {
        let path = doc.path.clone();
        let title = doc.title.clone();
        let page_dimensions = doc.page_dimensions.clone();
        let mut annotations = AnnotationManager::new();
        if doc.has_app_drawn_annotations {
            if let Ok(loaded_annots) = load_annotations_from_pdf(&path, &page_dimensions) {
                annotations.load_items(loaded_annots);
            }
        }
        let supports_tiled_rendering = !doc.has_form_fields();
        let page_count = page_dimensions.len();
        let mut ocr = TabOcrState::default();
        ocr.ensure_initialized(page_count);
        Self {
            id,
            path,
            title,
            page_dimensions,
            document: Some(doc),
            text_layers: Vec::new(),
            text_requested: vec![false; page_count],
            text_selection: TextSelectionState::default(),
            annotations,
            ocr,
            viewport: ViewportState::default(),
            is_modified: false,
            badge_color,
            supports_tiled_rendering,
        }
    }

    pub fn page_count(&self) -> usize {
        self.page_dimensions.len()
    }

    /// Scanne les objets `XObject Image` embarqués sur une page donnée (`page_index`) avec le moteur OCR Hybride Multi-Orientation.
    /// Retourne le nombre de nouveaux caractères OCR reconnus dans les images de la page.
    pub fn run_ocr_on_page(&mut self, page_index: usize, engine: &'static PdfEngine) -> usize {
        let total = self.page_dimensions.len();
        if page_index >= total {
            return 0;
        }
        self.ensure_page_text_layer(page_index, engine);
        self.ocr.ensure_initialized(total);
        if self.ocr.page_scanned[page_index] {
            return 0;
        }

        let _ = self.ensure_awake(engine);
        let Some(doc) = &self.document else {
            return 0;
        };
        let page_dim = self.page_dimensions[page_index];
        let layer = &mut self.text_layers[page_index];

        let (img_count, segments) =
            HybridOcrEngine::scan_page_embedded_images(doc, page_index, page_dim, layer);

        self.ocr.page_scanned[page_index] = true;
        self.ocr.detected_images_count += img_count;

        let added_chars: usize = segments.iter().map(|s| s.chars.len()).sum();
        if added_chars > 0 {
            self.ocr.recognized_ocr_chars_count += added_chars;
            self.ocr.recognized_segments_count += segments.len();
            self.ocr.has_unsaved_ocr = true;
            self.is_modified = true;
        }

        self.ocr.status_summary = format!(
            "OCR : {} img | {} seg. ({} car.)",
            self.ocr.detected_images_count,
            self.ocr.recognized_segments_count,
            self.ocr.recognized_ocr_chars_count
        );
        added_chars
    }

    /// Phase 2 de l'Ordonnanceur OCR (Mode Éco Tâche de fond) :
    /// Inspecte au maximum 1 page par tick autour de `visible_page` lorsque l'application est au repos,
    /// garantissant 0 lag et le maintien des 60 FPS.
    #[cfg(test)]
    pub fn step_eco_ocr_background(
        &mut self,
        visible_page: usize,
        engine: &'static PdfEngine,
    ) -> usize {
        let total = self.page_dimensions.len();
        if total == 0 {
            return 0;
        }
        self.ocr.ensure_initialized(total);

        let candidates = [
            Some(visible_page),
            visible_page.checked_add(1).filter(|&p| p < total),
            visible_page.checked_sub(1),
        ];

        for cand in candidates.into_iter().flatten() {
            if !self.ocr.page_scanned[cand] {
                if self.ocr.mode == OcrSpeedMode::InstantOpen {
                    self.ocr.mode = OcrSpeedMode::EcoBackground;
                }
                return self.run_ocr_on_page(cand, engine);
            }
        }
        0
    }

    /// Phase 3 de l'Ordonnanceur OCR (Mode Turbo déclenché par `Ctrl+F`) :
    /// Scanne en priorité la page active et les pages voisines comportant des images embarquées
    /// pour injecter immédiatement les textes des graphiques JPG (0°, +30°, -30°, +90°) dans `Ctrl+F`.
    pub fn run_turbo_ocr_for_search(
        &mut self,
        focus_page: usize,
        engine: &'static PdfEngine,
    ) -> usize {
        let total = self.page_dimensions.len();
        if total == 0 {
            return 0;
        }
        self.ocr.ensure_initialized(total);
        self.ocr.mode = OcrSpeedMode::TurboSearch;

        let mut total_added = 0;
        // Scanner d'abord la page courante
        if focus_page < total && !self.ocr.page_scanned[focus_page] {
            total_added += self.run_ocr_on_page(focus_page, engine);
        }

        for idx in 0..total {
            if !self.ocr.page_scanned[idx] {
                total_added += self.run_ocr_on_page(idx, engine);
            }
        }
        total_added
    }

    fn ensure_text_slots(&mut self) {
        let total = self.page_dimensions.len();
        if self.text_layers.len() < total {
            self.text_layers = (0..total)
                .map(|idx| PageTextLayer::new(idx, Vec::new()))
                .collect();
        }
        if self.text_requested.len() < total {
            self.text_requested.resize(total, false);
        }
    }

    /// Demande en arrière-plan la couche de texte d'une page, une seule fois.
    pub fn request_page_text(&mut self, page: usize, workers: &BackgroundWorkers, interactive: bool) {
        if page >= self.page_dimensions.len() {
            return;
        }
        self.ensure_text_slots();
        if !self.text_requested[page] {
            self.text_requested[page] = true;
            workers.request_text_layer(self.id, self.path.clone(), page, interactive);
        }
    }

    /// Demande l'indexation de tout le document pour la recherche.
    pub fn request_all_text(&mut self, workers: &BackgroundWorkers) {
        for page in 0..self.page_dimensions.len() {
            self.request_page_text(page, workers, false);
        }
    }

    /// Intègre une couche de texte reçue du thread d'arrière-plan.
    pub fn apply_text_layer(&mut self, layer: PageTextLayer) {
        self.ensure_text_slots();
        if let Some(slot) = self.text_layers.get_mut(layer.page_index) {
            if slot.chars.is_empty() {
                *slot = layer;
            }
        }
    }

    /// Bascule en mode Turbo (`Ctrl+F`) : l'OCR enchaîne les pages sans pause.
    pub fn start_turbo_ocr(&mut self) {
        self.ocr.ensure_initialized(self.page_dimensions.len());
        self.ocr.mode = OcrSpeedMode::TurboSearch;
    }

    /// Soumet au thread OCR la page non traitée la plus proche de `visible_page`.
    pub fn pump_ocr(&mut self, visible_page: usize, workers: &BackgroundWorkers) {
        let total = self.page_dimensions.len();
        if total == 0 || self.ocr.unavailable_reason.is_some() || self.ocr.in_flight_page.is_some() {
            return;
        }
        self.ocr.ensure_initialized(total);
        let visible = visible_page.min(total - 1);
        let next = (0..total)
            .flat_map(|d| [visible.checked_add(d), visible.checked_sub(d)])
            .flatten()
            .find(|&p| p < total && !self.ocr.page_scanned[p]);
        let Some(page) = next else {
            return;
        };
        if self.ocr.mode == OcrSpeedMode::InstantOpen {
            self.ocr.mode = OcrSpeedMode::EcoBackground;
        }
        let submitted = workers.submit_ocr(OcrJob {
            tab_id: self.id,
            path: self.path.clone(),
            page,
            eco: self.ocr.mode != OcrSpeedMode::TurboSearch,
        });
        if submitted {
            self.ocr.in_flight_page = Some(page);
        }
    }

    /// Intègre le résultat OCR d'une page. Retourne `true` si la couche de texte a changé.
    pub fn apply_ocr_outcome(&mut self, outcome: OcrPageOutcome) -> bool {
        self.ensure_text_slots();
        self.ocr.ensure_initialized(self.page_dimensions.len());
        self.ocr.in_flight_page = None;
        let page = outcome.page;
        if page >= self.page_dimensions.len() {
            return false;
        }
        if let Some(reason) = outcome.error {
            self.ocr.status_summary = format!("OCR indisponible : {reason}");
            self.ocr.unavailable_reason = Some(reason);
            return false;
        }

        let mut changed = false;
        if let Some(native) = outcome.native_layer {
            if self.text_layers[page].chars.is_empty() {
                changed = !native.chars.is_empty();
                self.text_layers[page] = native;
            }
            self.text_requested[page] = true;
        }
        self.ocr.page_scanned[page] = true;
        self.ocr.detected_images_count += outcome.images_found;

        if !outcome.segments.is_empty() {
            HybridOcrEngine::merge_segments_into_layer(&mut self.text_layers[page], &outcome.segments);
            self.ocr.recognized_ocr_chars_count +=
                outcome.segments.iter().map(|s| s.chars.len()).sum::<usize>();
            self.ocr.recognized_segments_count += outcome.segments.len();
            self.ocr.has_unsaved_ocr = true;
            self.is_modified = true;
            changed = true;
        }

        let scanned = self.ocr.scanned_pages_count();
        self.ocr.status_summary = format!(
            "OCR p.{}/{} : {} img, {} lignes",
            scanned,
            self.page_dimensions.len(),
            self.ocr.detected_images_count,
            self.ocr.recognized_segments_count
        );
        changed
    }

    /// Extrait à la demande la couche de texte d'une seule page (`page_index`) en < 1 ms,
    /// permettant d'interagir instantanément avec de très gros manuels techniques (ex: CR711S de plusieurs centaines de pages).
    pub fn ensure_page_text_layer(&mut self, page_index: usize, engine: &'static PdfEngine) {
        let total = self.page_dimensions.len();
        if page_index >= total {
            return;
        }
        if self.text_layers.len() < total {
            self.text_layers = (0..total)
                .map(|idx| PageTextLayer::new(idx, Vec::new()))
                .collect();
        }
        if !self.text_layers[page_index].chars.is_empty() {
            return;
        }
        let _ = self.ensure_awake(engine);
        if let Some(doc) = &self.document {
            self.text_layers[page_index] = doc.extract_single_page_text_layer(page_index);
        }
    }

    /// Extrait à la demande la couche de texte natif des pages (jusqu'à 120 pages en mode direct)
    pub fn ensure_text_layers(&mut self, engine: &'static PdfEngine) {
        let total = self.page_dimensions.len();
        if self.text_layers.len() < total {
            self.text_layers = (0..total)
                .map(|idx| PageTextLayer::new(idx, Vec::new()))
                .collect();
        }
        let _ = self.ensure_awake(engine);
        if let Some(doc) = &self.document {
            let max_scan = total.min(120);
            for idx in 0..max_scan {
                if self.text_layers[idx].chars.is_empty() {
                    self.text_layers[idx] = doc.extract_single_page_text_layer(idx);
                }
            }
        }
    }

    /// Indique si l'onglet est actuellement en hibernation mémoire (0 Mo de handle PDFium / textures)
    pub fn is_hibernated(&self) -> bool {
        self.document.is_none()
    }

    /// Met l'onglet en hibernation : libère le handle C++ PDFium et évince ses textures du cache LRU
    pub fn hibernate(&mut self, cache: &mut LruTextureCache) -> usize {
        {
            let _lock = PdfEngine::lock_guard();
            self.document = None;
        }
        cache.evict_tab(self.id)
    }

    /// Réveille l'onglet instantanément s'il était hiberné en rechargeant le document PDFium à la volée
    pub fn ensure_awake(&mut self, engine: &'static PdfEngine) -> Result<()> {
        if self.document.is_none() {
            let doc = engine.load_document(&self.path)?;
            self.page_dimensions = doc.page_dimensions.clone();
            self.document = Some(doc);
        }
        Ok(())
    }
}

/// Action exécutable depuis la barre centrale (`Ctrl+T` / `Ctrl+K`)
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteCommand {
    SwitchTab(usize),
    OpenRecent(PathBuf),
    ToggleSidebarPin,
    OpenFileDialog,
    OpenSearch,
    SaveAnnotatedPdf,
    RunHybridOcrTurbo,
    HibernateInactiveTabs,
    UndoAnnotation,
    RedoAnnotation,
    DeleteSelectedAnnotation,
    CloseActiveTab,
    SelectTool(ActiveTool),
    FitToWidth,
    FitToPage,
    ZoomReset100,
    JumpToPage(usize),
}

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub category: &'static str,
    pub label: String,
    /// Ligne secondaire (dossier d'un PDF récent, explication d'une action)
    pub detail: String,
    pub shortcut: &'static str,
    pub command: PaletteCommand,
}

/// État de la barre centrale de recherche (`Ctrl+T`, aussi `Ctrl+K` / `Ctrl+P`)
#[derive(Debug, Clone, Default)]
pub struct CommandPaletteState {
    pub is_open: bool,
    pub query: String,
    pub selected_index: usize,
    pub focus_requested: bool,
}

/// Action proposée par la barre centrale, avec les mots qui permettent de la retrouver
/// (« comment », « stabilo », « sauvegarder »… et pas seulement son libellé exact).
struct ActionSpec {
    category: &'static str,
    label: String,
    detail: &'static str,
    keywords: &'static str,
    shortcut: &'static str,
    command: PaletteCommand,
}

impl ActionSpec {
    fn new(
        category: &'static str,
        label: &str,
        detail: &'static str,
        keywords: &'static str,
        shortcut: &'static str,
        command: PaletteCommand,
    ) -> Self {
        Self { category, label: label.to_string(), detail, keywords, shortcut, command }
    }

    fn into_entry(self) -> PaletteEntry {
        PaletteEntry {
            category: self.category,
            label: self.label,
            detail: self.detail.to_string(),
            shortcut: self.shortcut,
            command: self.command,
        }
    }
}

fn tool_keywords(tool: ActiveTool) -> (&'static str, &'static str) {
    match tool {
        ActiveTool::Select => ("Sélectionner et copier du texte, déplacer une annotation", "selection selectionner curseur copier texte souris"),
        ActiveTool::Pan => ("Faire défiler le document à la main", "main deplacer glisser defiler pan naviguer"),
        ActiveTool::Highlight => ("Surligner un passage du texte", "surligner surligneur stabilo highlight marquer fluo souligner"),
        ActiveTool::FreeText => ("Écrire une zone de texte sur la page", "texte ecrire taper zone saisie machine a ecrire"),
        ActiveTool::Ink => ("Dessiner à main levée", "dessin dessiner crayon stylo main levee gribouiller signer"),
        ActiveTool::Shape => ("Tracer une flèche ou un rectangle", "fleche rectangle forme cadre encadrer pointer"),
        ActiveTool::StickyNote => ("Ajouter un commentaire (note) sur la page", "commentaire commenter comment note bulle annoter remarque post it memo"),
        ActiveTool::Measure => ("Mesurer une distance sur la page", "mesure mesurer regle distance longueur"),
    }
}

fn action_specs() -> Vec<ActionSpec> {
    use PaletteCommand as C;
    let mut actions = vec![
        ActionSpec::new("Fichier", "Ouvrir un PDF depuis le disque…", "Choisir un fichier dans l'explorateur", "ouvrir fichier parcourir explorateur disque importer", "Ctrl+O", C::OpenFileDialog),
        ActionSpec::new("Fichier", "Enregistrer le PDF", "Annotations et texte reconnu par l'OCR inclus", "enregistrer sauvegarder save garder", "Ctrl+S", C::SaveAnnotatedPdf),
        ActionSpec::new("Document", "Rechercher dans le PDF", "Trouver un mot dans le document ouvert", "rechercher chercher trouver find mot", "Ctrl+F", C::OpenSearch),
        ActionSpec::new("Document", "Reconnaître le texte des images (OCR)", "Rend cherchable le texte des figures et des scans", "ocr reconnaissance texte image scanner scan figure", "OCR", C::RunHybridOcrTurbo),
        ActionSpec::new("Annotations", "Annuler", "Défaire la dernière modification d'annotation", "annuler undo defaire retour arriere", "Ctrl+Z", C::UndoAnnotation),
        ActionSpec::new("Annotations", "Rétablir", "Refaire la modification annulée", "retablir redo refaire", "Ctrl+Y", C::RedoAnnotation),
        ActionSpec::new("Annotations", "Supprimer l'annotation sélectionnée", "", "supprimer effacer enlever retirer delete", "Suppr", C::DeleteSelectedAnnotation),
        ActionSpec::new("Onglet", "Fermer l'onglet actif", "", "fermer onglet close", "Ctrl+W", C::CloseActiveTab),
        ActionSpec::new("Affichage", "Ajuster à la largeur", "", "ajuster largeur zoom fit", "Ctrl+9", C::FitToWidth),
        ActionSpec::new("Affichage", "Afficher la page entière", "", "ajuster page entiere hauteur zoom fit", "Ctrl+0", C::FitToPage),
        ActionSpec::new("Affichage", "Zoom à 100 %", "", "zoom 100 taille reelle reinitialiser", "100%", C::ZoomReset100),
        ActionSpec::new("Affichage", "Épingler ou masquer la barre latérale", "", "barre laterale epingler masquer cacher plein ecran", "Ctrl+B", C::ToggleSidebarPin),
        ActionSpec::new("Mémoire", "Libérer la mémoire des onglets inactifs", "", "memoire ram purger liberer hiberner", "Purge", C::HibernateInactiveTabs),
    ];
    for tool in ActiveTool::all() {
        let (_, name, key) = tool.icon_and_label();
        let (detail, keywords) = tool_keywords(*tool);
        actions.push(ActionSpec::new("Outil", &format!("Outil {name}"), detail, keywords, key, C::SelectTool(*tool)));
    }
    actions
}

/// « 12 », « p12 », « page 12 », « aller a la page 12 » → page 12.
fn parse_page_request(normalized_query: &str) -> Option<usize> {
    let rest = normalized_query
        .trim_start_matches("aller a la")
        .trim_start_matches("aller")
        .trim()
        .trim_start_matches("page")
        .trim_start_matches('p')
        .trim();
    rest.parse::<usize>().ok().filter(|p| *p >= 1)
}

fn paths_match(a: &std::path::Path, b: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .map(|c| c.to_string_lossy().trim_start_matches(r"\\?\").to_lowercase())
            .unwrap_or_else(|_| p.to_string_lossy().to_lowercase())
    };
    canon(a) == canon(b)
}

impl CommandPaletteState {
    pub fn open(&mut self) {
        self.is_open = true;
        self.query.clear();
        self.selected_index = 0;
        self.focus_requested = true;
    }

    pub fn close(&mut self) {
        self.is_open = false;
        self.focus_requested = false;
    }

    pub fn toggle(&mut self) {
        if self.is_open {
            self.close();
        } else {
            self.open();
        }
    }

    /// Résultats pour la requête courante : page, onglets ouverts, PDF de l'historique et actions,
    /// classés par pertinence. Sans requête : les PDF récents, puis « Ouvrir un PDF… ».
    pub fn filtered_entries(
        &self,
        tabs_info: &[(usize, String, usize, PathBuf)],
        active_tab_page_count: usize,
        recent: &RecentFiles,
        now: u64,
    ) -> Vec<PaletteEntry> {
        let query = normalize(&self.query);
        let mut scored: Vec<(f32, PaletteEntry)> = Vec::new();

        if let Some(page) = parse_page_request(&query) {
            if page <= active_tab_page_count {
                scored.push((100.0, PaletteEntry {
                    category: "Page",
                    label: format!("Aller à la page {page}"),
                    detail: format!("sur {active_tab_page_count}"),
                    shortcut: "Entrée",
                    command: PaletteCommand::JumpToPage(page - 1),
                }));
            }
        }

        const TAB_SHORTCUTS: [&str; 8] = ["Ctrl+1", "Ctrl+2", "Ctrl+3", "Ctrl+4", "Ctrl+5", "Ctrl+6", "Ctrl+7", "Ctrl+8"];
        if !query.is_empty() {
            for (idx, title, pages, path) in tabs_info {
                let folder = normalize(&path.to_string_lossy());
                if let Some(score) = match_score(&query, &normalize(title), &folder) {
                    scored.push((score + 1.5, PaletteEntry {
                        category: "Onglet",
                        label: crate::recent_files::decode_percent(title),
                        detail: format!("Déjà ouvert · {pages} pages"),
                        shortcut: TAB_SHORTCUTS.get(*idx).copied().unwrap_or("Ctrl+Tab"),
                        command: PaletteCommand::SwitchTab(*idx),
                    }));
                }
            }
        }

        let max_recent = if query.is_empty() { 8 } else { 12 };
        for (score, file) in recent.search(&self.query, now).into_iter().take(max_recent) {
            let open_idx = tabs_info.iter().find(|t| paths_match(&t.3, &file.path)).map(|t| t.0);
            if open_idx.is_some() && !query.is_empty() {
                continue; // déjà proposé comme onglet ouvert
            }
            scored.push((score, PaletteEntry {
                category: "Récent",
                label: file.file_name(),
                detail: file.folder_label(),
                shortcut: if open_idx.is_some() { "ouvert" } else { "Entrée" },
                command: match open_idx {
                    Some(i) => PaletteCommand::SwitchTab(i),
                    None => PaletteCommand::OpenRecent(file.path.clone()),
                },
            }));
        }

        for action in action_specs() {
            if query.is_empty() {
                if action.command == PaletteCommand::OpenFileDialog {
                    scored.push((-1.0, action.into_entry()));
                }
                continue;
            }
            let label = normalize(&action.label);
            let secondary = format!("{} {} {}", normalize(action.category), normalize(action.detail), action.keywords);
            if let Some(score) = match_score(&query, &label, &secondary) {
                // Une action passe devant un PDF au nom proche : « comment » vise l'outil Note.
                scored.push((score + 0.5, action.into_entry()));
            }
        }

        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.into_iter().map(|(_, e)| e).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_engine::ensure_custom_sample_pdf;
    use egui::Vec2;

    #[test]
    fn test_sidebar_autohide_trackpad_hover_hysteresis() {
        let mut sidebar = SidebarState::default();
        assert!(sidebar.pinned);
        assert!(sidebar.is_expanded());

        sidebar.toggle_pinned();
        assert!(!sidebar.pinned);
        assert!(!sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(500.0));
        assert!(!sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(8.0));
        assert!(sidebar.hover_revealed);
        assert!(sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(180.0));
        assert!(sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(340.0));
        assert!(!sidebar.hover_revealed);
        assert!(!sidebar.is_expanded());
    }

    #[test]
    fn test_command_palette_filtering_tabs_and_tools() {
        let mut palette = CommandPaletteState::default();
        palette.open();

        let tabs = vec![
            (0, "Architecture_Rust.pdf".to_string(), 5, PathBuf::from("Architecture_Rust.pdf")),
            (1, "Spécification_ISO_32000.pdf".to_string(), 8, PathBuf::from("Spécification_ISO_32000.pdf")),
        ];
        let recent = RecentFiles::in_memory();
        let results = |palette: &CommandPaletteState| palette.filtered_entries(&tabs, 5, &recent, 0);

        palette.query = "specification".to_string();
        assert_eq!(results(&palette)[0].command, PaletteCommand::SwitchTab(1));

        // Les mots du langage courant mènent à l'outil, pas seulement son nom exact.
        for (query, tool) in [
            ("comment", ActiveTool::StickyNote),
            ("ajouter un commentaire", ActiveTool::StickyNote),
            ("stabilo", ActiveTool::Highlight),
            ("surligner", ActiveTool::Highlight),
            ("fleche", ActiveTool::Shape),
        ] {
            palette.query = query.to_string();
            assert_eq!(results(&palette)[0].command, PaletteCommand::SelectTool(tool), "{query}");
        }

        palette.query = "sauvegarder".to_string();
        assert_eq!(results(&palette)[0].command, PaletteCommand::SaveAnnotatedPdf);

        palette.query = "page 3".to_string();
        assert_eq!(results(&palette)[0].command, PaletteCommand::JumpToPage(2));
    }

    #[test]
    fn test_15_pdf_tabs_simultaneous_hibernation_and_instant_wakeup() {
        let engine = PdfEngine::instance();
        let mut cache = LruTextureCache::default();
        let mut tabs: Vec<DocumentTab> = Vec::with_capacity(15);

        // Ouverture de 15 fichiers PDF distincts en simultané
        for i in 1..=15 {
            let path = PathBuf::from(format!("test_pdfs/bench_15_tab_{:02}.pdf", i));
            ensure_custom_sample_pdf(
                &path,
                &format!("DOCUMENT BENCHMARK #{:02}", i),
                (0.15, 0.30, 0.55),
                6,
            )
            .expect("Doit générer le PDF");

            let doc = engine.load_document(&path).expect("Doit charger le PDF");
            let mut tab = DocumentTab::new_active(i as u64, doc, Color32::WHITE);
            tab.viewport.zoom = 1.0 + (i as f32) * 0.05;
            tab.viewport.scroll_offset = Vec2::new(10.0 * i as f32, 40.0 * i as f32);

            // Hiberner tous les onglets précédents, seul le dernier ouvert reste actif
            for prev in tabs.iter_mut() {
                prev.hibernate(&mut cache);
            }
            tabs.push(tab);
        }

        assert_eq!(tabs.len(), 15);
        let active_count = tabs.iter().filter(|t| !t.is_hibernated()).count();
        let hibernated_count = tabs.iter().filter(|t| t.is_hibernated()).count();
        assert_eq!(active_count, 1, "Seul l'onglet actif doit garder un handle PDFium en RAM");
        assert_eq!(hibernated_count, 14, "Les 14 onglets en arrière-plan doivent être hibernés");

        // Bascule sur l'onglet #7 (index 6) : réveil instantané et vérification de la conservation du zoom/scroll
        tabs[14].hibernate(&mut cache);
        tabs[6].ensure_awake(engine).expect("Doit réveiller l'onglet #7");
        assert!(!tabs[6].is_hibernated());
        assert!((tabs[6].viewport.zoom - 1.35).abs() < 1e-4);
        assert!((tabs[6].viewport.scroll_offset.y - 280.0).abs() < 1e-4);
    }
}
