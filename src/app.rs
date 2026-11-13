use crate::annotations::{
    FormEdit,
    line_ending_outline, line_stop_point, save_pdf_with_ocr_state, unit_direction,
    AnnotHandle, AnnotationKind, LineEnding, PdfAnnotation, ShapeKind, ARROW_LINE_WIDTH,
    default_author, format_pdf_date, pdf_date_now, CommentReply, ReviewState,
    FREETEXT_DEFAULT_FILL, FREETEXT_TEXT_RGB, PALETTE_COLORS, REVIEW_STATES,
};
use crate::memory_manager::{
    LruTextureCache, MultiplicativeHighlightQuad, PageTextureKey,
    DEFAULT_TEXTURE_BUDGET_BYTES,
};
use crate::pdf_engine::{PdfEngine, TileRegion};
use crate::recent_files::RecentFiles;
use crate::tabs::{
    ActiveTool, CommandPaletteState, DocumentTab, PaletteCommand, SidebarState, SIDEBAR_WIDTH_RANGE,
};
use crate::text_search::{PdfTopDownRect, SearchBarState, SelectionGestureMode};
use crate::viewport::BASE_DPI_SCALE;
use crate::worker::{BackgroundWorkers, RenderJob, WorkerEvent};
use egui::{
    Align2, Color32, Context, CornerRadius, FontId, Key, Margin, Order, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2, ViewportCommand,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::os::windows::process::CommandExt;
use std::time::{Duration, Instant};

/// Délai minimal entre deux recherches relancées par l'arrivée de nouvelles pages.
const SEARCH_REFRESH_INTERVAL: Duration = Duration::from_millis(150);
/// Part du budget de textures utilisable par les pages visibles et préchargées.
const VISIBLE_TEXTURE_BUDGET_RATIO: f32 = 0.7;
/// Précision du palier de zoom : assez fine pour que le rendu tombe pixel pour pixel sur l'écran.
const ZOOM_BUCKETS_PER_UNIT: f32 = 1000.0;
/// Délai sans changement de zoom avant de recalculer les pages à la nouvelle résolution.
const ZOOM_SETTLE_DELAY: Duration = Duration::from_millis(120);
/// Au-delà de cette taille (pixels écran), une page passe en rendu par tuiles.
const TILING_THRESHOLD_PIXELS: f32 = 2_600_000.0;
/// Taille de l'image de fond d'une page rendue par tuiles, affichée en attendant les tuiles.
const TILED_BASE_PIXELS: f32 = 1_200_000.0;
const TILE_SIZE_PX: i32 = 512;
/// Part du cache que les tuiles peuvent occuper ; au-delà, la marge de préchargement est retirée.
const TILE_BUDGET_RATIO: f32 = 0.85;
/// Rayon de saisie des poignées d'annotation, en pixels écran.
const HANDLE_GRAB_RADIUS_PX: f32 = 9.0;
/// Délai pendant lequel un nouveau tracé complète le dessin précédent (signature en plusieurs traits).
const INK_STROKE_GROUPING: Duration = Duration::from_millis(2500);

type Rgb = (u8, u8, u8);

const FREETEXT_FILL_CHOICES: [(&str, Rgb); 8] = [
    ("Blanc", (255, 255, 255)),
    ("Crème", FREETEXT_DEFAULT_FILL),
    ("Gris clair", (232, 234, 238)),
    ("Jaune pâle", (255, 243, 176)),
    ("Bleu pâle", (205, 228, 252)),
    ("Vert pâle", (205, 240, 220)),
    ("Rouge pâle", (250, 212, 212)),
    ("Violet pâle", (232, 214, 250)),
];

const FREETEXT_BORDER_CHOICES: [(&str, Rgb); 7] = [
    ("Noir", (0, 0, 0)),
    ("Gris", (140, 146, 158)),
    PALETTE_COLORS[0],
    PALETTE_COLORS[1],
    PALETTE_COLORS[2],
    PALETTE_COLORS[3],
    PALETTE_COLORS[4],
];

const INLINE_TEXT_COLOR: Color32 = Color32::from_rgb(20, 24, 34);
/// Largeur d'une bulle de commentaire : constante à l'écran, comme dans Acrobat.
const COMMENT_PANEL_WIDTH: f32 = 300.0;
const COMMENT_PANEL_BACKGROUND: Color32 = Color32::from_rgb(255, 255, 255);
const COMMENT_MUTED_TEXT: Color32 = Color32::from_rgb(105, 112, 125);
/// Zone sensible de l'ascenseur, plus large que son trait pour être facile à attraper.
const SCROLLBAR_HIT_WIDTH: f32 = 16.0;
const SCROLLBAR_MARGIN: f32 = 12.0;
const FREETEXT_DEFAULT_FONT_SIZE: f32 = 12.0;
const FREETEXT_FONT_SIZE_RANGE: (f32, f32) = (4.0, 144.0);
const FREETEXT_FONT_SIZES: [f32; 8] = [8.0, 10.0, 12.0, 14.0, 18.0, 24.0, 36.0, 48.0];

/// Géométrie de l'ascenseur vertical
struct ScrollbarGeometry {
    hit_area: Rect,
    track_top: f32,
    track_height: f32,
    thumb_top: f32,
    thumb_height: f32,
    max_scroll: f32,
}

impl ScrollbarGeometry {
    fn compute(canvas_rect: Rect, total_height: f32, scroll_y: f32) -> Option<Self> {
        let viewport_height = canvas_rect.height();
        if total_height <= viewport_height {
            return None;
        }
        let track_top = canvas_rect.min.y + SCROLLBAR_MARGIN;
        let track_height = viewport_height - 2.0 * SCROLLBAR_MARGIN;
        let thumb_height = track_height * (viewport_height / total_height).clamp(0.06, 1.0);
        let max_scroll = total_height - viewport_height;
        let progress = (scroll_y / max_scroll).clamp(0.0, 1.0);
        Some(Self {
            hit_area: Rect::from_min_max(
                Pos2::new(canvas_rect.max.x - SCROLLBAR_HIT_WIDTH, track_top),
                Pos2::new(canvas_rect.max.x, track_top + track_height),
            ),
            track_top,
            track_height,
            thumb_top: track_top + progress * (track_height - thumb_height),
            thumb_height,
            max_scroll,
        })
    }

    fn contains_thumb_y(&self, y: f32) -> bool {
        y >= self.thumb_top && y <= self.thumb_top + self.thumb_height
    }

    fn scroll_for_thumb_top(&self, thumb_top: f32) -> f32 {
        let free_travel = (self.track_height - self.thumb_height).max(1.0);
        ((thumb_top - self.track_top) / free_travel).clamp(0.0, 1.0) * self.max_scroll
    }
}

/// Partie d'un commentaire en cours de modification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentEditTarget {
    Main,
    Reply(usize),
}

/// Action choisie dans le menu contextuel d'un onglet
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabMenuAction {
    RevealInExplorer,
    OpenWithDefaultApp,
    CloseThis,
    CloseOthers,
    CloseBelow,
}

/// Choix du corps de texte. Retourne la nouvelle taille quand l'utilisateur en choisit une.
fn font_size_row(ui: &mut egui::Ui, current: f32) -> Option<f32> {
    let mut picked = None;
    let (min_size, max_size) = FREETEXT_FONT_SIZE_RANGE;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.label(egui::RichText::new("Taille").size(10.5));
        if ui.small_button("−").clicked() {
            picked = Some((current - 1.0).round().max(min_size));
        }
        ui.label(egui::RichText::new(format!("{current:.0} pt")).size(10.5).strong());
        if ui.small_button("+").clicked() {
            picked = Some((current + 1.0).round().min(max_size));
        }
        for size in FREETEXT_FONT_SIZES {
            let label = egui::RichText::new(format!("{size:.0}")).size(10.5);
            if ui.selectable_label((current - size).abs() < 0.05, label).clicked() {
                picked = Some(size);
            }
        }
    });
    picked
}

/// Position de la bulle à l'écran : sa place enregistrée, ou à côté de l'icône d'une note libre.
fn comment_panel_origin(
    annot: &PdfAnnotation,
    page_rect: Rect,
    page_dim: crate::pdf_engine::PageDimensions,
) -> Pos2 {
    let screen = annot.rect.to_screen_rect(page_rect, page_dim);
    if annot.is_fixed_to_text() {
        screen.min
    } else {
        screen.right_top() + Vec2::new(6.0, 0.0)
    }
}

fn review_state_label(state: &str) -> &'static str {
    REVIEW_STATES
        .iter()
        .find(|(value, _)| *value == state)
        .map_or("", |(_, label)| *label)
}

/// Ligne de choix « Aucun + nuancier ». Retourne le nouveau choix quand l'utilisateur en fait un.
fn color_choice_row(
    ui: &mut egui::Ui,
    title: &str,
    current: Option<Rgb>,
    choices: &[(&str, Rgb)],
) -> Option<Option<Rgb>> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.label(egui::RichText::new(title).size(10.5));
        if ui.selectable_label(current.is_none(), "Aucun").clicked() {
            picked = Some(None);
        }
        for (name, rgb) in choices {
            let swatch = egui::Button::new(
                egui::RichText::new(if current == Some(*rgb) { "✔" } else { " " })
                    .size(10.0)
                    .color(Color32::from_rgb(90, 96, 110)),
            )
            .fill(Color32::from_rgb(rgb.0, rgb.1, rgb.2))
            .min_size(Vec2::new(17.0, 16.0));
            if ui.add(swatch).on_hover_text(*name).clicked() {
                picked = Some(Some(*rgb));
            }
        }
    });
    picked
}

/// Tuile à afficher pour une page
struct PageTile {
    page_index: usize,
    key: PageTextureKey,
    region: TileRegion,
    full_size_px: (f32, f32),
    distance_to_center: f32,
    is_visible: bool,
}

/// Geste en cours sur une annotation
struct AnnotationDrag {
    id: u64,
    handle: AnnotHandle,
    grab_pt: (f32, f32),
    original: PdfAnnotation,
    has_moved: bool,
}

/// Largeur de la zone à saisir pour étirer la barre latérale.
const SIDEBAR_HANDLE_PX: f32 = 8.0;
/// À partir de cette largeur de barre latérale, les outils tiennent sur une seule rangée.
const SIDEBAR_ONE_ROW_TOOLS_WIDTH: f32 = 430.0;

/// Écart laissé au-dessus d'un titre atteint depuis le sommaire.
const OUTLINE_TARGET_MARGIN_PX: f32 = 16.0;
/// Hauteur du séparateur réglable entre la liste des onglets et le tiroir du bas.
const DRAWER_SPLITTER_PX: f32 = 8.0;
/// Hauteur de la barre du haut (disposition portrait)
const TOP_BAR_HEIGHT: f32 = 44.0;
/// Boutons de la barre du haut
const TOP_BAR_BUTTON: Vec2 = Vec2::new(30.0, 28.0);
/// Largeur minimale du bouton des onglets avant de replier les outils, puis le zoom, dans un menu
const TOP_BAR_MIN_TABS_WIDTH: f32 = 150.0;

/// Intervalle d'enregistrement de la session (endroit de lecture, fenêtre, barre latérale).
const SESSION_SAVE_INTERVAL: Duration = Duration::from_secs(3);
/// Durée par défaut d'une page d'OCR approfondi, avant la première mesure (page dense, CPU).
const DEFAULT_DEEP_OCR_SECONDS_PER_PAGE: f32 = 6.0;
/// Durée d'affichage de la bulle « page sans texte ».
const NO_TEXT_HINT_DURATION: Duration = Duration::from_secs(6);

/// Lecture OCR approfondie lancée sur un onglet.
struct DeepOcrRun {
    tab_id: u64,
    progress: std::sync::Arc<std::sync::Mutex<crate::worker::DeepOcrProgress>>,
    /// Le message de fin a été vu (enregistré ou écarté)
    acknowledged: bool,
}

/// Ligne d'information sous la recherche.
enum SearchOcrHint {
    PagesWithoutText(usize),
    Running { done: usize, total: usize },
}

/// Pages lues de part et d'autre de la page affichée par le choix « autour de celle-ci ».
const DEEP_OCR_PAGES_AROUND: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeepOcrScope {
    CurrentPage,
    Around,
    WholeDocument,
}

/// Fenêtre de l'OCR approfondi (la lecture commence par `current_page`).
struct DeepOcrDialog {
    scope: DeepOcrScope,
    current_page: usize,
}

/// Saisie en cours dans un champ texte de formulaire.
struct FormEditor {
    tab_id: u64,
    page: usize,
    name: String,
    rect: PdfTopDownRect,
    buffer: String,
    multiline: bool,
    focus_requested: bool,
}

/// Liste déroulante ouverte sur un champ à choix.
struct FormChoicePopup {
    tab_id: u64,
    page: usize,
    name: String,
    rect: PdfTopDownRect,
    options: Vec<String>,
}

/// Impression lancée depuis l'application.
struct PrintJobState {
    progress: std::sync::Arc<std::sync::Mutex<crate::print::PrintProgress>>,
    document: String,
    acknowledged: bool,
}

/// Demande du mot de passe d'un PDF protégé.
struct PasswordPrompt {
    path: PathBuf,
    input: String,
    wrong: bool,
    focus_requested: bool,
}

/// Bulle « cette page n'a pas de texte » après une tentative de sélection.
struct NoTextHint {
    page: usize,
    position: Pos2,
    shown_at: Instant,
}

/// « ≈ 45 s », « ≈ 12 min », « ≈ 1 h 05 ».
fn format_duration(seconds: f32) -> String {
    let seconds = seconds.max(1.0).round() as u64;
    if seconds < 60 {
        format!("≈ {seconds} s")
    } else if seconds < 3600 {
        format!("≈ {} min", (seconds + 30) / 60)
    } else {
        format!("≈ {} h {:02}", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Menu contextuel ouvert sur une flèche
struct LineContextMenu {
    annotation_id: u64,
    position: Pos2,
    opened_on_frame: u64,
}

/// Mode initial de la barre latérale pour les tests automatisés QA
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InitialSidebarMode {
    #[default]
    Pinned,
    AutoHideHidden,
    AutoHideHover,
}

/// Configuration d'automatisation (utilisée par les sous-agents QA pour les tests visuels et captures d'écran)
#[derive(Debug, Clone, Default)]
pub struct AutomationConfig {
    pub initial_pdf: Option<PathBuf>,
    pub extra_pdfs: Vec<PathBuf>,
    pub initial_zoom: Option<f32>,
    pub initial_page: Option<usize>,
    pub initial_scroll_x: Option<f32>,
    pub initial_scroll_y: Option<f32>,
    pub active_tab_index: Option<usize>,
    pub sidebar_mode: InitialSidebarMode,
    pub open_palette_query: Option<String>,
    pub initial_tool: Option<ActiveTool>,
    pub initial_search_query: Option<String>,
    pub initial_match_index: Option<usize>,
    pub initial_select_rect: Option<PdfTopDownRect>,
    pub initial_select_stream: Option<(usize, usize)>,
    pub initial_highlight_stream: Option<(usize, usize)>,
    pub ocr_turbo: bool,
    /// Ouvre la fenêtre de l'OCR approfondi au démarrage (captures QA)
    pub deep_ocr_dialog: bool,
    /// Sommaire ouvert au démarrage (captures QA)
    pub outline_open: bool,
    /// Largeur de la barre latérale au démarrage (captures QA)
    pub sidebar_width: Option<f32>,
    /// Entrée du sommaire « cliquée » au démarrage, à partir de 0 (captures QA)
    pub outline_entry: Option<usize>,
    /// Valeurs saisies dans des champs au démarrage, `nom=valeur` (« oui »/« non » : case à cocher)
    pub form_fills: Vec<(String, String)>,
    /// Imprime le document ouvert dans ce fichier via « Microsoft Print to PDF » (vérifications)
    pub print_to_pdf: Option<PathBuf>,
    /// Mot de passe du PDF ouvert au démarrage (captures QA)
    pub initial_password: Option<String>,
    /// Lance l'OCR approfondi de tout le document au démarrage (captures QA)
    pub deep_ocr_all: bool,
    /// Montre l'écran d'accueil du premier lancement (captures QA)
    pub show_welcome: bool,
    /// Montre l'aide-mémoire des raccourcis (captures QA)
    pub show_shortcuts: bool,
    /// Montre la page Réglages (captures QA)
    pub show_settings: bool,
    /// Lance « Rechercher une mise à jour » au démarrage (captures QA)
    pub check_update_now: bool,
    pub save_annotated_output: Option<PathBuf>,
    pub screenshot_output: Option<PathBuf>,
    /// Délai avant la capture (permet à un script QA d'envoyer des événements à la fenêtre)
    pub screenshot_delay_ms: u64,
    /// Capture automatisée dans une fenêtre visible au premier plan (sinon hors écran, sans focus).
    pub screenshot_visible: bool,
    /// Taille de la fenêtre de capture, `--window-size LxH` (ex. écran portrait)
    pub window_size: Option<[f32; 2]>,
    /// Facteur de zoom de l'interface, `--ui-zoom` (simule un écran plus grand que celui de la machine)
    pub ui_zoom: Option<f32>,
    /// Ouvre la liste des onglets de la barre du haut au démarrage (captures QA)
    pub open_tabs_menu: bool,
}

pub struct KiteApp {
    /// Moteur PDFium singleton
    engine: &'static PdfEngine,
    /// Liste des onglets PDF ouverts dans la barre verticale (avec support d'hibernation)
    pub tabs: Vec<DocumentTab>,
    /// Index de l'onglet actuellement affiché
    pub active_tab: usize,
    /// Prochain identifiant unique d'onglet
    next_tab_id: u64,
    /// État de la barre latérale gauche escamotable
    pub sidebar: SidebarState,
    /// Outil actif (Sélection, Main, Surligneur, Texte, Dessin, Formes, Note, Mesure)
    pub active_tool: ActiveTool,
    /// État de la palette de commandes flottante (`Ctrl+K`)
    pub command_palette: CommandPaletteState,
    /// Historique des PDF ouverts, cherché par la barre centrale (`Ctrl+T`)
    pub recent_files: RecentFiles,
    /// Usage récent d'un nombre seul dans la barre centrale : page ou zoom
    number_history: crate::tabs::NumberHistory,
    /// Réglages gardés entre deux lancements (aucun en capture automatisée)
    settings: Option<crate::settings::Settings>,
    /// Préférences en vigueur (celles du fichier de réglages, ou par défaut pour une capture automatisée)
    prefs: crate::settings::Preferences,
    /// Page Réglages ouverte
    settings_open: bool,
    /// « Effacer l'historique » cliqué une première fois : attend la confirmation
    confirm_clear_history: bool,
    /// Dernière position et taille connues de la fenêtre non agrandie, et si elle est agrandie
    last_window: Option<[f32; 4]>,
    last_maximized: bool,
    /// Dernier enregistrement de la session (fait régulièrement, pas seulement à la fermeture :
    /// une application arrêtée de force ne perd ainsi presque rien)
    last_session_save: Instant,
    /// Titre affiché dans la barre de titre et la barre des tâches (document actif)
    window_title: String,
    /// Fichiers des onglets fermés, du plus ancien au plus récent (`Ctrl+Maj+T`)
    closed_tabs: Vec<PathBuf>,
    /// Dépendances installées dans une version non testée (rempli par un thread au démarrage)
    dependency_warnings: std::sync::Arc<std::sync::Mutex<Option<Vec<crate::deps::UntestedVersion>>>>,
    dependency_warning_closed: bool,
    /// Bandeau « nouvelle version » fermé pour cette session
    update_banner_closed: bool,
    /// Commandes dans une barre en haut (écran portrait) plutôt que dans la barre latérale
    top_layout: bool,
    /// Liste des onglets à ouvrir dès la première image (captures QA)
    open_tabs_menu_pending: bool,
    /// Logo affiché en haut de la barre latérale
    logo: Option<egui::TextureHandle>,
    /// Écran d'accueil du premier lancement affiché
    welcome_open: bool,
    /// Aide-mémoire des raccourcis affiché (`F1`)
    shortcuts_open: bool,
    first_render_reported: bool,
    /// Dernière page vue en cours de lecture des images (onglet, page), pour l'afficher entre deux pages
    last_image_ocr_page: Option<(u64, usize)>,
    /// Lecture OCR approfondie en cours ou dont le résultat n'a pas encore été vu
    deep_ocr: Option<DeepOcrRun>,
    deep_ocr_dialog: Option<DeepOcrDialog>,
    no_text_hint: Option<NoTextHint>,
    password_prompt: Option<PasswordPrompt>,
    /// Le sommaire remplace la liste des onglets dans la barre latérale
    outline_open: bool,
    /// Impression en cours ou dont le résultat n'a pas encore été vu
    print_job: Option<PrintJobState>,
    form_editor: Option<FormEditor>,
    form_choice: Option<FormChoicePopup>,
    /// Téléchargement de l'OCR en cours, s'il manquait au démarrage
    ocr_install: Option<crate::setup::InstallJob>,
    /// Taille de la zone du document à l'image précédente (navigation depuis la barre latérale)
    canvas_size: Vec2,
    /// État du moteur de recherche `Ctrl+F`
    pub search_bar: SearchBarState,
    /// Gestionnaire de cache de textures GPU LRU à budget mémoire plafonné (48 Mo max)
    pub texture_cache: LruTextureCache,
    /// Déplacement ou redimensionnement d'annotation en cours
    annotation_drag: Option<AnnotationDrag>,
    /// Bloc FreeText en cours d'édition directement sur la page
    editing_freetext_id: Option<u64>,
    freetext_focus_pending: bool,
    /// Taille choisie pour les prochains blocs de texte
    freetext_font_size: f32,
    /// Saisie de l'ascenseur : décalage du pointeur par rapport au haut de la poignée
    scrollbar_grab: Option<f32>,
    pointer_on_scrollbar: bool,
    /// Commentaire dont la bulle est ouverte
    open_comment_id: Option<u64>,
    comment_edit: Option<(u64, CommentEditTarget)>,
    comment_focus_pending: bool,
    comment_reply_focus_pending: bool,
    comment_reply_draft: String,
    /// Dernier style choisi : les blocs suivants le reprennent (`None` tant que rien n'a été choisi)
    freetext_style: Option<(Option<Rgb>, Option<Rgb>)>,
    /// Un champ de saisie avait le focus à l'image précédente : seules ces images bloquent les raccourcis à une touche
    /// (`wants_keyboard_input` serait aussi vrai pour un simple bouton atteint avec Tab).
    text_field_focused_prev: bool,
    text_field_focused_now: bool,
    /// Le bouton qui a créé le bloc est encore enfoncé : son relâchement ne doit pas fermer l'éditeur
    freetext_awaiting_opening_release: bool,
    line_context_menu: Option<LineContextMenu>,
    /// Dernier dessin créé et instant de sa fin, pour regrouper les traits d'une signature
    last_ink_stroke: Option<(u64, Instant)>,
    /// Dernière erreur éventuelle à afficher
    error_message: Option<String>,
    /// Échec du dernier enregistrement, signalé dans une fenêtre sans masquer le document
    save_error: Option<String>,
    /// Compteur de frames pour le mode capture d'écran automatisée
    frame_count: u64,
    /// Indique si la commande Screenshot a déjà été envoyée
    screenshot_requested: bool,
    /// Configuration d'automatisation QA
    automation: AutomationConfig,
    /// Mesure du temps de frame / FPS
    last_frame_instant: Instant,
    /// Threads de rendu, de texte et d'OCR
    workers: BackgroundWorkers,
    /// Pages dont le rendu a échoué : on ne les redemande pas à chaque image
    failed_renders: HashSet<PageTextureKey>,
    /// De nouvelles pages sont arrivées depuis la dernière recherche
    search_refresh_pending: bool,
    last_search_refresh: Instant,
    app_started: Instant,
    /// Zoom réel (échelle Windows incluse) à la résolution duquel les pages sont rendues
    render_zoom: f32,
    /// Dernier zoom observé et instant de son changement, pour attendre la fin d'un geste
    observed_zoom: f32,
    observed_zoom_changed_at: Instant,
}

const TAB_BADGE_COLORS: [(Color32, (f32, f32, f32)); 6] = [
    (Color32::from_rgb(66, 135, 245), (0.10, 0.24, 0.48)),
    (Color32::from_rgb(46, 184, 138), (0.12, 0.42, 0.34)),
    (Color32::from_rgb(186, 85, 211), (0.42, 0.16, 0.38)),
    (Color32::from_rgb(235, 145, 55), (0.52, 0.28, 0.10)),
    (Color32::from_rgb(225, 85, 105), (0.48, 0.14, 0.20)),
    (Color32::from_rgb(80, 185, 225), (0.10, 0.36, 0.46)),
];

impl KiteApp {
    pub fn new(egui_ctx: &Context, automation: AutomationConfig) -> Self {
        let engine = PdfEngine::instance();

        let mut sidebar = SidebarState::default();
        match automation.sidebar_mode {
            InitialSidebarMode::Pinned => {
                sidebar.pinned = true;
                sidebar.hover_revealed = false;
                sidebar.anim_progress = 1.0;
            }
            InitialSidebarMode::AutoHideHidden => {
                sidebar.pinned = false;
                sidebar.hover_revealed = false;
                sidebar.anim_progress = 0.0;
            }
            InitialSidebarMode::AutoHideHover => {
                sidebar.pinned = false;
                sidebar.hover_revealed = true;
                sidebar.anim_progress = 1.0;
            }
        }

        if crate::settings::persistence_enabled(automation.screenshot_output.is_some())
            && automation.sidebar_mode == InitialSidebarMode::Pinned
        {
            let saved = crate::settings::Settings::load();
            if let Some(pinned) = saved.sidebar_pinned {
                sidebar.pinned = pinned;
                sidebar.anim_progress = if pinned { 1.0 } else { 0.0 };
            }
            if let Some(width) = saved.sidebar_width {
                sidebar.width = width.clamp(*SIDEBAR_WIDTH_RANGE.start(), *SIDEBAR_WIDTH_RANGE.end());
            }
            if let Some(height) = saved.drawer_height {
                sidebar.drawer_height = height.max(80.0);
            }
        }

        let mut app = Self {
            engine,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            sidebar,
            active_tool: automation.initial_tool.unwrap_or(ActiveTool::Select),
            command_palette: CommandPaletteState::default(),
            // Les lancements automatisés (captures QA) ne doivent pas remplir l'historique réel.
            closed_tabs: Vec::new(),
            settings: crate::settings::persistence_enabled(automation.screenshot_output.is_some())
                .then(crate::settings::Settings::load),
            last_window: None,
            last_maximized: false,
            last_session_save: Instant::now(),
            window_title: String::new(),
            number_history: if crate::settings::persistence_enabled(automation.screenshot_output.is_some()) {
                crate::tabs::NumberHistory::load_default()
            } else {
                crate::tabs::NumberHistory::default()
            },
            canvas_size: Vec2::new(1000.0, 800.0),
            recent_files: if crate::settings::persistence_enabled(automation.screenshot_output.is_some()) {
                RecentFiles::load_default()
            } else {
                RecentFiles::in_memory()
            },
            search_bar: SearchBarState::default(),
            texture_cache: LruTextureCache::new(DEFAULT_TEXTURE_BUDGET_BYTES),
            annotation_drag: None,
            editing_freetext_id: None,
            freetext_focus_pending: false,
            freetext_font_size: FREETEXT_DEFAULT_FONT_SIZE,
            scrollbar_grab: None,
            pointer_on_scrollbar: false,
            open_comment_id: None,
            comment_edit: None,
            comment_focus_pending: false,
            comment_reply_focus_pending: false,
            comment_reply_draft: String::new(),
            freetext_style: None,
            text_field_focused_prev: false,
            text_field_focused_now: false,
            freetext_awaiting_opening_release: false,
            line_context_menu: None,
            last_ink_stroke: None,
            error_message: None,
            save_error: None,
            frame_count: 0,
            screenshot_requested: false,
            automation: automation.clone(),
            last_frame_instant: Instant::now(),
            workers: BackgroundWorkers::spawn(egui_ctx.clone(), engine),
            dependency_warnings: crate::setup::check_versions_in_background(egui_ctx.clone()),
            dependency_warning_closed: false,
            update_banner_closed: false,
            top_layout: false,
            open_tabs_menu_pending: false,
            logo: load_logo(egui_ctx),
            welcome_open: false,
            settings_open: automation.show_settings,
            confirm_clear_history: false,
            prefs: crate::settings::Preferences::default(),
            shortcuts_open: automation.show_shortcuts,
            first_render_reported: false,
            last_image_ocr_page: None,
            deep_ocr: None,
            deep_ocr_dialog: None,
            no_text_hint: None,
            password_prompt: None,
            outline_open: false,
            print_job: None,
            form_editor: None,
            form_choice: None,
            // OCR manquant (PDFium présent) : téléchargé en tâche de fond, l'application reste utilisable.
            ocr_install: {
                let missing: Vec<_> = crate::deps::missing().into_iter().filter(|d| !d.required).collect();
                (automation.screenshot_output.is_none() && !missing.is_empty())
                    .then(|| crate::setup::InstallJob::start(missing, egui_ctx.clone()))
            },
            failed_renders: HashSet::new(),
            search_refresh_pending: false,
            last_search_refresh: Instant::now(),
            app_started: Instant::now(),
            render_zoom: 0.0,
            observed_zoom: 0.0,
            observed_zoom_changed_at: Instant::now(),
        };

        if let Some(initial) = &automation.initial_pdf {
            if let Some(password) = &automation.initial_password {
                crate::pdf_engine::set_document_password(initial, password);
            }
            app.open_pdf_in_new_tab(initial);
        }
        for extra in &automation.extra_pdfs {
            app.open_pdf_in_new_tab(extra);
        }
        if let Some(settings) = &app.settings {
            app.prefs = settings.prefs.clone();
        }
        if automation.check_update_now {
            crate::updates::check_now(egui_ctx.clone());
        }
        // Premier lancement : quelques gestes pour bien démarrer.
        app.welcome_open = automation.show_welcome || app.settings.as_ref().is_some_and(|s| !s.welcome_seen);
        app.open_tabs_menu_pending = automation.open_tabs_menu;
        // Lancé sans fichier : la barre centrale propose directement les PDF récents.
        if app.tabs.is_empty()
            && !app.welcome_open
            && automation.open_palette_query.is_none()
            && app.password_prompt.is_none()
        {
            app.command_palette.open();
        }

        if let Some(target_tab) = automation.active_tab_index {
            if !app.tabs.is_empty() {
                let idx = target_tab.saturating_sub(1).min(app.tabs.len() - 1);
                app.switch_to_tab(idx);
            }
        } else if !app.tabs.is_empty() {
            let current = app.active_tab;
            app.switch_to_tab(current);
        }

        if let Some(tab) = app.tabs.get_mut(app.active_tab) {
            if let Some(z) = automation.initial_zoom {
                tab.viewport.zoom = z;
                tab.viewport.last_gesture_label = format!("Zoom initial {:.0}%", z * 100.0);
            }
            if let Some(p) = automation.initial_page {
                let target_idx = p.saturating_sub(1);
                let pages = tab.page_dimensions.clone();
                tab.viewport
                    .jump_to_page(target_idx, Vec2::new(1000.0, 800.0), &pages);
            }
            if let Some(sx) = automation.initial_scroll_x {
                tab.viewport.scroll_offset.x = sx;
            }
            if let Some(sy) = automation.initial_scroll_y {
                tab.viewport.scroll_offset.y = sy;
            }
        }

        if let Some(q) = &automation.open_palette_query {
            app.command_palette.open();
            app.command_palette.query = q.clone();
        }

        app.outline_open = automation.outline_open;
        if let Some(width) = automation.sidebar_width {
            app.sidebar.width = width.clamp(*SIDEBAR_WIDTH_RANGE.start(), *SIDEBAR_WIDTH_RANGE.end());
        }
        if let Some(index) = automation.outline_entry {
            let target = app.tabs.get_mut(app.active_tab).and_then(|tab| {
                let outline = tab.document.as_ref().map(|doc| doc.outline_with_positions()).unwrap_or_default();
                let entry = outline.get(index).cloned();
                tab.outline = Some(outline);
                entry
            });
            if let Some(entry) = target {
                if let Some(page) = entry.page {
                    let command = match entry.y {
                        Some(y) => PaletteCommand::JumpToPageAt(page, y),
                        None => PaletteCommand::JumpToPage(page),
                    };
                    app.execute_palette_command(command, Vec2::new(1000.0, 800.0));
                }
            }
        }
        if let Some(tab) = app.tabs.get_mut(app.active_tab) {
            for (name, value) in &automation.form_fills {
                let edit = match value.as_str() {
                    "oui" => FormEdit::Checked(true),
                    "non" => FormEdit::Checked(false),
                    text => FormEdit::Text(text.to_string()),
                };
                tab.set_form_edit(name, edit);
            }
        }
        if let Some(output) = &automation.print_to_pdf {
            app.print_active_tab(Some(output.clone()));
        }
        if automation.deep_ocr_dialog {
            app.open_deep_ocr_dialog(Vec2::new(1000.0, 800.0));
        }
        if automation.deep_ocr_all {
            if let Some(tab) = app.tabs.get(app.active_tab) {
                let pages = tab.deep_ocr_pending_pages(0..=tab.page_count().saturating_sub(1));
                app.start_deep_ocr(pages, 0);
            }
        }

        if automation.ocr_turbo {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_p = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.run_turbo_ocr_for_search(target_p, app.engine);
            }
        }

        if let Some(sq) = &automation.initial_search_query {
            app.search_bar.open();
            app.search_bar.query = sq.clone();
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_p = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.ensure_text_layers(app.engine);
                tab.ensure_page_text_layer(target_p, app.engine);
                // Phase 3 : Mode Turbo OCR automatique sur Ctrl+F pour trouver aussi les textes des graphiques JPG
                tab.run_turbo_ocr_for_search(target_p, app.engine);
                app.search_bar.execute_search(&tab.text_layers);
                if let Some(m_idx) = automation.initial_match_index {
                    if !app.search_bar.matches.is_empty() {
                        app.search_bar.current_match_index =
                            m_idx.saturating_sub(1).min(app.search_bar.matches.len() - 1);
                    }
                } else if let Some(p) = automation.initial_page {
                    let target_p = p.saturating_sub(1);
                    if let Some(pos) = app
                        .search_bar
                        .matches
                        .iter()
                        .position(|m| m.page_index == target_p)
                    {
                        app.search_bar.current_match_index = pos;
                    }
                }
            }
        }

        // Sélection rectangulaire initiale (Alt + Glisser)
        if let Some(sel_rect) = automation.initial_select_rect {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_page = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.ensure_page_text_layer(target_page, app.engine);
                if let Some(layer) = tab.text_layers.get(target_page) {
                    let (txt, boxes) = layer.select_in_rect(&sel_rect);
                    tab.text_selection.page_index = Some(target_page);
                    tab.text_selection.mode = SelectionGestureMode::RectangularBlock;
                    tab.text_selection.drag_start_pt = Some((sel_rect.x_min, sel_rect.y_min));
                    tab.text_selection.drag_current_pt = Some((sel_rect.x_max, sel_rect.y_max));
                    tab.text_selection.selected_text = txt;
                    tab.text_selection.highlight_boxes = boxes;
                }
            }
        }

        // Sélection au fil des phrases initiale (Curseur I)
        if let Some((c_start, c_end)) = automation.initial_select_stream {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_page = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.ensure_page_text_layer(target_page, app.engine);
                if let Some(layer) = tab.text_layers.get(target_page) {
                    let (txt, boxes) = layer.select_char_stream(c_start, c_end);
                    tab.text_selection.page_index = Some(target_page);
                    tab.text_selection.mode = SelectionGestureMode::StreamIBeam;
                    tab.text_selection.anchor_char_index = Some(c_start);
                    tab.text_selection.current_char_index = Some(c_end);
                    tab.text_selection.selected_text = txt;
                    tab.text_selection.highlight_boxes = boxes;
                }
            }
        }

        // Création d'un surlignage ISO 32000 (/Highlight /BM /Multiply) sur un flux de caractères
        if let Some((h_start, h_end)) = automation.initial_highlight_stream {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_page = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.ensure_page_text_layer(target_page, app.engine);
                if let Some(layer) = tab.text_layers.get(target_page) {
                    let (_, quads) = layer.select_char_stream(h_start, h_end);
                    if let Some(first) = quads.first() {
                        let union_rect = quads.iter().fold(*first, |acc, q| acc.union(q));
                        tab.annotations.add_annotation(PdfAnnotation {
                            meta: Default::default(),
                            id: 0,
                            page_index: target_page,
                            rect: union_rect,
                            color_rgb: (255, 226, 52),
                            kind: AnnotationKind::Highlight { quads },
                        });
                        tab.is_modified = true;
                    }
                }
            }
        }

        // Sauvegarde du PDF (Annotations ISO 32000 + Couche Texte OCR invisible `3 Tr`) si demandée par CLI
        if let Some(save_path) = &automation.save_annotated_output {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let _ = save_pdf_with_ocr_state(
                    &tab.path,
                    save_path,
                    &tab.annotations.items,
                    &tab.text_layers,
                    &tab.page_dimensions,
                    &tab.ocr.page_scanned,
                    &tab.ocr.deep_done,
                    &tab.form_edit_list(),
                );
                tab.annotations.saved_feedback_frames = 120;
                tab.ocr.has_unsaved_ocr = false;
            }
        }

        app
    }

    pub fn switch_to_tab(&mut self, target_index: usize) {
        if target_index >= self.tabs.len() {
            return;
        }
        if target_index != self.active_tab {
            self.remember_reading(self.active_tab);
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != target_index {
                tab.hibernate(&mut self.texture_cache);
                self.workers.forget_tab(tab.id);
            }
        }
        if let Some(active) = self.tabs.get_mut(target_index) {
            if let Err(err) = active.ensure_awake(self.engine) {
                self.error_message = Some(format!("{:#}", err));
            } else {
                self.error_message = None;
            }
            if self.search_bar.is_open && !self.search_bar.query.trim().is_empty() {
                active.request_all_text(&self.workers);
                active.start_turbo_ocr();
                self.search_bar.execute_search(&active.text_layers);
            }
        }
        self.active_tab = target_index;
        self.render_zoom = 0.0;
    }

    pub fn open_pdf_in_new_tab(&mut self, path: &Path) {
        let color_idx = self.tabs.len() % TAB_BADGE_COLORS.len();
        let (badge_color, _) = TAB_BADGE_COLORS[color_idx];

        for existing in &mut self.tabs {
            existing.hibernate(&mut self.texture_cache);
            self.workers.forget_tab(existing.id);
        }

        match self.engine.load_document(path) {
            Ok(doc) => {
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                let tab = DocumentTab::new_active(id, doc, badge_color);
                // Gros document : affiché tout de suite, détails des pages lus ensuite.
                if !tab.page_details_complete() {
                    self.workers.scan_page_details(id, tab.path.clone(), tab.page_count());
                }
                let reading = self.prefs.resume_reading.then(|| self.recent_files.reading_for(path)).flatten();
                self.tabs.push(tab);
                if self.prefs.keep_history {
                    self.recent_files.record_open(path);
                }
                if let Some(tab) = self.tabs.last_mut() {
                    let pages = tab.page_dimensions.clone();
                    match reading {
                        Some((page, zoom)) => {
                            tab.viewport.zoom = zoom.clamp(0.1, 32.0);
                            tab.viewport.jump_to_page(page.min(pages.len().saturating_sub(1)), self.canvas_size, &pages);
                        }
                        None => match self.prefs.open_zoom {
                            crate::settings::OpenZoom::Actual => {}
                            crate::settings::OpenZoom::FitWidth => tab.viewport.fit_to_width(self.canvas_size, &pages),
                            crate::settings::OpenZoom::FitPage => tab.viewport.fit_to_page(self.canvas_size, &pages),
                        },
                    }
                }
                self.active_tab = self.tabs.len() - 1;
                self.error_message = None;
                self.render_zoom = 0.0;
            }
            Err(err) => {
                if let Some(required) = err.downcast_ref::<crate::pdf_engine::PasswordRequired>() {
                    self.command_palette.close();
                    self.password_prompt = Some(PasswordPrompt {
                        path: path.to_path_buf(),
                        input: String::new(),
                        wrong: required.wrong_password,
                        focus_requested: true,
                    });
                } else {
                    self.error_message = Some(format!("{:#}", err));
                }
            }
        }
    }

    /// Sommaire de l'onglet actif (tiroir du bas). Retourne la page et la hauteur choisies.
    fn render_outline(&mut self, ui: &mut egui::Ui, max_height: f32) -> Option<(usize, Option<f32>)> {
        let tab = self.tabs.get_mut(self.active_tab)?;
        if tab.outline.is_none() && !tab.outline_requested {
            tab.outline_requested = true;
            self.workers.load_outline(tab.id, tab.path.clone());
        }
        // Endroit lu : page et hauteur (points) du haut de l'écran, un peu en dessous du bord.
        let (layouts, _, _) = tab.viewport.compute_layout(&tab.page_dimensions);
        let reading_y = tab.viewport.scroll_offset.y + OUTLINE_TARGET_MARGIN_PX + 4.0;
        let reading = layouts
            .iter()
            .rev()
            .find(|l| l.y_top <= reading_y)
            .map(|l| {
                let height_pt = tab.page_dimensions.get(l.page_index).map_or(1.0, |d| d.height_pt);
                (l.page_index, (reading_y - l.y_top) / l.height.max(1.0) * height_pt)
            })
            .unwrap_or((0, 0.0));
        let entries = tab.outline.as_deref().unwrap_or(&[]);
        // Partie en cours de lecture : la dernière dont le titre est au-dessus de l'endroit lu.
        let current_entry = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.page.map(|p| (i, p, e.y.unwrap_or(0.0))))
            .filter(|&(_, p, y)| p < reading.0 || (p == reading.0 && y <= reading.1 + 1.0))
            .max_by(|a, b| (a.1, a.2, a.0).partial_cmp(&(b.1, b.2, b.0)).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _, _)| i);
        let muted = Color32::from_rgb(140, 150, 172);
        let mut chosen = None;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SOMMAIRE").size(10.5).strong().color(muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✖").on_hover_text("Fermer le sommaire").clicked() {
                    self.outline_open = false;
                }
            });
        });
        egui::ScrollArea::vertical()
            .id_salt("outline_scroll_area")
            .max_height(max_height - 24.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if tab.outline.is_none() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(11.0));
                        ui.label(egui::RichText::new("Lecture du sommaire…").size(11.5).color(muted));
                    });
                } else if entries.is_empty() {
                    ui.label(egui::RichText::new("Ce document n'a pas de sommaire.").size(11.5).color(muted));
                }
                ui.spacing_mut().item_spacing.y = 1.0;
                for (index, entry) in entries.iter().enumerate() {
                    let is_current = current_entry == Some(index);
                    ui.horizontal(|ui| {
                        ui.add_space(entry.depth.min(6) as f32 * 12.0);
                        let text = egui::RichText::new(&entry.title)
                            .size(if entry.depth == 0 { 12.0 } else { 11.5 })
                            .color(if is_current { Color32::WHITE } else { Color32::from_rgb(190, 198, 215) });
                        let label = egui::Button::new(text).frame(false).wrap().selected(is_current);
                        let response = ui.add_enabled(entry.page.is_some(), label);
                        let response = match entry.page {
                            Some(page) => response.on_hover_text(format!("Page {}", page + 1)),
                            None => response,
                        };
                        if response.clicked() {
                            chosen = entry.page.map(|p| (p, entry.y));
                        }
                    });
                }
            });
        chosen
    }

    /// Champs de formulaire de l'onglet actif : clic dans un champ (outil Sélection).
    /// Retourne `true` si le clic a été pris par un champ.
    fn handle_form_interaction(&mut self, ctx: &Context, page_index: usize, page_rect: Rect) -> bool {
        if self.active_tool != ActiveTool::Select || self.command_palette.is_open {
            return false;
        }
        let (hover, pressed) = ctx.input(|i| (i.pointer.hover_pos(), i.pointer.primary_pressed()));
        let Some(pointer) = hover else { return false };
        if !page_rect.contains(pointer) || ctx.is_pointer_over_area() {
            return false;
        }
        let Some(tab) = self.tabs.get_mut(self.active_tab) else { return false };
        let Some(dims) = tab.page_dimensions.get(page_index).copied() else { return false };
        let Some(widgets) = tab.form_widgets.get(page_index).and_then(|w| w.as_ref()) else { return false };
        let Some(widget) = widgets
            .iter()
            .find(|w| !w.read_only && w.rect.to_screen_rect(page_rect, dims).contains(pointer))
            .cloned()
        else {
            return false;
        };
        ctx.set_cursor_icon(match widget.kind {
            crate::pdf_engine::FormWidgetKind::Text { .. } => egui::CursorIcon::Text,
            _ => egui::CursorIcon::PointingHand,
        });
        if !pressed {
            return false;
        }
        let (value, checked) = tab.form_value(&widget);
        match &widget.kind {
            crate::pdf_engine::FormWidgetKind::Text { multiline } => {
                self.form_editor = Some(FormEditor {
                    tab_id: tab.id,
                    page: page_index,
                    name: widget.name.clone(),
                    rect: widget.rect,
                    buffer: value,
                    multiline: *multiline,
                    focus_requested: true,
                });
            }
            crate::pdf_engine::FormWidgetKind::Checkbox => {
                tab.set_form_edit(&widget.name, FormEdit::Checked(!checked));
            }
            crate::pdf_engine::FormWidgetKind::Radio { index } => {
                tab.set_form_edit(&widget.name, FormEdit::Radio(*index));
            }
            crate::pdf_engine::FormWidgetKind::Choice { options } => {
                self.form_choice = Some(FormChoicePopup {
                    tab_id: tab.id,
                    page: page_index,
                    name: widget.name.clone(),
                    rect: widget.rect,
                    options: options.clone(),
                });
            }
        }
        tab.text_selection.clear();
        true
    }

    /// Repère les champs à remplir (fond bleuté, comme dans Acrobat) et affiche les valeurs saisies
    /// pas encore enregistrées par-dessus le rendu de PDFium.
    fn paint_form_fields(&self, painter: &egui::Painter, visible_pages: &[(usize, Rect)]) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        for (page_index, page_rect) in visible_pages {
            let Some(dims) = tab.page_dimensions.get(*page_index).copied() else { continue };
            let Some(widgets) = tab.form_widgets.get(*page_index).and_then(|w| w.as_ref()) else { continue };
            for widget in widgets.iter().filter(|w| !w.read_only) {
                let rect = widget.rect.to_screen_rect(*page_rect, dims);
                let editing = self.form_editor.as_ref().is_some_and(|e| e.name == widget.name && e.page == *page_index);
                if !tab.form_edits.contains_key(&widget.name) {
                    painter.rect_filled(rect, CornerRadius::same(1), Color32::from_rgba_unmultiplied(70, 110, 255, 60));
                    continue;
                }
                if editing {
                    continue;
                }
                let (value, checked) = tab.form_value(widget);
                painter.rect_filled(rect, CornerRadius::same(1), Color32::from_rgb(236, 242, 255));
                let ink = Color32::from_rgb(20, 24, 32);
                match &widget.kind {
                    crate::pdf_engine::FormWidgetKind::Checkbox => {
                        if checked {
                            let r = rect.shrink(rect.height() * 0.2);
                            let points = vec![
                                Pos2::new(r.left(), r.center().y),
                                Pos2::new(r.left() + r.width() * 0.38, r.bottom()),
                                Pos2::new(r.right(), r.top()),
                            ];
                            painter.add(egui::Shape::line(points, Stroke::new((rect.height() * 0.12).max(1.5), ink)));
                        }
                    }
                    crate::pdf_engine::FormWidgetKind::Radio { .. } => {
                        if checked {
                            painter.circle_filled(rect.center(), rect.height().min(rect.width()) * 0.25, ink);
                        }
                    }
                    _ => {
                        let font_size = (rect.height() * 0.62).clamp(6.0, 18.0);
                        let galley = painter.layout(
                            value,
                            FontId::proportional(font_size),
                            ink,
                            if matches!(widget.kind, crate::pdf_engine::FormWidgetKind::Text { multiline: true }) {
                                rect.width() - 4.0
                            } else {
                                f32::INFINITY
                            },
                        );
                        let top = if galley.size().y < rect.height() {
                            rect.center().y - galley.size().y * 0.5
                        } else {
                            rect.top()
                        };
                        painter.with_clip_rect(rect).galley(Pos2::new(rect.left() + 2.0, top), galley, ink);
                    }
                }
            }
        }
    }

    /// Saisie d'un champ texte, posée sur le champ lui-même.
    fn render_form_editor(&mut self, ctx: &Context, visible_pages: &[(usize, Rect)]) {
        let Some(editor) = self.form_editor.as_mut() else { return };
        let Some(tab) = self.tabs.iter().find(|t| t.id == editor.tab_id) else {
            self.form_editor = None;
            return;
        };
        let Some((_, page_rect)) = visible_pages.iter().find(|(p, _)| *p == editor.page) else { return };
        let Some(dims) = tab.page_dimensions.get(editor.page).copied() else { return };
        let rect = editor.rect.to_screen_rect(*page_rect, dims);
        let font_size = (rect.height() * 0.62).clamp(9.0, 18.0);
        let mut commit = false;
        let mut cancel = false;
        // Tab / Maj+Tab : champ texte suivant / précédent (le champ garde le focus, egui ne le
        // donne pas aux boutons de la barre latérale).
        let mut move_to: Option<i32> = None;
        egui::Area::new(egui::Id::new("kite_form_editor"))
            .fixed_pos(rect.min)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let field = if editor.multiline {
                    egui::TextEdit::multiline(&mut editor.buffer)
                } else {
                    egui::TextEdit::singleline(&mut editor.buffer)
                };
                let tab_step = ui.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Key { key: Key::Tab, pressed: true, modifiers, .. } => {
                            Some(if modifiers.shift { -1 } else { 1 })
                        }
                        _ => None,
                    })
                });
                let response = ui.add_sized(
                    rect.size().max(Vec2::new(40.0, 18.0)),
                    field
                        .lock_focus(true)
                        .font(FontId::proportional(font_size))
                        .text_color(Color32::from_rgb(20, 24, 32))
                        .background_color(Color32::from_rgb(236, 242, 255))
                        .margin(Margin::symmetric(2, 1)),
                );
                if editor.focus_requested {
                    response.request_focus();
                    editor.focus_requested = false;
                }
                self.text_field_focused_now |= response.has_focus();
                let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                if let Some(step) = tab_step {
                    editor.buffer.retain(|c| c != '\t');
                    commit = true;
                    move_to = Some(step);
                } else if escape {
                    cancel = true;
                } else if response.lost_focus() || (enter && !editor.multiline) {
                    commit = true;
                }
            });
        if commit {
            let (tab_id, name, value) = (editor.tab_id, editor.name.clone(), editor.buffer.clone());
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.set_form_edit(&name, FormEdit::Text(value));
            }
        }
        if commit || cancel {
            let current = self.form_editor.take();
            if let (Some(step), Some(current)) = (move_to, current) {
                self.open_neighbour_form_field(&current, step);
            }
        }
    }

    /// Ouvre le champ texte suivant (`step` = 1) ou précédent (-1) dans l'ordre de la page, puis des
    /// pages suivantes, et l'amène à l'écran s'il n'y est pas.
    fn open_neighbour_form_field(&mut self, current: &FormEditor, step: i32) {
        let Some(tab_index) = self.tabs.iter().position(|t| t.id == current.tab_id) else { return };
        let tab = &mut self.tabs[tab_index];
        for page in 0..tab.page_count() {
            tab.ensure_form_widgets(page, self.engine);
        }
        let fields: Vec<(usize, crate::pdf_engine::FormWidget)> = tab
            .form_widgets
            .iter()
            .enumerate()
            .filter_map(|(page, widgets)| widgets.as_ref().map(|w| (page, w)))
            .flat_map(|(page, widgets)| widgets.iter().map(move |w| (page, w.clone())))
            .filter(|(_, w)| !w.read_only && matches!(w.kind, crate::pdf_engine::FormWidgetKind::Text { .. }))
            .collect();
        let Some(position) = fields
            .iter()
            .position(|(page, w)| *page == current.page && w.name == current.name && w.rect == current.rect)
        else {
            return;
        };
        let next = position as i64 + step as i64;
        let Some((page, widget)) = usize::try_from(next).ok().and_then(|n| fields.get(n)).cloned() else {
            return;
        };
        let (value, _) = tab.form_value(&widget);
        let tab_id = tab.id;
        if page != current.page {
            let canvas = self.canvas_size;
            self.execute_palette_command(PaletteCommand::JumpToPageAt(page, (widget.rect.y_min - 40.0).max(0.0)), canvas);
        }
        self.form_editor = Some(FormEditor {
            tab_id,
            page,
            name: widget.name.clone(),
            rect: widget.rect,
            buffer: value,
            multiline: matches!(widget.kind, crate::pdf_engine::FormWidgetKind::Text { multiline: true }),
            focus_requested: true,
        });
    }

    /// Liste déroulante d'un champ à choix.
    fn render_form_choice(&mut self, ctx: &Context, visible_pages: &[(usize, Rect)]) {
        let Some(popup) = self.form_choice.as_ref() else { return };
        let Some(tab) = self.tabs.iter().find(|t| t.id == popup.tab_id) else {
            self.form_choice = None;
            return;
        };
        let Some((_, page_rect)) = visible_pages.iter().find(|(p, _)| *p == popup.page) else { return };
        let Some(dims) = tab.page_dimensions.get(popup.page).copied() else { return };
        let rect = popup.rect.to_screen_rect(*page_rect, dims);
        let mut chosen = None;
        let area = egui::Area::new(egui::Id::new("kite_form_choice"))
            .fixed_pos(Pos2::new(rect.left(), rect.bottom() + 2.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(26, 30, 42))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(80, 130, 210)))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(4))
                    .show(ui, |ui| {
                        ui.set_min_width(rect.width().max(120.0));
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            for option in &popup.options {
                                if ui.selectable_label(false, option).clicked() {
                                    chosen = Some(option.clone());
                                }
                            }
                        });
                    });
            });
        let clicked_elsewhere = ctx.input(|i| {
            i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !rect.contains(p))
        });
        if let Some(value) = chosen {
            let (tab_id, name) = (popup.tab_id, popup.name.clone());
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.set_form_edit(&name, FormEdit::Text(value));
            }
            self.form_choice = None;
        } else if clicked_elsewhere || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.form_choice = None;
        }
    }

    /// Imprime l'onglet actif : boîte de dialogue de Windows, puis impression en arrière-plan.
    /// Les annotations et le texte OCR pas encore enregistrés sont imprimés aussi (copie temporaire).
    /// `pdf_printer_output` : impression sans dialogue vers « Microsoft Print to PDF » (vérifications).
    fn print_active_tab(&mut self, pdf_printer_output: Option<PathBuf>) {
        if self.print_job.as_ref().is_some_and(|j| !j.progress.lock().map(|p| p.finished).unwrap_or(true)) {
            self.error_message = Some("Une impression est déjà en cours.".to_string());
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active_tab) else { return };
        let page_count = tab.page_count();
        let document = tab.title.clone();
        let choice = match &pdf_printer_output {
            Some(_) => crate::print::named_printer("Microsoft Print to PDF", page_count),
            None => crate::print::choose_printer(page_count),
        };
        let Some(choice) = choice else { return };
        let mut source = tab.path.clone();
        let mut cleanup = None;
        if tab.is_modified {
            let temp = std::env::temp_dir().join(format!("kite-impression-{}.pdf", tab.id));
            tab.ensure_details_for_saved_pages(self.engine);
            let saved = save_pdf_with_ocr_state(
                &tab.path,
                &temp,
                &tab.annotations.items,
                &tab.text_layers,
                &tab.page_dimensions,
                &[],
                &[],
                &tab.form_edit_list(),
            );
            if saved.is_ok() {
                source = temp.clone();
                cleanup = Some(temp);
            }
        }
        let progress = crate::print::print_in_background(
            self.engine,
            source,
            document.clone(),
            choice,
            pdf_printer_output,
            cleanup,
        );
        self.print_job = Some(PrintJobState { progress, document, acknowledged: false });
    }

    /// Avancement puis fin de l'impression, en bas de la page.
    fn render_print_status(&mut self, ctx: &Context, canvas_rect: Rect) {
        let Some(job) = &self.print_job else { return };
        if job.acknowledged {
            return;
        }
        let Ok(progress) = job.progress.lock().map(|p| p.clone()) else { return };
        if !progress.finished {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        let message = match (&progress.error, progress.finished) {
            (Some(error), _) => format!("Impression de « {} » impossible : {error}", job.document),
            (None, true) => format!(
                "« {} » envoyé à l'imprimante ({} page{}).",
                job.document,
                progress.total,
                if progress.total > 1 { "s" } else { "" }
            ),
            (None, false) => format!("Impression de « {} » : page {} / {}", job.document, progress.done + 1, progress.total),
        };
        let mut close = false;
        let width = (canvas_rect.width() - 48.0).min(480.0);
        egui::Area::new(egui::Id::new("kite_print_status"))
            .fixed_pos(Pos2::new(canvas_rect.center().x - width * 0.5, canvas_rect.max.y - 90.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(26, 34, 48))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(80, 130, 210)))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(width - 24.0);
                        ui.horizontal(|ui| {
                            if !progress.finished {
                                ui.add(egui::Spinner::new().size(12.0));
                            }
                            ui.label(egui::RichText::new(message).size(12.5).color(Color32::WHITE));
                            if progress.finished {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("Fermer").clicked() {
                                        close = true;
                                    }
                                });
                            }
                        });
                    });
            });
        if close {
            if let Some(job) = &mut self.print_job {
                job.acknowledged = true;
            }
        }
    }

    /// Fenêtre de saisie du mot de passe d'un PDF protégé.
    fn render_password_prompt(&mut self, ctx: &Context) {
        let Some(prompt) = self.password_prompt.as_mut() else {
            return;
        };
        let name = prompt.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut submit = false;
        let mut cancel = false;
        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(Order::Foreground, egui::Id::new("password_backdrop")))
            .rect_filled(screen, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(6, 8, 12, 150));
        let width = 400.0_f32.min(screen.width() - 32.0);
        egui::Area::new(egui::Id::new("kite_password_prompt"))
            .fixed_pos(Pos2::new(screen.center().x - width * 0.5, screen.min.y + screen.height() * 0.25))
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(24, 28, 39))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(70, 100, 170)))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(width - 36.0);
                        ui.label(egui::RichText::new("Document protégé").size(17.0).color(Color32::WHITE));
                        ui.label(
                            egui::RichText::new(format!("« {name} » demande un mot de passe pour s'ouvrir."))
                                .size(12.0)
                                .color(Color32::from_rgb(150, 160, 182)),
                        );
                        ui.add_space(10.0);
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut prompt.input)
                                .password(true)
                                .hint_text("Mot de passe")
                                .desired_width(width - 36.0),
                        );
                        if prompt.focus_requested {
                            field.request_focus();
                            prompt.focus_requested = false;
                        }
                        self.text_field_focused_now |= field.has_focus();
                        if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            submit = true;
                        }
                        if prompt.wrong {
                            ui.label(
                                egui::RichText::new("Mot de passe incorrect.")
                                    .size(11.5)
                                    .color(Color32::from_rgb(255, 130, 120)),
                            );
                        }
                        ui.label(
                            egui::RichText::new("Il n'est gardé que jusqu'à la fermeture de l'application.")
                                .size(10.5)
                                .color(Color32::from_rgb(120, 130, 152)),
                        );
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!prompt.input.is_empty(), egui::Button::new("Ouvrir")).clicked() {
                                submit = true;
                            }
                            if ui.button("Annuler").clicked() {
                                cancel = true;
                            }
                        });
                    });
            });
        if submit && !prompt.input.is_empty() {
            let (path, password) = (prompt.path.clone(), std::mem::take(&mut prompt.input));
            self.password_prompt = None;
            crate::pdf_engine::set_document_password(&path, &password);
            self.open_pdf_in_new_tab(&path);
        } else if cancel {
            self.password_prompt = None;
        }
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.remember_reading(index);
        let removed = self.tabs.remove(index);
        self.closed_tabs.retain(|p| p != &removed.path);
        self.closed_tabs.push(removed.path.clone());
        self.texture_cache.evict_tab(removed.id);
        self.workers.tab_closed(removed.id);

        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else {
            let next_active = if self.active_tab >= self.tabs.len() {
                self.tabs.len() - 1
            } else if index < self.active_tab {
                self.active_tab.saturating_sub(1)
            } else {
                self.active_tab
            };
            self.switch_to_tab(next_active);
        }
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            let next = (self.active_tab + 1) % self.tabs.len();
            self.switch_to_tab(next);
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            let prev = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
            self.switch_to_tab(prev);
        }
    }

    fn zoom_bucket(zoom: f32) -> u32 {
        ((zoom * ZOOM_BUCKETS_PER_UNIT).round() as u32).max(1)
    }

    /// Enregistre les annotations ISO 32000 ET la couche de texte OCR invisible (`3 Tr` + `Tm`) dans le PDF (`Ctrl+S`)
    /// Retient la position et la taille de la fenêtre (hors agrandissement) pour le prochain lancement.
    fn track_window(&mut self, ctx: &Context) {
        let (outer, inner, maximized) = ctx.input(|i| {
            let viewport = i.viewport();
            (viewport.outer_rect, viewport.inner_rect, viewport.maximized.unwrap_or(false))
        });
        self.last_maximized = maximized;
        if let (false, Some(outer), Some(inner)) = (maximized, outer, inner) {
            self.last_window = Some([outer.min.x, outer.min.y, inner.width(), inner.height()]);
        }
    }

    /// PDF ouverts depuis l'Explorateur pendant que l'application tournait déjà : un onglet chacun
    /// (ou celui où le fichier est déjà ouvert), et la fenêtre revient au premier plan.
    fn open_forwarded_files(&mut self, ctx: &Context) {
        let requests = crate::single_instance::take_requests();
        if requests.is_empty() {
            return;
        }
        for path in requests.into_iter().flat_map(|r| r.paths) {
            match self.tabs.iter().position(|t| t.path == path) {
                Some(index) => self.switch_to_tab(index),
                None => self.open_pdf_in_new_tab(&path),
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// « document.pdf — Kite PDF » : le document actif se retrouve dans la barre des tâches et Alt+Tab.
    fn update_window_title(&mut self, ctx: &Context) {
        let title = match self.tabs.get(self.active_tab) {
            Some(tab) => format!("{} — {}", tab.title, crate::APP_NAME),
            None => crate::APP_NAME.to_string(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }

    /// PDF glissés dans la fenêtre : chacun s'ouvre dans un onglet (un calque l'annonce pendant le survol).
    fn open_dropped_files(&mut self, ctx: &Context) {
        let is_pdf = |path: &Path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let (dropped, hovering) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect();
            (dropped, !i.raw.hovered_files.is_empty())
        });
        if hovering {
            let screen = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(Order::Foreground, egui::Id::new("kite_drop")));
            painter.rect_filled(screen, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(20, 40, 80, 170));
            painter.rect_stroke(
                screen.shrink(12.0),
                CornerRadius::same(16),
                Stroke::new(2.0_f32, Color32::from_rgb(120, 170, 255)),
                StrokeKind::Inside,
            );
            painter.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "Déposer pour ouvrir",
                FontId::proportional(22.0),
                Color32::WHITE,
            );
        }
        let mut refused = Vec::new();
        for path in dropped {
            if is_pdf(&path) {
                self.open_pdf_in_new_tab(&path);
            } else {
                refused.push(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            }
        }
        if !refused.is_empty() {
            self.error_message = Some(format!("Seuls les PDF s'ouvrent ici : {}", refused.join(", ")));
        }
    }

    /// Note la page et le zoom de l'onglet `index` dans l'historique, pour y revenir à la réouverture.
    fn remember_reading(&mut self, index: usize) {
        if !self.prefs.resume_reading || !self.prefs.keep_history {
            return;
        }
        let Some(tab) = self.tabs.get(index) else { return };
        if tab.page_count() == 0 {
            return;
        }
        let page = tab.viewport.current_page_index(self.canvas_size, &tab.page_dimensions);
        let (path, zoom) = (tab.path.clone(), tab.viewport.zoom);
        self.recent_files.remember_reading(&path, page, zoom);
    }

    /// À la fermeture : endroit de lecture de chaque onglet, fenêtre et barre latérale.
    pub fn save_session(&mut self) {
        for index in 0..self.tabs.len() {
            self.remember_reading(index);
        }
        if let Some(settings) = &mut self.settings {
            if self.last_window.is_some() {
                settings.window = self.last_window;
            }
            settings.maximized = self.last_maximized;
            settings.sidebar_pinned = Some(self.sidebar.pinned);
            settings.sidebar_width = Some(self.sidebar.width);
            settings.drawer_height = Some(self.sidebar.drawer_height);
            settings.prefs = self.prefs.clone();
            settings.save();
        }
    }

    /// « Enregistrer sous » : écrit une copie (annotations et texte OCR compris) puis continue sur
    /// cette copie ; l'original reste tel qu'il était.
    fn save_active_tab_as(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let stem = tab.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Document PDF", &["pdf"])
            .set_file_name(format!("{stem} (copie).pdf"));
        if let Some(dir) = tab.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut destination) = dialog.save_file() else { return };
        if destination.extension().is_none() {
            destination.set_extension("pdf");
        }
        let same_file = std::fs::canonicalize(&destination).ok() == std::fs::canonicalize(&tab.path).ok();
        if same_file {
            self.save_active_tab_annotations();
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active_tab) else { return };
        tab.ensure_details_for_saved_pages(self.engine);
        let result = save_pdf_with_ocr_state(
            &tab.path,
            &destination,
            &tab.annotations.items,
            &tab.text_layers,
            &tab.page_dimensions,
            &tab.ocr.page_scanned,
            &tab.ocr.deep_done,
            &tab.form_edit_list(),
        );
        match result {
            Ok(()) => {
                // La suite du travail se fait sur la copie.
                let tab_id = tab.id;
                tab.hibernate(&mut self.texture_cache);
                tab.path = destination.clone();
                tab.title = destination
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| tab.title.clone());
                tab.is_modified = false;
                tab.ocr.has_unsaved_ocr = false;
                tab.mark_ocr_text_as_saved();
                tab.forget_saved_form_edits();
                tab.annotations.saved_feedback_frames = 120;
                if let Err(err) = tab.ensure_awake(self.engine) {
                    self.error_message = Some(format!("{err:#}"));
                }
                self.texture_cache.evict_tab(tab_id);
                self.workers.forget_tab(tab_id);
                self.recent_files.record_open(&destination);
                self.save_error = None;
            }
            Err(e) => self.save_error = Some(format!("{e:#}")),
        }
    }

    pub fn save_active_tab_annotations(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.ensure_details_for_saved_pages(self.engine);
            // Relâcher temporairement le handle PDFium sur Windows pour écrire dans le fichier en place
            let tab_id = tab.id;
            tab.hibernate(&mut self.texture_cache);
            let res = save_pdf_with_ocr_state(
                &tab.path,
                &tab.path,
                &tab.annotations.items,
                &tab.text_layers,
                &tab.page_dimensions,
                &tab.ocr.page_scanned,
                &tab.ocr.deep_done,
                &tab.form_edit_list(),
            );
            let _ = tab.ensure_awake(self.engine);
            self.texture_cache.evict_tab(tab_id);
            self.workers.forget_tab(tab_id);
            match res {
                Ok(()) => {
                    tab.is_modified = false;
                    tab.ocr.has_unsaved_ocr = false;
                    // Le texte reconnu fait maintenant partie du fichier : un nouvel enregistrement
                    // ne doit pas réécrire sa couche invisible une seconde fois.
                    tab.mark_ocr_text_as_saved();
                    tab.forget_saved_form_edits();
                    tab.annotations.saved_feedback_frames = 120;
                    self.error_message = None;
                    self.save_error = None;
                }
                Err(e) => {
                    self.save_error = Some(format!("{:#}", e));
                }
            }
        }
    }

    pub fn scroll_to_current_search_match(&mut self, viewport_size: Vec2) {
        let Some(m) = self
            .search_bar
            .matches
            .get(self.search_bar.current_match_index)
            .cloned()
        else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let (layouts, _, _) = tab.viewport.compute_layout(&tab.page_dimensions);
            if let Some(layout) = layouts.get(m.page_index) {
                if let Some(first_box) = m.highlight_boxes.first() {
                    let page_dim = tab.page_dimensions[m.page_index];
                    let box_y_in_page_px =
                        (first_box.y_min / page_dim.height_pt.max(1.0)) * layout.height;
                    let target_y = layout.y_top + box_y_in_page_px - viewport_size.y * 0.38;
                    tab.viewport.scroll_offset.y = target_y.max(0.0);
                    tab.viewport
                        .clamp_scroll(viewport_size, &tab.page_dimensions);
                } else {
                    tab.viewport
                        .jump_to_page(m.page_index, viewport_size, &tab.page_dimensions);
                }
            }
        }
    }

    fn execute_palette_command(&mut self, cmd: PaletteCommand, viewport_size: Vec2) {
        match cmd {
            PaletteCommand::SwitchTab(idx) => {
                self.switch_to_tab(idx);
            }
            PaletteCommand::ToggleSidebarPin => {
                if !self.top_layout {
                    self.sidebar.toggle_pinned();
                }
            }
            PaletteCommand::OpenFileDialog => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Document PDF", &["pdf"])
                    .pick_file()
                {
                    self.open_pdf_in_new_tab(&path);
                }
            }
            PaletteCommand::SaveAnnotatedPdf => {
                self.save_active_tab_annotations();
            }
            PaletteCommand::SaveAs => {
                self.save_active_tab_as();
            }
            PaletteCommand::Print => {
                self.print_active_tab(None);
            }
            PaletteCommand::OpenRecent(path) => {
                if path.is_file() {
                    self.open_pdf_in_new_tab(&path);
                } else {
                    self.recent_files.remove(&path);
                    self.error_message =
                        Some(format!("« {} » est introuvable : il a peut-être été déplacé ou supprimé.", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                }
            }
            PaletteCommand::ReopenClosedTab => {
                while let Some(path) = self.closed_tabs.pop() {
                    if path.is_file() {
                        self.open_pdf_in_new_tab(&path);
                        break;
                    }
                }
            }
            PaletteCommand::OpenSettings => {
                self.settings_open = true;
            }
            PaletteCommand::ShowShortcuts => {
                self.shortcuts_open = true;
            }
            PaletteCommand::OpenDeepOcrDialog => {
                self.open_deep_ocr_dialog(viewport_size);
            }
            PaletteCommand::OpenSearch => {
                self.search_bar.open();
                let read_images = self.prefs.image_ocr != crate::settings::ImageOcr::Never;
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let p = tab.viewport.current_page_index(viewport_size, &tab.page_dimensions);
                    tab.request_all_text(&self.workers);
                    if read_images {
                        tab.start_turbo_ocr();
                        tab.pump_ocr(p, &self.workers);
                    }
                }
            }
            PaletteCommand::UndoAnnotation => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.annotations.undo();
                }
            }
            PaletteCommand::RedoAnnotation => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.annotations.redo();
                }
            }
            PaletteCommand::DeleteSelectedAnnotation => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.annotations.delete_selected();
                }
            }
            PaletteCommand::HibernateInactiveTabs => {
                let current = self.active_tab;
                for (i, tab) in self.tabs.iter_mut().enumerate() {
                    if i != current {
                        tab.hibernate(&mut self.texture_cache);
                        self.workers.forget_tab(tab.id);
                    }
                }
                self.texture_cache.clear();
            }
            PaletteCommand::CloseActiveTab => {
                if !self.tabs.is_empty() {
                    let idx = self.active_tab;
                    self.close_tab(idx);
                }
            }
            PaletteCommand::SelectTool(tool) => {
                self.active_tool = tool;
            }
            PaletteCommand::FitToWidth => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport
                        .fit_to_width(viewport_size, &tab.page_dimensions);
                }
            }
            PaletteCommand::FitToPage => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport
                        .fit_to_page(viewport_size, &tab.page_dimensions);
                }
            }
            PaletteCommand::ZoomReset100 => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport.zoom = 1.0;
                    tab.viewport.scroll_offset.x = 0.0;
                    tab.viewport
                        .clamp_scroll(viewport_size, &tab.page_dimensions);
                    tab.viewport.last_gesture_label = "Zoom 100%".to_string();
                }
            }
            PaletteCommand::SetZoomPercent(percent) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let target = percent as f32 / 100.0;
                    let factor = target / tab.viewport.zoom.max(0.01);
                    let pages = tab.page_dimensions.clone();
                    tab.viewport
                        .apply_zoom_around_anchor(factor, viewport_size * 0.5, viewport_size, &pages);
                }
            }
            PaletteCommand::JumpToPageAt(page_idx, y_pt) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let pages = tab.page_dimensions.clone();
                    let (layouts, _, _) = tab.viewport.compute_layout(&pages);
                    if let (Some(layout), Some(dims)) = (layouts.get(page_idx), pages.get(page_idx)) {
                        let y_px = layout.y_top + y_pt / dims.height_pt.max(1.0) * layout.height;
                        tab.viewport.scroll_offset.y = (y_px - OUTLINE_TARGET_MARGIN_PX).max(0.0);
                        tab.viewport.clamp_scroll(viewport_size, &pages);
                    }
                }
            }
            PaletteCommand::JumpToPage(page_idx) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport
                        .jump_to_page(page_idx, viewport_size, &tab.page_dimensions);
                }
            }
        }
    }

    fn handle_inputs(&mut self, ctx: &Context, canvas_rect: Rect) {
        let viewport_size = canvas_rect.size();

        if self.frame_count == 1 {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if let Some(target_page) = self.automation.initial_page {
                    tab.viewport.jump_to_page(
                        target_page.saturating_sub(1),
                        viewport_size,
                        &tab.page_dimensions,
                    );
                }
                if self.automation.initial_scroll_x.is_some()
                    || self.automation.initial_scroll_y.is_some()
                {
                    tab.viewport
                        .clamp_scroll(viewport_size, &tab.page_dimensions);
                }
            }
            if self.automation.initial_search_query.is_some()
                && self.automation.initial_scroll_y.is_none()
                && self.automation.initial_page.is_none()
            {
                self.scroll_to_current_search_match(viewport_size);
            }
        }

        let mut pending_commands: Vec<PaletteCommand> = Vec::new();
        let mut tab_cycle_dir: i32 = 0;
        let mut trigger_search_open = false;
        let mut copy_selected_text = false;
        let text_input_focused = self.text_field_focused_prev;

        ctx.input(|input| {
            // 1. Capture d'écran GPU automatisée
            for event in &input.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    if let Some(out_path) = &self.automation.screenshot_output {
                        if let Some(parent) = out_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let w = image.width() as u32;
                        let h = image.height() as u32;
                        let rgba: Vec<u8> =
                            image.pixels.iter().flat_map(|c| c.to_array()).collect();
                        if let Some(buf) = image::RgbaImage::from_raw(w, h, rgba) {
                            let _ = buf.save(out_path);
                        }
                        // Comme une fermeture normale : endroit de lecture et réglages enregistrés.
                        self.save_session();
                        std::process::exit(0);
                    }
                }
            }

            // 2. Détection du survol du bord gauche au Trackpad
            if self.automation.sidebar_mode != InitialSidebarMode::AutoHideHover {
                let pointer_x = input.pointer.hover_pos().map(|p| p.x);
                self.sidebar.update_hover_hysteresis(pointer_x);
            }

            let ctrl = input.modifiers.ctrl || input.modifiers.command;
            let shift = input.modifiers.shift;

            // 3. Raccourcis globaux (Ctrl+F, Ctrl+C, Ctrl+S, Ctrl+Z, Ctrl+Y, Ctrl+K, Ctrl+B)
            if ctrl && input.key_pressed(Key::F) {
                trigger_search_open = true;
            }
            if (ctrl && input.key_pressed(Key::C))
                || input
                    .events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Copy))
            {
                copy_selected_text = true;
            }
            if ctrl && input.key_pressed(Key::S) {
                pending_commands.push(if shift { PaletteCommand::SaveAs } else { PaletteCommand::SaveAnnotatedPdf });
            }
            if ctrl && input.key_pressed(Key::Z) {
                if shift {
                    pending_commands.push(PaletteCommand::RedoAnnotation);
                } else {
                    pending_commands.push(PaletteCommand::UndoAnnotation);
                }
            }
            if ctrl && input.key_pressed(Key::Y) {
                pending_commands.push(PaletteCommand::RedoAnnotation);
            }
            if !text_input_focused
                && (input.key_pressed(Key::Delete) || input.key_pressed(Key::Backspace))
            {
                pending_commands.push(PaletteCommand::DeleteSelectedAnnotation);
            }

            if ctrl && input.key_pressed(Key::P) {
                pending_commands.push(PaletteCommand::Print);
            }
            if ctrl && input.key_pressed(Key::K) {
                self.command_palette.toggle();
            }
            if ctrl && input.key_pressed(Key::Comma) {
                self.settings_open = !self.settings_open;
            }
            if input.key_pressed(Key::F1) {
                self.shortcuts_open = !self.shortcuts_open;
            }
            if input.key_pressed(Key::Escape) {
                if self.welcome_open || self.shortcuts_open || self.settings_open {
                    // Fermés par leur fenêtre.
                } else if self.save_error.is_some() {
                    self.save_error = None;
                } else if self.editing_freetext_id.is_some()
                    || self.line_context_menu.is_some()
                    || self.comment_edit.is_some()
                {
                    // Géré par l'éditeur en place et le menu contextuel.
                } else if self.password_prompt.is_some() {
                    self.password_prompt = None;
                } else if self.deep_ocr_dialog.is_some() {
                    self.deep_ocr_dialog = None;
                } else if self.no_text_hint.is_some() {
                    self.no_text_hint = None;
                } else if self.command_palette.is_open {
                    self.command_palette.close();
                } else if self.search_bar.is_open {
                    self.search_bar.close();
                } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.text_selection.clear();
                    tab.annotations.selected_id = None;
                }
            }

            if ctrl && input.key_pressed(Key::B) {
                pending_commands.push(PaletteCommand::ToggleSidebarPin);
            }
            if ctrl && input.key_pressed(Key::O) {
                pending_commands.push(PaletteCommand::OpenFileDialog);
            }
            if ctrl && input.key_pressed(Key::T) {
                if shift {
                    pending_commands.push(PaletteCommand::ReopenClosedTab);
                } else {
                    self.command_palette.toggle();
                }
            }
            if ctrl && input.key_pressed(Key::W) {
                pending_commands.push(PaletteCommand::CloseActiveTab);
            }
            if ctrl && input.key_pressed(Key::Tab) {
                tab_cycle_dir = if shift { -1 } else { 1 };
            }

            let num_keys = [
                (Key::Num1, 0),
                (Key::Num2, 1),
                (Key::Num3, 2),
                (Key::Num4, 3),
                (Key::Num5, 4),
                (Key::Num6, 5),
                (Key::Num7, 6),
                (Key::Num8, 7),
            ];
            if ctrl {
                for (k, idx) in num_keys {
                    if input.key_pressed(k) {
                        pending_commands.push(PaletteCommand::SwitchTab(idx));
                    }
                }
                if input.key_pressed(Key::Num0) {
                    pending_commands.push(PaletteCommand::FitToPage);
                }
                if input.key_pressed(Key::Num9) {
                    pending_commands.push(PaletteCommand::FitToWidth);
                }
            }

            if self.command_palette.is_open {
                return;
            }

            // 4. Raccourcis d'outils à 1 touche
            if !ctrl && !text_input_focused {
                if input.key_pressed(Key::V) {
                    self.active_tool = ActiveTool::Select;
                } else if input.key_pressed(Key::H) {
                    self.active_tool = ActiveTool::Highlight;
                } else if input.key_pressed(Key::T) {
                    self.active_tool = ActiveTool::FreeText;
                } else if input.key_pressed(Key::D) {
                    self.active_tool = ActiveTool::Ink;
                } else if input.key_pressed(Key::R) {
                    self.active_tool = ActiveTool::Shape;
                } else if input.key_pressed(Key::N) {
                    self.active_tool = ActiveTool::StickyNote;
                } else if input.key_pressed(Key::M) {
                    self.active_tool = ActiveTool::Measure;
                }
            }

            // 5. Gestes Trackpad Windows Precision & Navigation sur l'onglet actif
            let Some(tab) = self.tabs.get_mut(self.active_tab) else {
                return;
            };
            let pages = &tab.page_dimensions;

            let pointer_pos = input.pointer.hover_pos();
            let pointer_over_sidebar = sidebar_shown(&self.sidebar, self.top_layout)
                && pointer_pos.map(|p| p.x <= self.sidebar.width).unwrap_or(false);

            let pointer_in_viewport = pointer_pos
                .map(|p| p - canvas_rect.min)
                .unwrap_or(viewport_size * 0.5);

            // Trackpad Pinch-to-Zoom (2 doigts) & Ctrl+Molette
            let zoom_delta = input.zoom_delta();
            if !pointer_over_sidebar && (zoom_delta - 1.0).abs() > 1e-3 {
                tab.viewport.apply_zoom_around_anchor(
                    zoom_delta,
                    pointer_in_viewport,
                    viewport_size,
                    pages,
                );
                tab.viewport.last_gesture_label = format!(
                    "Trackpad Pinch-Zoom -> {:.0}%",
                    tab.viewport.zoom * 100.0
                );
            }

            // Trackpad Défilement Fluide 2 Doigts (Vertical + Horizontal)
            let scroll_delta = input.smooth_scroll_delta;
            if !pointer_over_sidebar && !ctrl && scroll_delta.length_sq() > 0.01 {
                tab.viewport
                    .apply_pan_delta(scroll_delta, viewport_size, pages);
                tab.viewport.inertia_velocity = scroll_delta * 0.35;
                tab.viewport.last_gesture_label = format!(
                    "Trackpad Scroll 2D (dx:{:+.0}, dy:{:+.0})",
                    scroll_delta.x, scroll_delta.y
                );
            } else if tab.viewport.inertia_velocity.length_sq() > 0.5 {
                let v = tab.viewport.inertia_velocity;
                tab.viewport.apply_pan_delta(v, viewport_size, pages);
                tab.viewport.inertia_velocity *= 0.86;
            } else {
                tab.viewport.inertia_velocity = Vec2::ZERO;
            }

            // Panoramique à 1 doigt sur le trackpad (Outil Main, Espace maintenu + glisser, ou clic milieu)
            let space_held = (!text_input_focused && input.key_down(Key::Space))
                || self.active_tool == ActiveTool::Pan;
            let drag_pan_active = !pointer_over_sidebar
                && ((space_held && input.pointer.primary_down()) || input.pointer.middle_down());
            tab.viewport.is_panning = drag_pan_active;
            if drag_pan_active {
                let drag_delta = input.pointer.delta();
                if drag_delta.length_sq() > 0.01 {
                    tab.viewport
                        .apply_pan_delta(drag_delta, viewport_size, pages);
                    tab.viewport.last_gesture_label =
                        "Panoramique 1 doigt (Trackpad Drag)".to_string();
                }
            }

            if ctrl && (input.key_pressed(Key::Plus) || input.key_pressed(Key::Equals)) {
                tab.viewport.apply_zoom_around_anchor(
                    1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
            }
            if ctrl && input.key_pressed(Key::Minus) {
                tab.viewport.apply_zoom_around_anchor(
                    1.0 / 1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
            }

            let step_y = 75.0;
            let step_x = 60.0;
            if !ctrl && !text_input_focused {
                if input.key_pressed(Key::ArrowDown) || input.key_pressed(Key::J) {
                    tab.viewport
                        .apply_pan_delta(Vec2::new(0.0, -step_y), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowUp) || input.key_pressed(Key::K) {
                    tab.viewport
                        .apply_pan_delta(Vec2::new(0.0, step_y), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowRight) || input.key_pressed(Key::L) {
                    tab.viewport
                        .apply_pan_delta(Vec2::new(-step_x, 0.0), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowLeft) {
                    tab.viewport
                        .apply_pan_delta(Vec2::new(step_x, 0.0), viewport_size, pages);
                }
                if input.key_pressed(Key::PageDown) {
                    tab.viewport.apply_pan_delta(
                        Vec2::new(0.0, -viewport_size.y * 0.85),
                        viewport_size,
                        pages,
                    );
                }
                if input.key_pressed(Key::PageUp) || (shift && input.key_pressed(Key::Space)) {
                    tab.viewport.apply_pan_delta(
                        Vec2::new(0.0, viewport_size.y * 0.85),
                        viewport_size,
                        pages,
                    );
                }
                if input.key_pressed(Key::Home) {
                    tab.viewport.jump_to_page(0, viewport_size, pages);
                }
                if input.key_pressed(Key::End) && !pages.is_empty() {
                    tab.viewport
                        .jump_to_page(pages.len() - 1, viewport_size, pages);
                }
            }
        });

        if trigger_search_open {
            self.search_bar.open();
            let read_images = self.prefs.image_ocr != crate::settings::ImageOcr::Never;
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                let p = tab
                    .viewport
                    .current_page_index(viewport_size, &tab.page_dimensions);
                tab.request_all_text(&self.workers);
                if read_images {
                    tab.start_turbo_ocr();
                    tab.pump_ocr(p, &self.workers);
                }
            }
        }

        if copy_selected_text {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if !tab.text_selection.selected_text.is_empty() {
                    ctx.copy_text(tab.text_selection.selected_text.clone());
                    tab.text_selection.copied_feedback_frames = 90;
                }
            }
        }

        if tab_cycle_dir > 0 {
            self.next_tab();
        } else if tab_cycle_dir < 0 {
            self.prev_tab();
        }

        for cmd in pending_commands {
            self.execute_palette_command(cmd, viewport_size);
        }

        if let Some(tab) = self.tabs.get(self.active_tab) {
            if tab.viewport.inertia_velocity.length_sq() > 0.5 {
                ctx.request_repaint();
            }
        }
    }

    /// Gère sur une page donnée :
    /// 1. Le curseur `I` (`CursorIcon::Text`), la sélection au fil des phrases (glisser simple), le double-clic sur un mot, et `Alt + Glisser` (bloc rectangulaire).
    /// 2. La sélection, le déplacement et la création d'annotations ISO 32000 (`Highlight`, `FreeText`, `Ink`, `Shape`, `StickyNote`, `Measure`).
    fn handle_page_canvas_interactions(
        &mut self,
        ctx: &Context,
        page_index: usize,
        page_rect: Rect,
    ) {
        if self.command_palette.is_open || self.deep_ocr_dialog.is_some() || self.password_prompt.is_some() {
            return;
        }
        let pointer_over_sidebar = sidebar_shown(&self.sidebar, self.top_layout)
            && ctx
                .input(|i| i.pointer.hover_pos())
                .map(|p| p.x <= self.sidebar.width)
                .unwrap_or(false);
        if pointer_over_sidebar
            || self.pointer_on_scrollbar
            || (self.annotation_drag.is_none() && ctx.is_pointer_over_area())
        {
            return;
        }

        let (hover_pos, primary_pressed, primary_down, primary_released, double_clicked, alt_held, space_down) =
            ctx.input(|i| {
                (
                    i.pointer.hover_pos(),
                    i.pointer.primary_pressed(),
                    i.pointer.primary_down(),
                    i.pointer.primary_released(),
                    i.pointer.button_double_clicked(egui::PointerButton::Primary),
                    i.modifiers.alt,
                    i.key_down(Key::Space),
                )
            });

        if space_down || self.active_tool == ActiveTool::Pan {
            return;
        }

        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let page_dim = tab.page_dimensions[page_index];

        let Some(cursor) = hover_pos else {
            return;
        };
        if !page_rect.contains(cursor) || cursor.y < 52.0 {
            return;
        }

        let pt_x =
            (((cursor.x - page_rect.min.x) / page_rect.width().max(1.0)) * page_dim.width_pt)
                .clamp(0.0, page_dim.width_pt);
        let pt_y =
            (((cursor.y - page_rect.min.y) / page_rect.height().max(1.0)) * page_dim.height_pt)
                .clamp(0.0, page_dim.height_pt);

        // Curseur I-Beam quand on survole du texte en mode Sélection ou Surligner
        if matches!(
            self.active_tool,
            ActiveTool::Select | ActiveTool::Highlight | ActiveTool::StickyNote
        ) {
            tab.request_page_text(page_index, &self.workers, true);
            let over_annot = tab
                .annotations
                .items
                .iter()
                .any(|a| a.page_index == page_index && a.hit_test(pt_x, pt_y));
            if over_annot && self.active_tool == ActiveTool::Select {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            } else if let Some(layer) = tab.text_layers.get(page_index) {
                if layer.char_index_near_point(pt_x, pt_y, 12.0).is_some() {
                    ctx.set_cursor_icon(egui::CursorIcon::Text);
                }
            }
        } else {
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        let handle_tolerance_pt =
            HANDLE_GRAB_RADIUS_PX * page_dim.width_pt / page_rect.width().max(1.0);
        let selected_handle = (self.active_tool == ActiveTool::Select)
            .then(|| tab.annotations.selected())
            .flatten()
            .filter(|a| a.page_index == page_index)
            .and_then(|a| a.handle_at(pt_x, pt_y, handle_tolerance_pt));
        if let Some(handle) = selected_handle {
            ctx.set_cursor_icon(match handle {
                AnnotHandle::Corner(0) | AnnotHandle::Corner(3) => egui::CursorIcon::ResizeNwSe,
                AnnotHandle::Corner(_) => egui::CursorIcon::ResizeNeSw,
                _ => egui::CursorIcon::Grab,
            });
        }

        // Annotation sous le curseur : un dessin déjà sélectionné se saisit partout dans son cadre.
        let selected_id = tab.annotations.selected_id;
        let annotation_under_cursor = tab
            .annotations
            .items
            .iter()
            .rev()
            .find(|a| {
                a.page_index == page_index
                    && (a.hit_test(pt_x, pt_y)
                        || (selected_id == Some(a.id)
                            && matches!(a.kind, AnnotationKind::Ink { .. })
                            && a.rect.contains_pt(pt_x, pt_y, 0.0)))
            })
            .map(|a| a.id);

        // --- CAS 1 : OUTIL SÉLECTION (V) OU SURLIGNER (H) ---
        if matches!(
            self.active_tool,
            ActiveTool::Select | ActiveTool::Highlight | ActiveTool::StickyNote
        ) {
            if self.active_tool == ActiveTool::Select {
                // Double-clic : édition du bloc de texte, ou du commentaire sur le texte marqué.
                if double_clicked {
                    if let Some(id) = annotation_under_cursor {
                        let kind = tab.annotations.items.iter().find(|a| a.id == id).map(|a| &a.kind);
                        let is_freetext = matches!(kind, Some(AnnotationKind::FreeText { .. }));
                        let is_comment = matches!(
                            kind,
                            Some(AnnotationKind::StickyNote { .. } | AnnotationKind::Highlight { .. })
                        );
                        if is_freetext || is_comment {
                            tab.annotations.selected_id = Some(id);
                            tab.annotations.push_undo_snapshot();
                            self.annotation_drag = None;
                            tab.text_selection.clear();
                            if is_freetext {
                                self.editing_freetext_id = Some(id);
                                self.freetext_focus_pending = true;
                            } else {
                                if let Some(annot) = tab.annotations.selected_mut() {
                                    annot.convert_highlight_to_note(page_dim);
                                }
                                self.comment_edit = Some((id, CommentEditTarget::Main));
                                self.comment_focus_pending = true;
                            }
                            return;
                        }
                    }
                }

                // Clic droit sur une annotation : menu de ses attributs.
                if ctx.input(|i| i.pointer.secondary_released()) {
                    let arrow_id = annotation_under_cursor.filter(|id| {
                        tab.annotations.items.iter().any(|a| {
                            a.id == *id
                                && matches!(
                                    a.kind,
                                    AnnotationKind::Shape {
                                        shape_kind: ShapeKind::ArrowLine { .. },
                                        ..
                                    } | AnnotationKind::FreeText { .. }
                                        | AnnotationKind::StickyNote { .. }
                                        | AnnotationKind::Highlight { .. }
                                )
                        })
                    });
                    if let Some(id) = arrow_id {
                        tab.annotations.selected_id = Some(id);
                        tab.text_selection.clear();
                        self.line_context_menu = Some(LineContextMenu {
                            annotation_id: id,
                            position: cursor,
                            opened_on_frame: self.frame_count,
                        });
                        return;
                    }
                }
            }

            // Double-clic sur un mot : sélectionne le mot entier !
            if double_clicked {
                tab.request_page_text(page_index, &self.workers, true);
                if let Some(layer) = tab.text_layers.get(page_index) {
                    if let Some((s, e, txt, boxes)) = layer.select_word_at_point(pt_x, pt_y) {
                        tab.text_selection.page_index = Some(page_index);
                        tab.text_selection.mode = SelectionGestureMode::StreamIBeam;
                        tab.text_selection.anchor_char_index = Some(s);
                        tab.text_selection.current_char_index = Some(e);
                        tab.text_selection.selected_text = txt;
                        tab.text_selection.highlight_boxes = boxes;
                        return;
                    }
                }
            }

            if primary_pressed {
                // 1a. Poignée de l'annotation sélectionnée, sinon annotation sous le curseur.
                // Surlignages et notes sont solidaires du texte : ils se sélectionnent sans se déplacer.
                let mut keep_annotation_selected = false;
                if self.active_tool == ActiveTool::Select && !alt_held {
                    let grabbed = selected_handle
                        .map(|handle| (tab.annotations.selected_id, handle))
                        .or_else(|| {
                            annotation_under_cursor.map(|id| (Some(id), AnnotHandle::Body))
                        });
                    if let Some((Some(id), handle)) = grabbed {
                        tab.annotations.selected_id = Some(id);
                        let original = tab.annotations.selected().cloned();
                        if original.as_ref().is_some_and(|a| a.is_fixed_to_text()) {
                            keep_annotation_selected = true;
                        } else {
                            if let Some(original) = original {
                                self.annotation_drag = Some(AnnotationDrag {
                                    id,
                                    handle,
                                    grab_pt: (pt_x, pt_y),
                                    original,
                                    has_moved: false,
                                });
                            }
                            tab.text_selection.clear();
                            return;
                        }
                    }
                }

                // 1b. Sinon, démarrer une sélection de texte (Flux Curseur I par défaut, ou Rectangle si Alt maintenu)
                if !keep_annotation_selected {
                    tab.annotations.selected_id = None;
                }
                self.annotation_drag = None;
                tab.request_page_text(page_index, &self.workers, true);
                tab.text_selection.page_index = Some(page_index);
                tab.text_selection.drag_start_pt = Some((pt_x, pt_y));
                tab.text_selection.drag_current_pt = Some((pt_x, pt_y));
                tab.text_selection.selected_text.clear();
                tab.text_selection.highlight_boxes.clear();

                if alt_held {
                    tab.text_selection.mode = SelectionGestureMode::RectangularBlock;
                } else {
                    tab.text_selection.mode = SelectionGestureMode::StreamIBeam;
                    if let Some(layer) = tab.text_layers.get(page_index) {
                        let start_idx = layer.char_index_near_point(pt_x, pt_y, 28.0);
                        tab.text_selection.anchor_char_index = start_idx;
                        tab.text_selection.current_char_index = start_idx;
                    }
                }
            }

            if primary_down {
                // Déplacement ou redimensionnement en direct de l'annotation saisie
                if let Some(drag) = &mut self.annotation_drag {
                    let dist = (pt_x - drag.grab_pt.0).hypot(pt_y - drag.grab_pt.1);
                    if drag.has_moved || dist > 0.5 {
                        if !drag.has_moved {
                            drag.has_moved = true;
                            tab.annotations.push_undo_snapshot();
                        }
                        if let Some(annot) =
                            tab.annotations.items.iter_mut().find(|a| a.id == drag.id)
                        {
                            annot.apply_handle_drag(
                                &drag.original,
                                drag.handle,
                                drag.grab_pt,
                                (pt_x, pt_y),
                            );
                            tab.is_modified = true;
                        }
                    }
                    return;
                }

                // Mise à jour de la sélection de texte en cours
                if tab.text_selection.page_index == Some(page_index) {
                    tab.text_selection.drag_current_pt = Some((pt_x, pt_y));
                    if let Some(layer) = tab.text_layers.get(page_index) {
                        match tab.text_selection.mode {
                            SelectionGestureMode::StreamIBeam => {
                                if let Some(anchor_idx) = tab.text_selection.anchor_char_index {
                                    if let Some(curr_idx) =
                                        layer.char_index_near_point(pt_x, pt_y, 40.0)
                                    {
                                        tab.text_selection.current_char_index = Some(curr_idx);
                                        if curr_idx != anchor_idx {
                                            let (txt, boxes) =
                                                layer.select_char_stream(anchor_idx, curr_idx);
                                            tab.text_selection.selected_text = txt;
                                            tab.text_selection.highlight_boxes = boxes;
                                        }
                                    }
                                }
                            }
                            SelectionGestureMode::RectangularBlock => {
                                if let Some(sel_rect) = tab.text_selection.active_drag_rect() {
                                    if sel_rect.width() > 3.0 || sel_rect.height() > 3.0 {
                                        let (txt, boxes) = layer.select_in_rect(&sel_rect);
                                        tab.text_selection.selected_text = txt;
                                        tab.text_selection.highlight_boxes = boxes;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if primary_released {
                self.annotation_drag = None;

                let dragged_far = match (tab.text_selection.drag_start_pt, tab.text_selection.drag_current_pt) {
                    (Some(a), Some(b)) => (a.0 - b.0).hypot(a.1 - b.1) > 12.0,
                    _ => false,
                };
                if matches!(self.active_tool, ActiveTool::Select | ActiveTool::Highlight)
                    && dragged_far
                    && !tab.text_selection.has_selection()
                    && tab.page_lacks_text(page_index) == Some(true)
                    && !tab.ocr.deep_running
                {
                    self.no_text_hint = Some(NoTextHint { page: page_index, position: cursor, shown_at: Instant::now() });
                }

                // Un glissé sur du texte marqué est une sélection de texte, pas un clic sur l'annotation.
                if tab.text_selection.has_selection()
                    && tab.annotations.selected().is_some_and(|a| a.is_fixed_to_text())
                {
                    tab.annotations.selected_id = None;
                }

                // Outil Note : la note s'attache à la sélection, ou au mot cliqué.
                if self.active_tool == ActiveTool::StickyNote {
                    if !tab.text_selection.has_selection() {
                        if let Some(layer) = tab.text_layers.get(page_index) {
                            if let Some((_, _, txt, boxes)) = layer.select_word_at_point(pt_x, pt_y) {
                                tab.text_selection.selected_text = txt;
                                tab.text_selection.highlight_boxes = boxes;
                            }
                        }
                    }
                    let quads = std::mem::take(&mut tab.text_selection.highlight_boxes);
                    tab.text_selection.clear();
                    if !quads.is_empty() {
                        let color = tab.annotations.active_color_rgb;
                        let note = PdfAnnotation::new_text_note(page_index, quads, color, page_dim);
                        let new_id = tab.annotations.add_annotation(note);
                        tab.is_modified = true;
                        self.comment_edit = Some((new_id, CommentEditTarget::Main));
                        self.comment_focus_pending = true;
                        self.active_tool = ActiveTool::Select;
                    }
                }

                // Si l'outil actif est Surligner (H) et qu'une sélection vient d'être faite, créer l'annotation /Highlight ISO 32000 !
                if self.active_tool == ActiveTool::Highlight && tab.text_selection.has_selection() {
                    let quads = tab.text_selection.highlight_boxes.clone();
                    if let Some(first) = quads.first() {
                        let union_rect = quads.iter().fold(*first, |acc, q| acc.union(q));
                        let color = tab.annotations.active_color_rgb;
                        tab.annotations.add_annotation(PdfAnnotation {
                            meta: Default::default(),
                            id: 0,
                            page_index,
                            rect: union_rect,
                            color_rgb: color,
                            kind: AnnotationKind::Highlight { quads },
                        });
                        tab.is_modified = true;
                        tab.text_selection.clear();
                    }
                }
            }
            return;
        }

        // --- CAS 2 : OUTIL BLOC DE TEXTE (/FreeText - Touche T) ---
        if self.active_tool == ActiveTool::FreeText {
            if primary_pressed {
                let rect = PdfTopDownRect {
                    x_min: pt_x,
                    y_min: pt_y,
                    x_max: (pt_x + 185.0).min(page_dim.width_pt - 10.0),
                    y_max: (pt_y + 48.0).min(page_dim.height_pt - 10.0),
                    angle_deg: 0.0,
                };
                let color = tab.annotations.active_color_rgb;
                let (fill_rgb, border_rgb) = self
                    .freetext_style
                    .unwrap_or((Some(FREETEXT_DEFAULT_FILL), Some(color)));
                let new_id = tab.annotations.add_annotation(PdfAnnotation {
                    meta: Default::default(),
                    id: 0,
                    page_index,
                    rect,
                    color_rgb: color,
                    kind: AnnotationKind::FreeText {
                        text: String::new(),
                        font_size: self.freetext_font_size,
                        text_rgb: FREETEXT_TEXT_RGB,
                        fill_rgb,
                        border_rgb,
                    },
                });
                self.editing_freetext_id = Some(new_id);
                self.freetext_focus_pending = true;
                self.freetext_awaiting_opening_release = true;
                tab.is_modified = true;
                self.active_tool = ActiveTool::Select;
            }
            return;
        }

        // --- CAS 3 : OUTILS DESSIN LIBRE (/Ink - D), FORMES (/Line & /Square - R), ET MESURE (/Measure - M) ---
        if matches!(
            self.active_tool,
            ActiveTool::Ink | ActiveTool::Shape | ActiveTool::Measure
        ) {
            if primary_pressed {
                tab.annotations.active_draft_page = Some(page_index);
                tab.annotations.active_draft_points.clear();
                tab.annotations.active_draft_points.push((pt_x, pt_y));
            }
            if primary_down && tab.annotations.active_draft_page == Some(page_index) {
                if let Some(&last) = tab.annotations.active_draft_points.last() {
                    let dist_sq = (pt_x - last.0).powi(2) + (pt_y - last.1).powi(2);
                    if dist_sq > 2.0 {
                        tab.annotations.active_draft_points.push((pt_x, pt_y));
                    }
                }
            }
            if primary_released && tab.annotations.active_draft_page == Some(page_index) {
                let pts = std::mem::take(&mut tab.annotations.active_draft_points);
                tab.annotations.active_draft_page = None;

                // Un trait enchaîné rapidement complète le dessin précédent (signature).
                let continued_ink = (self.active_tool == ActiveTool::Ink && pts.len() >= 2)
                    .then_some(self.last_ink_stroke)
                    .flatten()
                    .filter(|(_, at)| at.elapsed() < INK_STROKE_GROUPING)
                    .map(|(id, _)| id)
                    .filter(|id| {
                        tab.annotations
                            .items
                            .iter()
                            .any(|a| a.id == *id && a.page_index == page_index)
                    });
                if let Some(id) = continued_ink {
                    tab.annotations.push_undo_snapshot();
                    if let Some(annot) = tab.annotations.items.iter_mut().find(|a| a.id == id) {
                        annot.append_ink_stroke(pts);
                    }
                    self.last_ink_stroke = Some((id, Instant::now()));
                    tab.is_modified = true;
                } else if pts.len() >= 2 {
                    let start = pts[0];
                    let end = *pts.last().unwrap();
                    let min_x = pts.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
                    let max_x = pts.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
                    let min_y = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
                    let max_y = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
                    let rect = PdfTopDownRect {
                        x_min: (min_x - 4.0).max(0.0),
                        y_min: (min_y - 4.0).max(0.0),
                        x_max: (max_x + 4.0).min(page_dim.width_pt),
                        y_max: (max_y + 4.0).min(page_dim.height_pt),
                        angle_deg: 0.0,
                    };
                    let color = tab.annotations.active_color_rgb;
                    let kind = match self.active_tool {
                        ActiveTool::Ink => AnnotationKind::Ink {
                            strokes: vec![pts],
                            thickness: 2.5,
                        },
                        ActiveTool::Shape => AnnotationKind::Shape {
                            shape_kind: if alt_held {
                                ShapeKind::Rectangle
                            } else {
                                ShapeKind::DEFAULT_ARROW
                            },
                            start_pt: start,
                            end_pt: end,
                        },
                        ActiveTool::Measure => AnnotationKind::Measure {
                            start_pt: start,
                            end_pt: end,
                            stamp_tag: Some("COTE ISO".to_string()),
                        },
                        _ => unreachable!(),
                    };
                    let is_ink = matches!(kind, AnnotationKind::Ink { .. });
                    let is_shape = matches!(kind, AnnotationKind::Shape { .. });
                    let mut annotation = PdfAnnotation {
                        meta: Default::default(),
                        id: 0,
                        page_index,
                        rect,
                        color_rgb: color,
                        kind,
                    };
                    annotation.refresh_rect_from_geometry();
                    let new_id = tab.annotations.add_annotation(annotation);
                    tab.is_modified = true;
                    if is_ink {
                        self.last_ink_stroke = Some((new_id, Instant::now()));
                    }
                    if is_shape {
                        // La forme posée est immédiatement ajustable avec ses poignées.
                        self.active_tool = ActiveTool::Select;
                    }
                }
            }
        }
    }
}

impl eframe::App for KiteApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.frame_count += 1;
        self.text_field_focused_prev = std::mem::take(&mut self.text_field_focused_now);
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_instant).as_secs_f32();
        self.last_frame_instant = now;

        self.process_worker_events(ctx);
        self.track_window(ctx);
        self.open_dropped_files(ctx);
        self.open_forwarded_files(ctx);
        self.update_window_title(ctx);
        if self.last_session_save.elapsed() >= SESSION_SAVE_INTERVAL {
            self.last_session_save = Instant::now();
            self.save_session();
        }

        // Lecture approfondie terminée : l'OCR des images de cet onglet peut reprendre.
        if let Some(run) = &self.deep_ocr {
            if run.progress.lock().map(|p| p.finished).unwrap_or(true) {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == run.tab_id) {
                    tab.ocr.deep_running = false;
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }

        // OCR téléchargé en tâche de fond : les onglets qui l'avaient trouvé absent réessaient.
        if self.ocr_install.as_ref().is_some_and(|job| job.is_finished()) {
            self.ocr_install = None;
            for tab in &mut self.tabs {
                if tab.ocr.unavailable_reason.as_deref() == Some(crate::ocr_engine::OCR_MODELS_MISSING) {
                    tab.ocr.unavailable_reason = None;
                }
            }
        }

        if self.sidebar.step_animation(dt) {
            ctx.request_repaint();
        }

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(18, 20, 26);
        ctx.set_visuals(visuals);

        let screen = ctx.screen_rect().size();
        self.top_layout = self.prefs.layout.top_bar([screen.x, screen.y], self.top_layout);
        if self.top_layout {
            // Écran portrait : une barre d'une ligne en haut, le tiroir (sommaire, résultats) en bas.
            // Toujours affichée : Ctrl+B ne concerne que la barre latérale du mode paysage.
            egui::TopBottomPanel::top("kite_top_bar")
                .exact_height(TOP_BAR_HEIGHT)
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(23, 26, 34))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 50, 66)))
                        .inner_margin(Margin::symmetric(8, 0)),
                )
                .show(ctx, |ui| self.render_top_bar(ui));
            self.render_bottom_drawer(ctx);
        } else if self.sidebar.pinned {
            // Largeur fixe : la poignée d'egui est recouverte par la zone du document, qui capte le
            // glissé ; une poignée propre, au premier plan, la remplace (voir `render_sidebar_resize_handle`).
            egui::SidePanel::left("kite_sidebar_pinned")
                .exact_width(self.sidebar.width)
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(23, 26, 34))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 50, 66))),
                )
                .show(ctx, |ui| {
                    // La place est réservée d'un coup (le document ne bouge qu'une fois) et le contenu
                    // glisse à l'intérieur du panneau, coupé à son bord.
                    let panel_rect = ui.max_rect();
                    let slide = -self.sidebar.width * (1.0 - self.sidebar.anim_progress);
                    let mut content_ui = ui.new_child(
                        egui::UiBuilder::new().max_rect(panel_rect.translate(Vec2::new(slide, 0.0))),
                    );
                    content_ui.set_clip_rect(panel_rect);
                    self.render_sidebar_contents(&mut content_ui);
                    // Sans cela, le panneau étirable se réduirait à la largeur minimale (le contenu
                    // est dessiné dans un enfant décalé, qui ne compte pas dans sa taille).
                    ui.expand_to_include_rect(panel_rect);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(18, 20, 26)))
            .show(ctx, |ui| {
                let canvas_rect = ui.max_rect();
                self.canvas_size = canvas_rect.size();
                let _response = ui.allocate_rect(canvas_rect, Sense::click_and_drag());

                self.handle_inputs(ctx, canvas_rect);
                self.handle_scrollbar(ctx, canvas_rect);

                let image_ocr = self.prefs.image_ocr;
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if tab.is_hibernated() {
                        let _ = tab.ensure_awake(self.engine);
                    }
                    if tab.text_selection.copied_feedback_frames > 0 {
                        tab.text_selection.copied_feedback_frames -= 1;
                    }
                    if tab.annotations.saved_feedback_frames > 0 {
                        tab.annotations.saved_feedback_frames -= 1;
                    }
                    if tab.viewport.is_panning {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    } else if self.active_tool == ActiveTool::Pan {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                    // Phase 2/3 : Éco uniquement au repos, Turbo en continu. Le calcul se fait sur le thread OCR.
                    // Selon les réglages : en continu, seulement pendant une recherche, ou jamais.
                    let is_idle = tab.viewport.inertia_velocity.length_sq() < 0.1;
                    let searching = tab.ocr.mode == crate::ocr_engine::OcrSpeedMode::TurboSearch;
                    let read_now = match image_ocr {
                        crate::settings::ImageOcr::Auto => is_idle || searching,
                        crate::settings::ImageOcr::OnSearch => searching,
                        crate::settings::ImageOcr::Never => false,
                    };
                    if read_now {
                        let vis_p = tab
                            .viewport
                            .current_page_index(canvas_rect.size(), &tab.page_dimensions);
                        tab.pump_ocr(vis_p, &self.workers);
                    }
                }

                let visible_pages_info: Vec<(usize, Rect)> =
                    if let Some(tab) = self.tabs.get(self.active_tab) {
                        let (layouts, _, _) = tab.viewport.compute_layout(&tab.page_dimensions);
                        let expanded_canvas = canvas_rect.expand(140.0);
                        layouts
                            .into_iter()
                            .filter_map(|layout| {
                                let r = tab.viewport.page_screen_rect(&layout, canvas_rect);
                                if expanded_canvas.intersects(r) {
                                    Some((layout.page_index, r))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                // Le texte des pages à l'écran est prêt avant le premier clic (sélection, notes).
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    for (p_idx, _) in &visible_pages_info {
                        tab.request_page_text(*p_idx, &self.workers, false);
                        tab.ensure_form_widgets(*p_idx, self.engine);
                    }
                }

                for (p_idx, p_rect) in &visible_pages_info {
                    if self.handle_form_interaction(ctx, *p_idx, *p_rect) {
                        continue;
                    }
                    self.handle_page_canvas_interactions(ctx, *p_idx, *p_rect);
                }
                // Un relâchement hors de toute page doit aussi terminer le geste.
                if ctx.input(|i| i.pointer.primary_released()) {
                    self.annotation_drag = None;
                }

                let painter = ui.painter_at(canvas_rect);

                if self.tabs.is_empty() && !self.command_palette.is_open && !self.welcome_open {
                    painter.text(
                        canvas_rect.center() - Vec2::new(0.0, 14.0),
                        Align2::CENTER_CENTER,
                        "Aucun PDF ouvert",
                        FontId::proportional(20.0),
                        Color32::from_rgb(170, 180, 200),
                    );
                    painter.text(
                        canvas_rect.center() + Vec2::new(0.0, 16.0),
                        Align2::CENTER_CENTER,
                        "Ctrl+T : rouvrir un PDF récent      Ctrl+O : parcourir le disque",
                        FontId::proportional(13.0),
                        Color32::from_rgb(120, 130, 152),
                    );
                }
                if let Some(err) = &self.error_message {
                    // Message centré, sur plusieurs lignes si la fenêtre est étroite.
                    let color = Color32::from_rgb(255, 120, 120);
                    let wrap = (canvas_rect.width() - 64.0).clamp(200.0, 640.0);
                    let galley = painter.layout(err.clone(), FontId::proportional(16.0), color, wrap);
                    let center = canvas_rect.center() + Vec2::new(0.0, if self.tabs.is_empty() { 60.0 } else { 0.0 });
                    painter.galley(center - galley.size() * 0.5, galley, color);
                } else if let Some(tab) = self.tabs.get(self.active_tab) {
                    let (_, _, total_h) = tab.viewport.compute_layout(&tab.page_dimensions);
                    // Le rendu suit les pixels physiques de l'écran (échelle Windows comprise), et n'est
                    // recalculé qu'une fois le geste de zoom terminé : l'image existante est étirée entre-temps.
                    let pixels_per_point = ctx.pixels_per_point();
                    let live_zoom = tab.viewport.zoom * pixels_per_point;
                    if (live_zoom - self.observed_zoom).abs() > 1e-4 {
                        self.observed_zoom = live_zoom;
                        self.observed_zoom_changed_at = Instant::now();
                    }
                    if self.render_zoom <= 0.0
                        || self.observed_zoom_changed_at.elapsed() >= ZOOM_SETTLE_DELAY
                    {
                        self.render_zoom = live_zoom;
                    } else {
                        ctx.request_repaint_after(ZOOM_SETTLE_DELAY);
                    }
                    let device_zoom = self.render_zoom;

                    let page_pixels_at = |page_index: usize, zoom: f32| -> f32 {
                        let dim = tab.page_dimensions[page_index];
                        dim.width_pt * dim.height_pt * (BASE_DPI_SCALE * zoom).powi(2)
                    };
                    let largest_page_pixels = visible_pages_info
                        .iter()
                        .map(|(p, _)| page_pixels_at(*p, device_zoom))
                        .fold(0.0_f32, f32::max);
                    let mut use_tiles =
                        tab.supports_tiled_rendering && largest_page_pixels > TILING_THRESHOLD_PIXELS;

                    // Sans tuiles, les pages visibles doivent tenir ensemble dans le budget :
                    // sinon elles s'évinceraient mutuellement en boucle.
                    let budget = self.texture_cache.max_budget_bytes as f32;
                    let visible_page_budget = budget * VISIBLE_TEXTURE_BUDGET_RATIO;
                    let visible_bytes: f32 = visible_pages_info
                        .iter()
                        .map(|(p, _)| page_pixels_at(*p, device_zoom) * 4.0)
                        .sum();

                    // Grille de tuiles couvrant la zone visible, plus une rangée de préchargement.
                    let tile_bucket = Self::zoom_bucket(device_zoom);
                    let tile_zoom = tile_bucket as f32 / ZOOM_BUCKETS_PER_UNIT;
                    let tile_scale = BASE_DPI_SCALE * tile_zoom;
                    let mut page_tiles: Vec<PageTile> = Vec::new();
                    if use_tiles {
                        for &(page_index, page_rect) in &visible_pages_info {
                            let clip = page_rect.intersect(canvas_rect);
                            if !clip.is_positive() {
                                continue;
                            }
                            let dim = tab.page_dimensions[page_index];
                            let full_w = (dim.width_pt * tile_scale).round() as i32;
                            let full_h = (dim.height_pt * tile_scale).round() as i32;
                            let px_per_screen_point = full_w as f32 / page_rect.width().max(1.0);
                            let to_tile = |points: f32| {
                                ((points * px_per_screen_point) / TILE_SIZE_PX as f32).floor() as i32
                            };
                            let first_col = to_tile(clip.min.x - page_rect.min.x);
                            let last_col = to_tile(clip.max.x - page_rect.min.x);
                            let first_row = to_tile(clip.min.y - page_rect.min.y);
                            let last_row = to_tile(clip.max.y - page_rect.min.y);
                            for row in (first_row - 1).max(0)..=(last_row + 1) {
                                for col in (first_col - 1).max(0)..=(last_col + 1) {
                                    let (x, y) = (col * TILE_SIZE_PX, row * TILE_SIZE_PX);
                                    if x >= full_w || y >= full_h {
                                        continue;
                                    }
                                    let region = TileRegion {
                                        x,
                                        y,
                                        width: TILE_SIZE_PX.min(full_w - x),
                                        height: TILE_SIZE_PX.min(full_h - y),
                                    };
                                    let center = page_rect.min
                                        + Vec2::new(
                                            (x + region.width / 2) as f32,
                                            (y + region.height / 2) as f32,
                                        ) / px_per_screen_point;
                                    page_tiles.push(PageTile {
                                        page_index,
                                        key: PageTextureKey {
                                            tab_id: tab.id,
                                            page_index,
                                            zoom_bucket: tile_bucket,
                                            tile: Some((col as u32, row as u32)),
                                        },
                                        region,
                                        full_size_px: (full_w as f32, full_h as f32),
                                        distance_to_center: center.distance(canvas_rect.center()),
                                        is_visible: (first_row..=last_row).contains(&row)
                                            && (first_col..=last_col).contains(&col),
                                    });
                                }
                            }
                        }
                        let tile_bytes = (TILE_SIZE_PX * TILE_SIZE_PX * 4) as f32;
                        let tile_budget = budget * TILE_BUDGET_RATIO;
                        if page_tiles.len() as f32 * tile_bytes > tile_budget {
                            page_tiles.retain(|tile| tile.is_visible);
                        }
                        if page_tiles.len() as f32 * tile_bytes > tile_budget {
                            page_tiles.clear();
                            use_tiles = false;
                        }
                        page_tiles.sort_by(|a, b| a.distance_to_center.total_cmp(&b.distance_to_center));
                    }

                    let page_zoom = if use_tiles {
                        device_zoom * (TILED_BASE_PIXELS / largest_page_pixels.max(1.0)).sqrt()
                    } else {
                        device_zoom * (visible_page_budget / visible_bytes.max(1.0)).sqrt().min(1.0)
                    };
                    let bucket = Self::zoom_bucket(page_zoom);
                    let bucket_zoom = (bucket as f32) / ZOOM_BUCKETS_PER_UNIT;

                    let all_ready = visible_pages_info.iter().all(|(p, _)| {
                        self.texture_cache.contains_key(&PageTextureKey {
                            tab_id: tab.id,
                            page_index: *p,
                            zoom_bucket: bucket,
                            tile: None,
                        })
                    }) && page_tiles
                        .iter()
                        .filter(|tile| tile.is_visible)
                        .all(|tile| self.texture_cache.contains_key(&tile.key));
                    if all_ready && tab.viewport.inertia_velocity.length_sq() < 0.1 {
                        self.texture_cache.prune_stale_zoom_buckets(
                            tab.id,
                            bucket,
                            use_tiles.then_some(tile_bucket),
                        );
                    }

                    // Pages à rendre : visibles d'abord (les plus centrales en tête), puis les voisines.
                    let make_job = |page_index: usize| RenderJob {
                        key: PageTextureKey {
                            tab_id: tab.id,
                            page_index,
                            zoom_bucket: bucket,
                            tile: None,
                        },
                        path: tab.path.clone(),
                        width_px: (tab.page_dimensions[page_index].width_pt * BASE_DPI_SCALE * bucket_zoom)
                            .round() as i32,
                        height_px: (tab.page_dimensions[page_index].height_pt * BASE_DPI_SCALE * bucket_zoom)
                            .round() as i32,
                        tile: None,
                        tile_scale: 0.0,
                    };
                    let mut wanted_pages: Vec<(usize, f32)> = visible_pages_info
                        .iter()
                        .map(|(p, r)| (*p, (r.center().y - canvas_rect.center().y).abs()))
                        .collect();
                    wanted_pages.sort_by(|a, b| a.1.total_cmp(&b.1));
                    let mut wanted: Vec<usize> = wanted_pages.into_iter().map(|(p, _)| p).collect();
                    if let (Some(first), Some(last)) = (
                        visible_pages_info.first().map(|(p, _)| *p),
                        visible_pages_info.last().map(|(p, _)| *p),
                    ) {
                        let shown_bytes: f32 = visible_pages_info
                            .iter()
                            .map(|(p, _)| page_pixels_at(*p, bucket_zoom) * 4.0)
                            .sum();
                        let preload_bytes: f32 = [first.checked_sub(1), Some(last + 1)]
                            .into_iter()
                            .flatten()
                            .filter(|p| *p < tab.page_dimensions.len())
                            .map(|p| page_pixels_at(p, bucket_zoom) * 4.0)
                            .sum();
                        if shown_bytes + preload_bytes <= visible_page_budget {
                            wanted.extend(first.checked_sub(1));
                            if last + 1 < tab.page_dimensions.len() {
                                wanted.push(last + 1);
                            }
                        }
                    }
                    let mut render_jobs: Vec<RenderJob> = wanted.into_iter().map(make_job).collect();
                    render_jobs.extend(page_tiles.iter().map(|tile| RenderJob {
                        key: tile.key,
                        path: tab.path.clone(),
                        width_px: tile.region.width,
                        height_px: tile.region.height,
                        tile: Some(tile.region),
                        tile_scale,
                    }));
                    render_jobs.retain(|job| {
                        !self.texture_cache.contains_key(&job.key)
                            && !self.failed_renders.contains(&job.key)
                    });
                    self.workers.set_render_jobs(render_jobs);

                    for &(page_index, page_rect) in &visible_pages_info {
                        // Ombre portée sous la feuille
                        let shadow_rect = page_rect.translate(Vec2::new(0.0, 4.0)).expand(2.0);
                        painter.rect_filled(
                            shadow_rect,
                            CornerRadius::same(3),
                            Color32::from_rgba_premultiplied(0, 0, 0, 110),
                        );

                        // Fond blanc de la feuille
                        painter.rect_filled(page_rect, CornerRadius::same(2), Color32::WHITE);

                        let page_dim = tab.page_dimensions[page_index];
                        let inv_w = 1.0 / page_dim.width_pt.max(1.0);
                        let inv_h = 1.0 / page_dim.height_pt.max(1.0);

                        // Collecte de tous les surlignages multiplicatifs (/BM /Multiply) de la page
                        // pour que l'écriture noire traverse le surligneur à 100 % en noir pur !
                        let mut mult_highlights: Vec<MultiplicativeHighlightQuad> = Vec::new();
                        for annot in tab
                            .annotations
                            .items
                            .iter()
                            .filter(|a| a.page_index == page_index)
                        {
                            let tinted_quads: &[PdfTopDownRect] = match &annot.kind {
                                AnnotationKind::Highlight { quads } => quads,
                                AnnotationKind::StickyNote { anchor_quads, .. } => anchor_quads,
                                _ => &[],
                            };
                            for q in tinted_quads {
                                mult_highlights.push(MultiplicativeHighlightQuad {
                                    norm_rect: [
                                        (q.x_min - 1.0) * inv_w,
                                        (q.y_min - 1.0) * inv_h,
                                        (q.x_max + 1.0) * inv_w,
                                        (q.y_max + 1.0) * inv_h,
                                    ],
                                    color_rgb: annot.color_rgb,
                                });
                            }
                        }

                        // Surlignage multiplicatif pour la sélection de texte active (Curseur I)
                        if tab.text_selection.page_index == Some(page_index) {
                            let sel_rgb = if self.active_tool == ActiveTool::Highlight {
                                tab.annotations.active_color_rgb
                            } else {
                                (165, 212, 255)
                            };
                            for b in &tab.text_selection.highlight_boxes {
                                mult_highlights.push(MultiplicativeHighlightQuad {
                                    norm_rect: [
                                        (b.x_min - 1.0) * inv_w,
                                        (b.y_min - 1.0) * inv_h,
                                        (b.x_max + 1.0) * inv_w,
                                        (b.y_max + 1.0) * inv_h,
                                    ],
                                    color_rgb: sel_rgb,
                                });
                            }
                        }

                        // Surlignage multiplicatif pour les résultats de recherche `Ctrl+F`
                        if self.search_bar.is_open && !self.search_bar.matches.is_empty() {
                            for (m_idx, m) in self.search_bar.matches.iter().enumerate() {
                                if m.page_index != page_index {
                                    continue;
                                }
                                let is_active_match = m_idx == self.search_bar.current_match_index;
                                let match_rgb = if is_active_match {
                                    (255, 195, 75)
                                } else {
                                    (255, 238, 105)
                                };
                                for b in &m.highlight_boxes {
                                    mult_highlights.push(MultiplicativeHighlightQuad {
                                        norm_rect: [
                                            (b.x_min - 1.5) * inv_w,
                                            (b.y_min - 1.5) * inv_h,
                                            (b.x_max + 1.5) * inv_w,
                                            (b.y_max + 1.5) * inv_h,
                                        ],
                                        color_rgb: match_rgb,
                                    });
                                }
                            }
                        }

                        let key = PageTextureKey {
                            tab_id: tab.id,
                            page_index,
                            zoom_bucket: bucket,
                            tile: None,
                        };

                        // Aligné sur les pixels physiques : une image décalée d'un demi-pixel serait floue.
                        let snap = |value: f32| (value * pixels_per_point).round() / pixels_per_point;
                        let image_rect = Rect::from_min_size(
                            Pos2::new(snap(page_rect.min.x), snap(page_rect.min.y)),
                            page_rect.size(),
                        );
                        let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

                        // En attendant le rendu exact, on étire le palier de zoom le plus proche déjà en cache.
                        let draw_key = if self.texture_cache.contains_key(&key) {
                            Some(key)
                        } else {
                            self.texture_cache
                                .closest_cached_key(tab.id, page_index, bucket)
                        };
                        if let Some(tex) = draw_key.and_then(|k| {
                            self.texture_cache.get_with_highlights(&k, &mult_highlights)
                        }) {
                            painter.image(tex.id(), image_rect, full_uv, Color32::WHITE);
                        }

                        for tile in page_tiles.iter().filter(|tile| tile.page_index == page_index) {
                            let (full_w, full_h) = tile.full_size_px;
                            let normalized = [
                                tile.region.x as f32 / full_w,
                                tile.region.y as f32 / full_h,
                                (tile.region.x + tile.region.width) as f32 / full_w,
                                (tile.region.y + tile.region.height) as f32 / full_h,
                            ];
                            let tile_rect = Rect::from_min_max(
                                Pos2::new(
                                    snap(image_rect.min.x + normalized[0] * image_rect.width()),
                                    snap(image_rect.min.y + normalized[1] * image_rect.height()),
                                ),
                                Pos2::new(
                                    snap(image_rect.min.x + normalized[2] * image_rect.width()),
                                    snap(image_rect.min.y + normalized[3] * image_rect.height()),
                                ),
                            );
                            let tile_highlights =
                                MultiplicativeHighlightQuad::remap_to_region(&mult_highlights, normalized);
                            if let Some(tex) = self
                                .texture_cache
                                .get_with_highlights(&tile.key, &tile_highlights)
                            {
                                painter.image(tex.id(), tile_rect, full_uv, Color32::WHITE);
                            }
                        }

                        // --- 1. DESSIN DES ANNOTATIONS ISO 32000 SUR LA PAGE (JALON 5) ---
                        Self::paint_page_annotations(
                            &painter,
                            &tab.annotations.items,
                            tab.annotations.selected_id,
                            self.editing_freetext_id,
                            page_index,
                            page_rect,
                            page_dim,
                            ctx.input(|i| i.pointer.hover_pos()),
                        );

                        // Tracé temporaire en cours de dessin au Trackpad / Souris
                        if tab.annotations.active_draft_page == Some(page_index)
                            && tab.annotations.active_draft_points.len() >= 2
                        {
                            let sx = page_rect.width() / page_dim.width_pt.max(1.0);
                            let sy = page_rect.height() / page_dim.height_pt.max(1.0);
                            let screen_pts: Vec<Pos2> = tab
                                .annotations
                                .active_draft_points
                                .iter()
                                .map(|&(x, y)| {
                                    Pos2::new(page_rect.min.x + x * sx, page_rect.min.y + y * sy)
                                })
                                .collect();
                            let (r, g, b) = tab.annotations.active_color_rgb;
                            painter.add(egui::Shape::line(
                                screen_pts,
                                Stroke::new(2.5_f32, Color32::from_rgb(r, g, b)),
                            ));
                        }

                        // --- 2. DESSIN DES SURBRILLANCES DE RECHERCHE `Ctrl+F` (JALON 4) ---
                        if self.search_bar.is_open && !self.search_bar.matches.is_empty() {
                            for (m_idx, m) in self.search_bar.matches.iter().enumerate() {
                                if m.page_index != page_index {
                                    continue;
                                }
                                let is_active_match = m_idx == self.search_bar.current_match_index;
                                let stroke_col = if is_active_match {
                                    Color32::from_rgb(235, 75, 0)
                                } else {
                                    Color32::from_rgb(210, 165, 0)
                                };

                                for b in &m.highlight_boxes {
                                    let screen_b = b.to_screen_rect(page_rect, page_dim).expand(2.5);
                                    // Le fond est teinté par mélange multiplicatif (/BM /Multiply) : on ne trace que le contour !
                                    painter.rect_stroke(
                                        screen_b,
                                        CornerRadius::same(3),
                                        Stroke::new(
                                            if is_active_match { 2.0_f32 } else { 1.0_f32 },
                                            stroke_col,
                                        ),
                                        StrokeKind::Outside,
                                    );
                                }

                                if is_active_match {
                                    if let Some(first_b) = m.highlight_boxes.first() {
                                        let screen_b = first_b.to_screen_rect(page_rect, page_dim);
                                        let where_found = if m.is_ocr { " · dans une image" } else { "" };
                                        let label = format!(
                                            "🔎 {} / {}{where_found}",
                                            m_idx + 1,
                                            self.search_bar.matches.len()
                                        );
                                        let badge_w = (label.chars().count() as f32 * 6.8).max(140.0);
                                        let badge_rect = Rect::from_min_size(
                                            Pos2::new(
                                                screen_b.min.x,
                                                (screen_b.min.y - 24.0).max(page_rect.min.y + 4.0),
                                            ),
                                            Vec2::new(badge_w, 20.0),
                                        );
                                        painter.rect_filled(
                                            badge_rect,
                                            CornerRadius::same(5),
                                            if m.is_ocr {
                                                Color32::from_rgb(195, 45, 115)
                                            } else {
                                                Color32::from_rgb(225, 75, 15)
                                            },
                                        );
                                        painter.text(
                                            badge_rect.center(),
                                            Align2::CENTER_CENTER,
                                            label,
                                            FontId::proportional(11.0),
                                            Color32::WHITE,
                                        );
                                    }
                                }
                            }
                        }

                        // --- 3. DESSIN DE LA SÉLECTION DE TEXTE (CURSEUR I OU ALT+RECTANGLE) ---
                        if tab.text_selection.page_index == Some(page_index) {
                            if let Some(drag_r) = tab.text_selection.active_drag_rect() {
                                if drag_r.width() > 3.0 || drag_r.height() > 3.0 {
                                    let screen_drag = drag_r.to_screen_rect(page_rect, page_dim);
                                    painter.rect_stroke(
                                        screen_drag,
                                        CornerRadius::same(2),
                                        Stroke::new(
                                            1.0_f32,
                                            Color32::from_rgba_unmultiplied(60, 140, 255, 180),
                                        ),
                                        StrokeKind::Outside,
                                    );
                                }
                            }

                            // Le fond des boîtes sélectionnées est teinté par mélange multiplicatif (/BM /Multiply),
                            // ce qui laisse le texte noir 100 % net et lisible !

                        }

                        painter.rect_stroke(
                            page_rect,
                            CornerRadius::same(2),
                            Stroke::new(1.0_f32, Color32::from_rgb(55, 60, 72)),
                            StrokeKind::Outside,
                        );
                    }

                    // Ascenseur vertical : s'élargit quand il est survolé ou saisi.
                    if let Some(bar) =
                        ScrollbarGeometry::compute(canvas_rect, total_h, tab.viewport.scroll_offset.y)
                    {
                        let is_active = self.pointer_on_scrollbar || self.scrollbar_grab.is_some();
                        let (bar_width, alpha) = if is_active { (10.0, 220) } else { (4.0, 140) };
                        let thumb_rect = Rect::from_min_size(
                            Pos2::new(canvas_rect.max.x - 4.0 - bar_width, bar.thumb_top),
                            Vec2::new(bar_width, bar.thumb_height),
                        );
                        if is_active {
                            painter.rect_filled(
                                Rect::from_min_max(
                                    Pos2::new(thumb_rect.min.x, bar.track_top),
                                    Pos2::new(thumb_rect.max.x, bar.track_top + bar.track_height),
                                ),
                                CornerRadius::same(4),
                                Color32::from_rgba_unmultiplied(120, 130, 150, 45),
                            );
                        }
                        painter.rect_filled(
                            thumb_rect,
                            CornerRadius::same(4),
                            Color32::from_rgba_unmultiplied(150, 170, 210, alpha),
                        );
                    }
                }


                self.paint_form_fields(&ui.painter_at(canvas_rect), &visible_pages_info);
                self.render_form_editor(ctx, &visible_pages_info);
                self.render_form_choice(ctx, &visible_pages_info);
                self.render_freetext_editor(ctx, &visible_pages_info);
                self.render_comment_panel(ctx, canvas_rect, &visible_pages_info);
                self.render_line_context_menu(ctx);
                self.render_save_error_popup(ctx);
                self.render_dependency_warning(ctx, canvas_rect);
                self.render_update_banner(ctx, canvas_rect);
                self.render_settings(ctx);
                self.render_help(ctx);

                // Barre de recherche flottante `Ctrl+F`
                if self.search_bar.is_open {
                    self.render_floating_search_bar(ctx, canvas_rect);
                }

                if !self.sidebar.pinned && !self.top_layout {
                    self.render_autohide_sidebar_overlay(ctx, canvas_rect);
                }
                if !self.top_layout {
                    self.render_sidebar_resize_handle(ctx);
                }


                self.render_no_text_hint(ctx);
                self.render_password_prompt(ctx);
                self.render_print_status(ctx, canvas_rect);
                self.render_deep_ocr_result(ctx, canvas_rect);
                self.render_deep_ocr_dialog(ctx);
                if self.command_palette.is_open {
                    self.render_command_palette(ctx, canvas_rect);
                }
            });

        if self.automation.screenshot_output.is_some() {
            let waiting_for_pages = self.workers.is_render_busy()
                && self.last_frame_instant.duration_since(self.app_started) < Duration::from_secs(20);
            let delay_running = self.app_started.elapsed()
                < Duration::from_millis(self.automation.screenshot_delay_ms);
            if self.frame_count < 5 || waiting_for_pages || delay_running {
                ctx.request_repaint();
            } else if !self.screenshot_requested {
                self.screenshot_requested = true;
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
                ctx.request_repaint();
            }
        }
    }
}

impl KiteApp {
    /// Intègre les résultats des threads d'arrière-plan (textures, texte, OCR).
    fn process_worker_events(&mut self, ctx: &Context) {
        while let Some(event) = self.workers.try_next_event() {
            match event {
                WorkerEvent::Rendered { key, image } => {
                    // `KITE_TIMING=1` : temps entre le lancement et la première image de page.
                    if !self.first_render_reported && std::env::var_os("KITE_TIMING").is_some() {
                        self.first_render_reported = true;
                        println!(
                            "Première page affichée {:.0} ms après le lancement",
                            crate::PROCESS_START
                                .get()
                                .unwrap_or(&self.app_started)
                                .elapsed()
                                .as_secs_f64()
                                * 1000.0
                        );
                    }
                    if self.tabs.iter().any(|t| t.id == key.tab_id) {
                        self.texture_cache.insert_color_image(ctx, key, image, &[]);
                    }
                }
                WorkerEvent::RenderFailed { key } => {
                    self.failed_renders.insert(key);
                }
                WorkerEvent::PageDetails { tab_id, pages } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.apply_page_details(pages);
                    }
                }
                WorkerEvent::PageDetailsDone { tab_id } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.supports_tiled_rendering = !tab.found_form_widgets;
                        if tab.found_app_annotations {
                            self.workers.load_annotations(
                                tab_id,
                                tab.path.clone(),
                                tab.page_dimensions.clone(),
                            );
                        }
                    }
                }
                WorkerEvent::DeepOcrPage { tab_id, page, segments } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.apply_deep_ocr_page(page, segments);
                        self.search_refresh_pending = true;
                    }
                }
                WorkerEvent::OutlineReady { tab_id, entries } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.outline = Some(entries);
                    }
                }
                WorkerEvent::AnnotationsLoaded { tab_id, items } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.annotations.merge_loaded(items);
                    }
                }
                WorkerEvent::TextLayer { tab_id, layer } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.apply_text_layer(layer);
                        self.search_refresh_pending = true;
                    }
                }
                WorkerEvent::OcrPage(outcome) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == outcome.tab_id) {
                        if tab.apply_ocr_outcome(outcome) {
                            self.search_refresh_pending = true;
                        }
                    }
                }
            }
        }

        if self.search_refresh_pending && self.search_bar.is_open {
            if self.last_search_refresh.elapsed() >= SEARCH_REFRESH_INTERVAL {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    self.search_bar.refresh_keeping_current(&tab.text_layers);
                }
                self.search_refresh_pending = false;
                self.last_search_refresh = Instant::now();
            } else {
                ctx.request_repaint_after(SEARCH_REFRESH_INTERVAL);
            }
        }
    }

    /// Dessine toutes les annotations ISO 32000 d'une page sur le canevas GPU
    fn paint_page_annotations(
        painter: &egui::Painter,
        annotations: &[PdfAnnotation],
        selected_id: Option<u64>,
        editing_id: Option<u64>,
        page_index: usize,
        page_rect: Rect,
        page_dim: crate::pdf_engine::PageDimensions,
        hover_pos: Option<Pos2>,
    ) {
        let sx = page_rect.width() / page_dim.width_pt.max(1.0);
        let sy = page_rect.height() / page_dim.height_pt.max(1.0);
        let to_screen_pos = |pt: (f32, f32)| -> Pos2 {
            Pos2::new(page_rect.min.x + pt.0 * sx, page_rect.min.y + pt.1 * sy)
        };

        for annot in annotations.iter().filter(|a| a.page_index == page_index) {
            let is_selected = selected_id == Some(annot.id);
            let screen_rect = annot.rect.to_screen_rect(page_rect, page_dim);
            let stroke_col = annot.egui_color(240);

            match &annot.kind {
                AnnotationKind::Highlight { .. } => {
                    // Rendu via mélange multiplicatif (/BM /Multiply) directement dans la texture de page :
                    // l'encre noire du texte traverse le surligneur à 100 % en noir pur !
                }
                AnnotationKind::FreeText {
                    text,
                    font_size,
                    text_rgb,
                    fill_rgb,
                    border_rgb,
                } => {
                    if let Some((r, g, b)) = fill_rgb {
                        painter.rect_filled(
                            screen_rect,
                            CornerRadius::ZERO,
                            Color32::from_rgb(*r, *g, *b),
                        );
                    }
                    if let Some((r, g, b)) = border_rgb {
                        painter.rect_stroke(
                            screen_rect,
                            CornerRadius::ZERO,
                            Stroke::new(1.5 * sy, Color32::from_rgb(*r, *g, *b)),
                            StrokeKind::Inside,
                        );
                    }
                    if editing_id != Some(annot.id) {
                        let galley = painter.layout(
                            text.clone(),
                            FontId::proportional(font_size * sy),
                            Color32::from_rgb(text_rgb.0, text_rgb.1, text_rgb.2),
                            (screen_rect.width() - 12.0 * sx).max(10.0),
                        );
                        painter.with_clip_rect(screen_rect).galley(
                            screen_rect.min + Vec2::new(6.0 * sx, 4.0 * sy),
                            galley,
                            Color32::from_rgb(20, 24, 34),
                        );
                    }
                }
                AnnotationKind::Ink { strokes, thickness } => {
                    for stroke in strokes {
                        if stroke.len() >= 2 {
                            let pts: Vec<Pos2> =
                                stroke.iter().copied().map(to_screen_pos).collect();
                            painter.add(egui::Shape::line(
                                pts,
                                Stroke::new((*thickness * sy).max(0.8), stroke_col),
                            ));
                        }
                    }
                }
                AnnotationKind::Shape {
                    shape_kind,
                    start_pt,
                    end_pt,
                } => match shape_kind {
                    ShapeKind::Rectangle => {
                        painter.rect_filled(
                            screen_rect,
                            CornerRadius::same(2),
                            annot.egui_color(35),
                        );
                        painter.rect_stroke(
                            screen_rect,
                            CornerRadius::same(2),
                            Stroke::new(2.0_f32, stroke_col),
                            StrokeKind::Outside,
                        );
                    }
                    ShapeKind::Ellipse => {
                        let (radius_x, radius_y) =
                            (screen_rect.width() * 0.5, screen_rect.height() * 0.5);
                        let center = screen_rect.center();
                        let mut outline: Vec<Pos2> = (0..64)
                            .map(|i| {
                                let angle = i as f32 / 64.0 * std::f32::consts::TAU;
                                center + Vec2::new(angle.cos() * radius_x, angle.sin() * radius_y)
                            })
                            .collect();
                        outline.push(outline[0]);
                        painter.add(egui::Shape::line(outline, Stroke::new(2.0_f32, stroke_col)));
                    }
                    ShapeKind::ArrowLine {
                        start_head,
                        end_head,
                    } => {
                        let dir_to_end = unit_direction(*start_pt, *end_pt);
                        let dir_to_start = (-dir_to_end.0, -dir_to_end.1);
                        let stroke = Stroke::new(ARROW_LINE_WIDTH * sy, stroke_col);
                        painter.line_segment(
                            [
                                to_screen_pos(line_stop_point(*start_pt, dir_to_start, *start_head)),
                                to_screen_pos(line_stop_point(*end_pt, dir_to_end, *end_head)),
                            ],
                            stroke,
                        );
                        let endings = [
                            (*start_head, *start_pt, dir_to_start),
                            (*end_head, *end_pt, dir_to_end),
                        ];
                        for (ending, tip, dir) in endings {
                            let Some(outline) = line_ending_outline(ending, tip, dir) else {
                                continue;
                            };
                            let points: Vec<Pos2> =
                                outline.points.iter().copied().map(to_screen_pos).collect();
                            if outline.filled {
                                painter.add(egui::Shape::convex_polygon(
                                    points,
                                    stroke_col,
                                    Stroke::NONE,
                                ));
                            } else {
                                painter.add(egui::Shape::line(points, stroke));
                            }
                        }
                    }
                },
                AnnotationKind::StickyNote {
                    comment,
                    anchor_quads,
                    ..
                } => {
                    let anchored = !anchor_quads.is_empty();
                    let hovered = hover_pos.is_some_and(|p| {
                        if anchored {
                            anchor_quads
                                .iter()
                                .any(|q| q.to_screen_rect(page_rect, page_dim).expand(2.0).contains(p))
                        } else {
                            screen_rect.expand(2.0).contains(p)
                        }
                    });
                    let marker_font = FontId::proportional((11.0 * sy).clamp(10.0, 18.0));
                    match anchor_quads.last() {
                        Some(last) => {
                            let end = last.to_screen_rect(page_rect, page_dim);
                            painter.text(
                                Pos2::new(end.max.x + 2.0, end.min.y),
                                Align2::LEFT_BOTTOM,
                                "💬",
                                marker_font,
                                stroke_col,
                            );
                        }
                        None => {
                            painter.text(
                                screen_rect.center(),
                                Align2::CENTER_CENTER,
                                "💬",
                                marker_font,
                                stroke_col,
                            );
                        }
                    }

                    // Aperçu au survol ; la bulle complète s'ouvre à la sélection.
                    if !hovered || is_selected {
                        continue;
                    }
                    let origin = comment_panel_origin(annot, page_rect, page_dim);
                    let inner_width = COMMENT_PANEL_WIDTH - 20.0;
                    let date = format_pdf_date(if annot.meta.modified.is_empty() {
                        &annot.meta.created
                    } else {
                        &annot.meta.modified
                    });
                    let header = painter.layout_no_wrap(
                        format!("{}   {}", annot.meta.author, date),
                        FontId::proportional(11.5),
                        INLINE_TEXT_COLOR,
                    );
                    let body = painter.layout(
                        comment.clone(),
                        FontId::proportional(13.0),
                        INLINE_TEXT_COLOR,
                        inner_width,
                    );
                    let replies_text = match annot.meta.replies.len() {
                        0 => String::new(),
                        1 => "1 réponse".to_string(),
                        n => format!("{n} réponses"),
                    };
                    let replies = painter.layout_no_wrap(
                        replies_text,
                        FontId::proportional(11.0),
                        COMMENT_MUTED_TEXT,
                    );
                    let height = 24.0 + header.size().y + body.size().y + replies.size().y;
                    let bubble = Rect::from_min_size(origin, Vec2::new(COMMENT_PANEL_WIDTH, height));
                    if let Some(first) = anchor_quads.first() {
                        let start = first.to_screen_rect(page_rect, page_dim).center();
                        painter.line_segment(
                            [start, bubble.clamp(start)],
                            Stroke::new(1.0_f32, stroke_col),
                        );
                    }
                    painter.rect_filled(bubble, CornerRadius::same(4), COMMENT_PANEL_BACKGROUND);
                    painter.rect_stroke(
                        bubble,
                        CornerRadius::same(4),
                        Stroke::new(1.2_f32, stroke_col),
                        StrokeKind::Outside,
                    );
                    let mut cursor_y = bubble.min.y + 8.0;
                    for galley in [header, body, replies] {
                        let height = galley.size().y;
                        painter.galley(
                            Pos2::new(bubble.min.x + 10.0, cursor_y),
                            galley,
                            INLINE_TEXT_COLOR,
                        );
                        cursor_y += height + 4.0;
                    }
                }
                AnnotationKind::Measure {
                    start_pt,
                    end_pt,
                    stamp_tag,
                } => {
                    let p1 = to_screen_pos(*start_pt);
                    let p2 = to_screen_pos(*end_pt);
                    painter.line_segment([p1, p2], Stroke::new(2.0_f32, stroke_col));

                    // Barres d'extrémité de cote
                    let dir = (p2 - p1).normalized();
                    let perp = if dir.length_sq() > 0.1 {
                        Vec2::new(-dir.y, dir.x) * 7.0
                    } else {
                        Vec2::new(0.0, 7.0)
                    };
                    painter.line_segment([p1 - perp, p1 + perp], Stroke::new(2.0_f32, stroke_col));
                    painter.line_segment([p2 - perp, p2 + perp], Stroke::new(2.0_f32, stroke_col));

                    let (dist_pt, dist_mm) =
                        PdfAnnotation::measure_distance_pt_and_mm(*start_pt, *end_pt);
                    let mid = p1 + (p2 - p1) * 0.5;
                    let badge_text = match stamp_tag {
                        Some(tag) => format!("📏 {:.1} mm ({:.0} pt) • {}", dist_mm, dist_pt, tag),
                        None => format!("📏 {:.1} mm ({:.0} pt)", dist_mm, dist_pt),
                    };
                    let pill_w = (badge_text.len() as f32 * 6.4).max(130.0);
                    let pill_rect = Rect::from_center_size(mid, Vec2::new(pill_w, 20.0));
                    painter.rect_filled(
                        pill_rect,
                        CornerRadius::same(10),
                        Color32::from_rgba_unmultiplied(20, 26, 38, 240),
                    );
                    painter.rect_stroke(
                        pill_rect,
                        CornerRadius::same(10),
                        Stroke::new(1.2_f32, stroke_col),
                        StrokeKind::Outside,
                    );
                    painter.text(
                        pill_rect.center(),
                        Align2::CENTER_CENTER,
                        badge_text,
                        FontId::proportional(10.5),
                        Color32::WHITE,
                    );
                }
            }

            // Sélection : deux points aux extrémités d'une ligne, cadre et coins pour le reste.
            if is_selected && !matches!(annot.kind, AnnotationKind::StickyNote { .. }) {
                const HANDLE_COLOR: Color32 = Color32::from_rgb(90, 175, 255);
                if let Some((start, end)) = annot.line_endpoints() {
                    for point in [start, end] {
                        let center = to_screen_pos(point);
                        painter.circle_filled(center, 6.0, Color32::WHITE);
                        painter.circle_stroke(center, 6.0, Stroke::new(2.0_f32, HANDLE_COLOR));
                    }
                } else {
                    painter.rect_stroke(
                        screen_rect,
                        CornerRadius::same(2),
                        Stroke::new(1.5_f32, HANDLE_COLOR),
                        StrokeKind::Outside,
                    );
                    if !matches!(annot.kind, AnnotationKind::Highlight { .. }) {
                        for corner in [
                            screen_rect.left_top(),
                            screen_rect.right_top(),
                            screen_rect.left_bottom(),
                            screen_rect.right_bottom(),
                        ] {
                            let square = Rect::from_center_size(corner, Vec2::splat(9.0));
                            painter.rect_filled(square, CornerRadius::same(1), Color32::WHITE);
                            painter.rect_stroke(
                                square,
                                CornerRadius::same(1),
                                Stroke::new(1.5_f32, HANDLE_COLOR),
                                StrokeKind::Outside,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Clic ou glissé sur l'ascenseur vertical.
    fn handle_scrollbar(&mut self, ctx: &Context, canvas_rect: Rect) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            self.pointer_on_scrollbar = false;
            return;
        };
        let (_, _, total_height) = tab.viewport.compute_layout(&tab.page_dimensions);
        let Some(bar) =
            ScrollbarGeometry::compute(canvas_rect, total_height, tab.viewport.scroll_offset.y)
        else {
            self.pointer_on_scrollbar = false;
            self.scrollbar_grab = None;
            return;
        };

        let (pointer, pressed, down) = ctx.input(|i| {
            (
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
            )
        });
        let over_floating_panel = ctx.is_pointer_over_area();
        self.pointer_on_scrollbar = self.scrollbar_grab.is_some()
            || (!over_floating_panel && pointer.is_some_and(|p| bar.hit_area.contains(p)));

        if pressed && self.pointer_on_scrollbar {
            if let Some(y) = pointer.map(|p| p.y) {
                // Sur la poignée : on garde le point de saisie. Ailleurs : la poignée vient sous le pointeur.
                self.scrollbar_grab = Some(if bar.contains_thumb_y(y) {
                    y - bar.thumb_top
                } else {
                    bar.thumb_height * 0.5
                });
            }
        }
        if !down {
            self.scrollbar_grab = None;
        }
        if let (Some(grab), Some(pointer)) = (self.scrollbar_grab, pointer) {
            tab.viewport.scroll_offset.y = bar.scroll_for_thumb_top(pointer.y - grab);
            tab.viewport.inertia_velocity = Vec2::ZERO;
            tab.viewport
                .clamp_scroll(canvas_rect.size(), &tab.page_dimensions);
            ctx.request_repaint();
        }
        if self.pointer_on_scrollbar {
            ctx.set_cursor_icon(egui::CursorIcon::Default);
        }
    }

    fn apply_tab_menu_action(&mut self, index: usize, action: TabMenuAction) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let full_path = std::path::absolute(&tab.path).unwrap_or_else(|_| tab.path.clone());
        match action {
            TabMenuAction::RevealInExplorer => {
                // `/select,` exige des guillemets que `Command::arg` échapperait.
                let _ = std::process::Command::new("explorer")
                    .raw_arg(format!("/select,\"{}\"", full_path.display()))
                    .spawn();
            }
            TabMenuAction::OpenWithDefaultApp => {
                let _ = std::process::Command::new("explorer").arg(&full_path).spawn();
            }
            TabMenuAction::CloseThis => self.close_tab(index),
            TabMenuAction::CloseOthers => {
                for other in (0..self.tabs.len()).rev().filter(|i| *i != index) {
                    self.close_tab(other);
                }
            }
            TabMenuAction::CloseBelow => {
                for below in (index + 1..self.tabs.len()).rev() {
                    self.close_tab(below);
                }
            }
        }
    }

    /// Champ de saisie posé exactement sur le bloc FreeText en cours d'édition.
    fn render_freetext_editor(&mut self, ctx: &Context, visible_pages: &[(usize, Rect)]) {
        let Some(editing_id) = self.editing_freetext_id else {
            return;
        };
        // Tant que l'éditeur est ouvert, chaque lettre lui appartient, quel que soit l'état du focus.
        self.text_field_focused_now = true;
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let Some(annot) = tab.annotations.items.iter_mut().find(|a| a.id == editing_id) else {
            self.editing_freetext_id = None;
            return;
        };
        let page_on_screen = visible_pages
            .iter()
            .find(|(p, _)| *p == annot.page_index)
            .map(|(_, r)| *r);
        let Some(page_rect) = page_on_screen else {
            return;
        };
        let page_dim = tab.page_dimensions[annot.page_index];
        let screen_rect = annot.rect.to_screen_rect(page_rect, page_dim);
        let scale_y = page_rect.height() / page_dim.height_pt.max(1.0);
        let scale_x = page_rect.width() / page_dim.width_pt.max(1.0);
        let AnnotationKind::FreeText {
            text,
            font_size,
            text_rgb,
            ..
        } = &mut annot.kind
        else {
            self.editing_freetext_id = None;
            return;
        };
        let font = FontId::proportional(*font_size * scale_y);
        let text_color = Color32::from_rgb(text_rgb.0, text_rgb.1, text_rgb.2);
        // Un clic dans la barre latérale (couleurs, taille...) ne doit pas fermer le bloc en cours d'écriture.
        let pointer_in_sidebar = sidebar_shown(&self.sidebar, self.top_layout)
            && ctx
                .input(|i| i.pointer.hover_pos())
                .is_some_and(|p| p.x <= self.sidebar.width);

        let mut changed = false;
        let mut finished = ctx.input(|i| i.key_pressed(Key::Escape));
        let focus_now = std::mem::take(&mut self.freetext_focus_pending);
        let ignore_outside_click = self.freetext_awaiting_opening_release;
        if ctx.input(|i| i.pointer.any_released()) {
            self.freetext_awaiting_opening_release = false;
        }
        egui::Area::new(egui::Id::new("kite_freetext_editor").with(editing_id))
            .fixed_pos(screen_rect.min)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let response = ui.add_sized(
                    screen_rect.size(),
                    egui::TextEdit::multiline(text)
                        .frame(false)
                        .font(font)
                        .text_color(text_color)
                        .hint_text("Saisissez votre texte...")
                        .margin(egui::vec2(6.0 * scale_x, 4.0 * scale_y)),
                );
                if focus_now {
                    response.request_focus();
                }
                changed = response.changed();
                let outside_click = response.clicked_elsewhere();
                finished |= !ignore_outside_click && outside_click && !pointer_in_sidebar;
                if outside_click && pointer_in_sidebar {
                    self.freetext_focus_pending = true;
                }
            });

        if changed {
            tab.is_modified = true;
        }
        if finished {
            self.editing_freetext_id = None;
            let is_blank = tab
                .annotations
                .items
                .iter()
                .find(|a| a.id == editing_id)
                .is_some_and(|a| matches!(&a.kind, AnnotationKind::FreeText { text, .. } if text.trim().is_empty()));
            if is_blank {
                tab.annotations.selected_id = Some(editing_id);
                tab.annotations.delete_selected();
            }
        }
    }

    /// Bulle de commentaire façon Acrobat : auteur, heure, texte, réponses et état de révision.
    fn render_comment_panel(
        &mut self,
        ctx: &Context,
        canvas_rect: Rect,
        visible_pages: &[(usize, Rect)],
    ) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let selected_note = tab
            .annotations
            .selected()
            .filter(|a| matches!(a.kind, AnnotationKind::StickyNote { .. }))
            .map(|a| a.id);

        if self.open_comment_id != selected_note {
            // Un commentaire refermé sans contenu ne laisse pas de trace.
            if let Some(previous) = self.open_comment_id {
                if let Some(index) = tab
                    .annotations
                    .items
                    .iter()
                    .position(|a| a.id == previous && a.is_empty_comment())
                {
                    if tab.annotations.items[index].meta.was_highlight {
                        tab.annotations.items[index].revert_empty_comment_to_highlight();
                    } else {
                        tab.annotations.items.remove(index);
                    }
                }
            }
            self.open_comment_id = selected_note;
            self.comment_reply_draft.clear();
        }
        if self.comment_edit.is_some_and(|(id, _)| Some(id) != selected_note) {
            self.comment_edit = None;
        }
        let Some(note_id) = selected_note else {
            return;
        };
        let Some(annot) = tab.annotations.items.iter().find(|a| a.id == note_id).cloned() else {
            return;
        };
        let Some(page_rect) = visible_pages
            .iter()
            .find(|(p, _)| *p == annot.page_index)
            .map(|(_, r)| *r)
        else {
            return;
        };
        let AnnotationKind::StickyNote { comment, .. } = &annot.kind else {
            return;
        };
        let page_dim = tab.page_dimensions[annot.page_index];
        let scale = page_rect.width() / page_dim.width_pt.max(1.0);
        let origin = comment_panel_origin(&annot, page_rect, page_dim);
        let panel_pos = Pos2::new(
            origin.x.clamp(canvas_rect.min.x + 8.0, (canvas_rect.max.x - COMMENT_PANEL_WIDTH - 28.0).max(canvas_rect.min.x + 8.0)),
            origin.y.clamp(canvas_rect.min.y + 8.0, (canvas_rect.max.y - 120.0).max(canvas_rect.min.y + 8.0)),
        );

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.comment_edit = None;
        }
        let editing = self.comment_edit.map(|(_, target)| target);
        let mut main_text = comment.clone();
        let mut edited_reply: Option<(usize, String)> = match editing {
            Some(CommentEditTarget::Reply(index)) => annot
                .meta
                .replies
                .get(index)
                .map(|reply| (index, reply.text.clone())),
            _ => None,
        };
        let focus_edit = std::mem::take(&mut self.comment_focus_pending);
        let focus_reply = std::mem::take(&mut self.comment_reply_focus_pending);
        let mut draft = std::mem::take(&mut self.comment_reply_draft);
        let mut close_panel = false;
        let mut send_reply = false;
        let mut delete_reply = None;
        let mut start_edit = None;
        let mut finish_edit = false;
        let mut header_drag = Vec2::ZERO;
        let mut reply_has_focus = false;
        let mut open_menu_at = None;
        let mut header_buttons_left = f32::INFINITY;

        let area = egui::Area::new(egui::Id::new("kite_comment_panel").with(note_id))
            .fixed_pos(panel_pos)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                ui.visuals_mut().override_text_color = Some(INLINE_TEXT_COLOR);
                ui.visuals_mut().extreme_bg_color = Color32::from_rgb(245, 246, 248);
                ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::from_rgb(236, 238, 242);
                ui.visuals_mut().widgets.hovered.weak_bg_fill = Color32::from_rgb(222, 226, 233);
                egui::Frame::NONE
                    .fill(COMMENT_PANEL_BACKGROUND)
                    .stroke(Stroke::new(1.5_f32, annot.egui_color(255)))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(10))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 3],
                        blur: 10,
                        spread: 0,
                        color: Color32::from_black_alpha(70),
                    })
                    .show(ui, |ui| {
                        ui.set_width(COMMENT_PANEL_WIDTH - 20.0);
                        let date = if annot.meta.modified.is_empty() {
                            &annot.meta.created
                        } else {
                            &annot.meta.modified
                        };
                        let header = ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&annot.meta.author).strong().color(INLINE_TEXT_COLOR));
                            ui.label(egui::RichText::new(format_pdf_date(date)).size(11.0).color(COMMENT_MUTED_TEXT));
                            if let Some(review) = annot.meta.review_state.as_ref().filter(|r| r.state != "None") {
                                ui.label(
                                    egui::RichText::new(review_state_label(&review.state))
                                        .size(10.5)
                                        .color(Color32::from_rgb(30, 110, 200)),
                                );
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("✖").on_hover_text("Fermer").clicked() {
                                    close_panel = true;
                                }
                                let more = ui.small_button("…").on_hover_text("Options");
                                if more.clicked() {
                                    open_menu_at = Some(more.rect.left_bottom());
                                }
                                header_buttons_left = more.rect.left();
                            });
                        });
                        if annot.is_fixed_to_text() {
                            // La zone de déplacement s'arrête avant les boutons, sinon elle avale leurs clics.
                            let mut drag_rect = header.response.rect;
                            drag_rect.max.x = drag_rect.max.x.min(header_buttons_left - 4.0);
                            let drag = ui.interact(
                                drag_rect,
                                ui.id().with("comment_header"),
                                Sense::drag(),
                            );
                            header_drag = drag.drag_delta();
                        }

                        if editing == Some(CommentEditTarget::Main) {
                            let field = ui.add(
                                egui::TextEdit::multiline(&mut main_text)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("Saisissez votre commentaire..."),
                            );
                            if focus_edit {
                                field.request_focus();
                            }
                            if ui.small_button("Terminer").clicked() {
                                finish_edit = true;
                            }
                        } else {
                            let label = ui.add(egui::Label::new(comment.as_str()).wrap().sense(Sense::click()));
                            if label.double_clicked() {
                                start_edit = Some(CommentEditTarget::Main);
                            }
                        }

                        for (index, reply) in annot.meta.replies.iter().enumerate() {
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&reply.author).strong().size(12.0).color(INLINE_TEXT_COLOR));
                                ui.label(egui::RichText::new(format_pdf_date(&reply.date)).size(10.5).color(COMMENT_MUTED_TEXT));
                            });
                            match edited_reply.as_mut().filter(|(i, _)| *i == index) {
                                Some((_, text)) => {
                                    let field = ui.add(
                                        egui::TextEdit::multiline(text)
                                            .desired_rows(1)
                                            .desired_width(f32::INFINITY),
                                    );
                                    if focus_edit {
                                        field.request_focus();
                                    }
                                    if ui.small_button("Terminer").clicked() {
                                        finish_edit = true;
                                    }
                                }
                                None => {
                                    let label = ui.add(egui::Label::new(reply.text.as_str()).wrap().sense(Sense::click()));
                                    if label.double_clicked() {
                                        start_edit = Some(CommentEditTarget::Reply(index));
                                    }
                                    label.context_menu(|ui| {
                                        if ui.button("Modifier").clicked() {
                                            start_edit = Some(CommentEditTarget::Reply(index));
                                            ui.close_menu();
                                        }
                                        if ui.button("Supprimer").clicked() {
                                            delete_reply = Some(index);
                                            ui.close_menu();
                                        }
                                    });
                                }
                            }
                        }

                        // On ne répond qu'à un commentaire déjà écrit.
                        let can_reply = editing != Some(CommentEditTarget::Main)
                            && (!comment.trim().is_empty() || !annot.meta.replies.is_empty());
                        if !can_reply {
                            return;
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let field = ui.add(
                                egui::TextEdit::singleline(&mut draft)
                                    .desired_width(COMMENT_PANEL_WIDTH - 96.0)
                                    .hint_text("Ajouter une réponse"),
                            );
                            if focus_reply {
                                field.request_focus();
                            }
                            reply_has_focus = field.has_focus();
                            let pressed_enter =
                                field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                            let can_send = !draft.trim().is_empty();
                            if ui.add_enabled(can_send, egui::Button::new("Répondre").small()).clicked()
                                || (pressed_enter && can_send)
                            {
                                send_reply = true;
                            }
                        });
                    });
            });
        self.comment_reply_draft = draft;
        if editing.is_some() || reply_has_focus {
            self.text_field_focused_now = true;
        }

        let panel_rect = area.response.rect;
        let right_clicked_inside = ctx.input(|i| {
            i.pointer.secondary_released()
                && i.pointer.hover_pos().is_some_and(|p| panel_rect.contains(p))
        });
        if right_clicked_inside && self.comment_edit.is_none() && !ctx.is_context_menu_open() {
            open_menu_at = ctx.input(|i| i.pointer.hover_pos());
        }

        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if header_drag != Vec2::ZERO {
            if let Some(target) = tab.annotations.items.iter_mut().find(|a| a.id == note_id) {
                target.translate(header_drag.x / scale, header_drag.y / scale);
                tab.is_modified = true;
            }
        }
        if main_text != *comment {
            if let Some(PdfAnnotation {
                kind: AnnotationKind::StickyNote { comment, .. },
                ..
            }) = tab.annotations.items.iter_mut().find(|a| a.id == note_id)
            {
                *comment = main_text;
                tab.is_modified = true;
            }
        }
        if let Some((index, text)) = edited_reply {
            if let Some(reply) = tab
                .annotations
                .items
                .iter_mut()
                .find(|a| a.id == note_id)
                .and_then(|a| a.meta.replies.get_mut(index))
            {
                if reply.text != text {
                    reply.text = text;
                    tab.is_modified = true;
                }
            }
        }
        if let Some(index) = delete_reply {
            tab.annotations.push_undo_snapshot();
            if let Some(target) = tab.annotations.items.iter_mut().find(|a| a.id == note_id) {
                target.meta.replies.remove(index);
                tab.is_modified = true;
            }
        }
        if send_reply {
            tab.annotations.push_undo_snapshot();
            if let Some(target) = tab.annotations.items.iter_mut().find(|a| a.id == note_id) {
                target.meta.replies.push(CommentReply {
                    author: default_author(),
                    date: pdf_date_now(),
                    text: self.comment_reply_draft.trim().to_string(),
                    ..Default::default()
                });
                tab.is_modified = true;
            }
            self.comment_reply_draft.clear();
            self.comment_reply_focus_pending = true;
        }
        if let Some(target) = start_edit {
            tab.annotations.push_undo_snapshot();
            self.comment_edit = Some((note_id, target));
            self.comment_focus_pending = true;
        }
        if finish_edit {
            self.comment_edit = None;
        }
        if close_panel {
            tab.annotations.selected_id = None;
        }
        if let Some(position) = open_menu_at {
            self.line_context_menu = Some(LineContextMenu {
                annotation_id: note_id,
                position,
                opened_on_frame: self.frame_count,
            });
        }
    }

    /// Signale un enregistrement raté sans toucher à l'affichage du document.
    fn render_save_error_popup(&mut self, ctx: &Context) {
        let Some(reason) = &self.save_error else {
            return;
        };
        let mut dismissed = false;
        egui::Window::new("Enregistrement impossible")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label("Le fichier n'a pas été enregistré (Ctrl+S). Vos annotations sont toujours là.");
                ui.label("Il est probablement ouvert dans une autre application, comme Acrobat. Fermez-le là-bas puis réessayez.");
                ui.add_space(4.0);
                ui.label(egui::RichText::new(reason).size(11.0).weak());
                ui.add_space(6.0);
                if ui.button("OK").clicked() {
                    dismissed = true;
                }
            });
        if dismissed {
            self.save_error = None;
        }
    }

    /// Menu du clic droit : attributs d'une flèche ou d'un bloc, actions d'un commentaire.
    fn render_line_context_menu(&mut self, ctx: &Context) {
        let Some(menu) = &self.line_context_menu else {
            return;
        };
        let (annotation_id, position, opened_on_frame) =
            (menu.annotation_id, menu.position, menu.opened_on_frame);
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let Some(annot) = tab
            .annotations
            .items
            .iter()
            .find(|a| a.id == annotation_id)
            .cloned()
        else {
            self.line_context_menu = None;
            return;
        };
        let arrow_heads = match annot.kind {
            AnnotationKind::Shape {
                shape_kind:
                    ShapeKind::ArrowLine {
                        start_head,
                        end_head,
                    },
                ..
            } => Some((start_head, end_head)),
            _ => None,
        };
        let block_style = match annot.kind {
            AnnotationKind::FreeText {
                font_size,
                fill_rgb,
                border_rgb,
                ..
            } => Some((font_size, fill_rgb, border_rgb)),
            _ => None,
        };
        let is_note = matches!(annot.kind, AnnotationKind::StickyNote { .. });
        let is_highlight = matches!(annot.kind, AnnotationKind::Highlight { .. });
        if arrow_heads.is_none() && block_style.is_none() && !is_note && !is_highlight {
            self.line_context_menu = None;
            return;
        }

        let mut new_fill = None;
        let mut new_border = None;
        let mut new_size = None;
        let mut new_color = None;
        let mut new_start = None;
        let mut new_end = None;
        let mut new_state: Option<&'static str> = None;
        let mut reply_requested = false;
        let mut edit_requested = false;
        let mut comment_requested = false;
        let mut delete_requested = false;
        let ending_row = |ui: &mut egui::Ui, title: &str, current: LineEnding| {
            let mut picked = None;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).size(11.0));
                for ending in LineEnding::ALL {
                    if ui.selectable_label(current == ending, ending.label()).clicked() {
                        picked = Some(ending);
                    }
                }
            });
            picked
        };
        let current_state = annot
            .meta
            .review_state
            .as_ref()
            .map_or("None", |r| r.state.as_str());

        let area = egui::Area::new(egui::Id::new("kite_line_context_menu"))
            .fixed_pos(position)
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if is_note {
                        if ui.add_enabled(!annot.is_empty_comment(), egui::Button::new("Répondre")).clicked() {
                            reply_requested = true;
                        }
                        if ui.button("Modifier le commentaire").clicked() {
                            edit_requested = true;
                        }
                        if ui.button("Supprimer").clicked() {
                            delete_requested = true;
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("État").size(11.0));
                            for (value, label) in REVIEW_STATES {
                                if ui.selectable_label(current_state == value, label).clicked() {
                                    new_state = Some(value);
                                }
                            }
                        });
                    }
                    if is_highlight {
                        if ui.button("Ajouter un commentaire").clicked() {
                            comment_requested = true;
                        }
                        if ui.button("Supprimer").clicked() {
                            delete_requested = true;
                        }
                        ui.separator();
                    }
                    if let Some((font_size, fill, border)) = block_style {
                        new_fill = color_choice_row(ui, "Fond", fill, &FREETEXT_FILL_CHOICES);
                        new_border = color_choice_row(ui, "Contour", border, &FREETEXT_BORDER_CHOICES);
                        new_size = font_size_row(ui, font_size);
                    }
                    if arrow_heads.is_some() || is_note || is_highlight {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Couleur").size(11.0));
                            for (name, rgb) in PALETTE_COLORS {
                                let swatch =
                                    egui::Button::new(if annot.color_rgb == rgb { "✔" } else { " " })
                                        .fill(Color32::from_rgb(rgb.0, rgb.1, rgb.2))
                                        .min_size(Vec2::new(22.0, 16.0));
                                if ui.add(swatch).on_hover_text(name).clicked() {
                                    new_color = Some(rgb);
                                }
                            }
                        });
                    }
                    if let Some((start_head, end_head)) = arrow_heads {
                        new_start = ending_row(ui, "Début", start_head);
                        new_end = ending_row(ui, "Fin    ", end_head);
                    }
                });
            });

        let page_dim = tab.page_dimensions[annot.page_index];
        let something_changed = new_fill.is_some()
            || new_border.is_some()
            || new_size.is_some()
            || new_color.is_some()
            || new_start.is_some()
            || new_end.is_some()
            || new_state.is_some();
        if something_changed {
            tab.annotations.push_undo_snapshot();
            if let Some(target) = tab.annotations.items.iter_mut().find(|a| a.id == annotation_id) {
                if let Some(color) = new_color {
                    target.color_rgb = color;
                }
                if let Some(state) = new_state {
                    target.meta.review_state = Some(ReviewState {
                        state: state.to_string(),
                        author: default_author(),
                        date: pdf_date_now(),
                    });
                }
                match &mut target.kind {
                    AnnotationKind::Shape {
                        shape_kind:
                            ShapeKind::ArrowLine {
                                start_head,
                                end_head,
                            },
                        ..
                    } => {
                        *start_head = new_start.unwrap_or(*start_head);
                        *end_head = new_end.unwrap_or(*end_head);
                    }
                    AnnotationKind::FreeText {
                        font_size,
                        fill_rgb,
                        border_rgb,
                        ..
                    } => {
                        *fill_rgb = new_fill.unwrap_or(*fill_rgb);
                        *border_rgb = new_border.unwrap_or(*border_rgb);
                        *font_size = new_size.unwrap_or(*font_size);
                        self.freetext_style = Some((*fill_rgb, *border_rgb));
                        self.freetext_font_size = *font_size;
                    }
                    _ => {}
                }
            }
            tab.is_modified = true;
        }

        let mut close_menu = false;
        if reply_requested {
            self.comment_edit = None;
            self.comment_reply_focus_pending = true;
            close_menu = true;
        }
        if edit_requested {
            tab.annotations.push_undo_snapshot();
            self.comment_edit = Some((annotation_id, CommentEditTarget::Main));
            self.comment_focus_pending = true;
            close_menu = true;
        }
        if comment_requested {
            tab.annotations.push_undo_snapshot();
            if let Some(target) = tab.annotations.items.iter_mut().find(|a| a.id == annotation_id) {
                target.convert_highlight_to_note(page_dim);
            }
            tab.annotations.selected_id = Some(annotation_id);
            self.comment_edit = Some((annotation_id, CommentEditTarget::Main));
            self.comment_focus_pending = true;
            close_menu = true;
        }
        if delete_requested {
            tab.annotations.selected_id = Some(annotation_id);
            tab.annotations.delete_selected();
            tab.is_modified = true;
            close_menu = true;
        }

        let dismissed = ctx.input(|i| i.key_pressed(Key::Escape))
            || (self.frame_count != opened_on_frame && area.response.clicked_elsewhere());
        if dismissed || close_menu {
            self.line_context_menu = None;
        }
    }

    /// Dessine la barre flottante de copie (`Ctrl+C`) et de surlignage direct (`✒ Surligner H`)
    fn render_floating_search_bar(&mut self, ctx: &Context, canvas_rect: Rect) {
        let bar_w = 430.0;
        let bar_pos = Pos2::new(
            (canvas_rect.max.x - bar_w - 20.0).max(canvas_rect.min.x + 12.0),
            canvas_rect.min.y + 14.0,
        );

        let mut run_search = false;
        let mut step_dir: i32 = 0;
        let mut close_search = false;
        let mut open_ocr_dialog = false;
        // Document dont au moins une page sur cinq n'a pas de texte : proposer l'OCR approfondi.
        let ocr_hint = self.tabs.get(self.active_tab).and_then(|tab| {
            if self.deep_ocr_running_for(tab.id) {
                let p = self.deep_ocr.as_ref()?.progress.lock().ok()?.clone();
                return Some(SearchOcrHint::Running { done: p.done, total: p.total });
            }
            let (analysed, lacking) = tab.text_coverage();
            (lacking > 0 && lacking * 5 >= analysed.max(1)).then_some(SearchOcrHint::PagesWithoutText(lacking))
        });

        egui::Area::new(egui::Id::new("kite_floating_ctrl_f"))
            .fixed_pos(bar_pos)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(24, 28, 40, 245))
                    .stroke(Stroke::new(1.5_f32, Color32::from_rgb(90, 145, 245)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("🔎")
                                    .size(13.0)
                                    .color(Color32::from_rgb(130, 185, 255)),
                            );

                            let edit = egui::TextEdit::singleline(&mut self.search_bar.query)
                                .hint_text("Rechercher dans le PDF (Ctrl+F)...")
                                .desired_width(185.0);
                            let resp = ui.add(edit);
                            self.text_field_focused_now |= resp.has_focus();
                            if self.search_bar.focus_requested {
                                resp.request_focus();
                                self.search_bar.focus_requested = false;
                            }
                            if resp.changed()
                                || self.search_bar.query != self.search_bar.last_executed_query
                            {
                                run_search = true;
                            }

                            let total = self.search_bar.matches.len();
                            let current = if total > 0 {
                                self.search_bar.current_match_index + 1
                            } else {
                                0
                            };

                            ui.label(
                                egui::RichText::new(format!("{}/{}", current, total))
                                    .monospace()
                                    .size(11.5)
                                    .color(if total > 0 {
                                        Color32::from_rgb(130, 225, 170)
                                    } else {
                                        Color32::from_rgb(235, 120, 120)
                                    }),
                            );

                            if ui
                                .add_enabled(total > 0, egui::Button::new("◀").small())
                                .on_hover_text("Résultat précédent (Maj+Entrée)")
                                .clicked()
                            {
                                step_dir = -1;
                            }
                            if ui
                                .add_enabled(total > 0, egui::Button::new("▶").small())
                                .on_hover_text("Résultat suivant (Entrée)")
                                .clicked()
                            {
                                step_dir = 1;
                            }
                            if ui
                                .small_button("✖")
                                .on_hover_text("Fermer la recherche (Échap)")
                                .clicked()
                            {
                                close_search = true;
                            }
                        });
                        match &ocr_hint {
                            Some(SearchOcrHint::Running { done, total }) => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "OCR approfondi en cours : {done} / {total} pages. Les résultats s'ajoutent au fur et à mesure."
                                    ))
                                    .size(11.0)
                                    .color(Color32::from_rgb(150, 160, 182)),
                                );
                            }
                            Some(SearchOcrHint::PagesWithoutText(count)) => {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{count} page{} sans texte lisible.",
                                            if *count > 1 { "s" } else { "" }
                                        ))
                                        .size(11.0)
                                        .color(Color32::from_rgb(150, 160, 182)),
                                    );
                                    if ui
                                        .link(egui::RichText::new("Lancer l'OCR approfondi…").size(11.0))
                                        .clicked()
                                    {
                                        open_ocr_dialog = true;
                                    }
                                });
                            }
                            None => {}
                        }
                    });
            });
        if open_ocr_dialog {
            self.open_deep_ocr_dialog(canvas_rect.size());
        }

        ctx.input(|input| {
            if input.key_pressed(Key::Enter) && !self.command_palette.is_open {
                step_dir = if input.modifiers.shift { -1 } else { 1 };
            }
        });

        if run_search {
            let read_images = self.prefs.image_ocr != crate::settings::ImageOcr::Never;
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                let vis_p = tab
                    .viewport
                    .current_page_index(canvas_rect.size(), &tab.page_dimensions);
                // Phase 3 : les résultats s'ajoutent au fil de l'indexation, sans bloquer la saisie.
                tab.request_all_text(&self.workers);
                if read_images {
                    tab.start_turbo_ocr();
                    tab.pump_ocr(vis_p, &self.workers);
                }
                self.search_bar.execute_search(&tab.text_layers);
            }
            self.scroll_to_current_search_match(canvas_rect.size());
        } else if step_dir != 0 {
            if step_dir > 0 {
                self.search_bar.next_match();
            } else {
                self.search_bar.prev_match();
            }
            self.scroll_to_current_search_match(canvas_rect.size());
        }

        if close_search {
            self.search_bar.close();
        }
    }

    fn render_sidebar_contents(&mut self, ui: &mut egui::Ui) {
        let mut pending_cmd: Option<PaletteCommand> = None;
        let mut tab_to_close: Option<usize> = None;
        let mut tab_to_select: Option<usize> = None;
        let mut match_to_select: Option<usize> = None;
        let mut tab_menu_action: Option<(usize, TabMenuAction)> = None;

        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);

        // 1. En-tête de la barre latérale : logo et nom
        let logo = self.logo.clone();
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(logo) = &logo {
                        ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(22.0)));
                    }
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(egui::RichText::new("Kite").strong().size(15.0).color(Color32::from_rgb(245, 247, 250)));
                    ui.label(egui::RichText::new("PDF").size(15.0).color(Color32::from_rgb(140, 150, 170)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pin_label = if self.sidebar.pinned {
                            "📌 Épinglée"
                        } else {
                            "👁 Survol"
                        };
                        if ui
                            .small_button(pin_label)
                            .on_hover_text("Garder la barre ouverte, ou la masquer et la faire apparaître au bord gauche (Ctrl+B)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::ToggleSidebarPin);
                        }

                        if ui.small_button("⚙").on_hover_text("Réglages (Ctrl+,)").clicked() {
                            self.settings_open = true;
                        }

                        if ui
                            .small_button("🔎")
                            .on_hover_text("Chercher un mot dans le PDF (Ctrl+F)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::OpenSearch);
                        }

                    });
                });

                ui.add_space(3.0);

                // Ouvre la barre centrale (Ctrl+T)
                let search_btn = egui::Button::new(
                    egui::RichText::new("🔍 PDF récent, outil, page…   Ctrl+T")
                        .size(11.5)
                        .color(Color32::from_rgb(175, 185, 208)),
                )
                .fill(Color32::from_rgb(33, 38, 50))
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(self.sidebar.width - 26.0, 25.0));

                if ui.add(search_btn).clicked() {
                    self.command_palette.open();
                }

                ui.add_space(5.0);

                // 2. Grille d'outils compacts (2 lignes x 4 colonnes) + Actions Annotations ISO 32000
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("OUTILS")
                            .size(10.0)
                            .strong()
                            .color(Color32::from_rgb(120, 132, 158)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("💾 Ctrl+S")
                            .on_hover_text(
                                "Enregistrer les annotations au format standard ISO 32000 dans le PDF",
                            )
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::SaveAnnotatedPdf);
                        }
                        if ui
                            .small_button("↪")
                            .on_hover_text("Rétablir (Ctrl+Y)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::RedoAnnotation);
                        }
                        if ui
                            .small_button("↩")
                            .on_hover_text("Annuler (Ctrl+Z)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::UndoAnnotation);
                        }
                    });
                });

                // Une rangée de 8 si la barre est large, sinon deux rangées de 4 ; les boutons se
                // partagent toute la largeur.
                let columns = if self.sidebar.width >= SIDEBAR_ONE_ROW_TOOLS_WIDTH { 8 } else { 4 };
                let spacing = 5.0;
                let tool_width = ((ui.available_width() - spacing * (columns - 1) as f32) / columns as f32).max(40.0);
                egui::Grid::new("kite_tools_grid")
                    .num_columns(columns)
                    .spacing([spacing, 4.0])
                    .show(ui, |ui| {
                        for (i, tool) in ActiveTool::all().iter().enumerate() {
                            let (icon, label, shortcut) = tool.icon_and_label();
                            let is_active = self.active_tool == *tool;
                            let fill = if is_active {
                                Color32::from_rgb(52, 105, 210)
                            } else {
                                Color32::from_rgb(32, 37, 48)
                            };
                            let text_col = if is_active {
                                Color32::WHITE
                            } else {
                                Color32::from_rgb(190, 200, 220)
                            };

                            let btn = egui::Button::new(
                                egui::RichText::new(format!("{}\n{}", icon, shortcut))
                                    .size(11.0)
                                    .color(text_col),
                            )
                            .fill(fill)
                            .corner_radius(CornerRadius::same(7))
                            .min_size(Vec2::new(tool_width, 33.0));

                            if ui
                                .add(btn)
                                .on_hover_text(format!("{label} (touche {shortcut})"))
                                .clicked()
                            {
                                self.active_tool = *tool;
                            }
                            if (i + 1) % columns == 0 {
                                ui.end_row();
                            }
                        }
                    });

                ui.add_space(4.0);

                // 3. Boutons d'ouverture, au-dessus de la liste des onglets
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("➕")
                            .on_hover_text("Rouvrir un PDF de l'historique (Ctrl+T)")
                            .clicked()
                        {
                            self.command_palette.open();
                        }
                        if ui
                            .small_button("📂")
                            .on_hover_text("Ouvrir un fichier PDF (Ctrl+O)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::OpenFileDialog);
                        }
                        let outline_button = egui::Button::new(egui::RichText::new("☰ Sommaire").size(11.0))
                            .small()
                            .selected(self.outline_open);
                        if ui
                            .add_enabled(!self.tabs.is_empty(), outline_button)
                            .on_hover_text("Sommaire du document (signets)")
                            .clicked()
                        {
                            self.outline_open = !self.outline_open;
                        }
                    });
                });
            });

        ui.separator();

        let (has_annot_inspector, has_search_drawer, has_outline_drawer) = self.drawer_kinds();
        let has_drawer = has_annot_inspector || has_search_drawer || has_outline_drawer;
        // Bas de la barre (navigation) toujours réservé ; le tiroir prend la hauteur choisie, sans
        // jamais écraser la liste des onglets.
        let navigation_h = 100.0;
        let max_drawer_h = (ui.available_height() - navigation_h - 90.0).max(80.0);
        self.sidebar.drawer_height = self.sidebar.drawer_height.clamp(80.0, max_drawer_h);
        let tabs_reserved_bottom = navigation_h
            + if has_drawer { self.sidebar.drawer_height + DRAWER_SPLITTER_PX } else { 0.0 };
        let available_h = (ui.available_height() - tabs_reserved_bottom).max(85.0);

        // 4. Liste déroulante verticale des onglets
        egui::ScrollArea::vertical()
            .id_salt("tabs_scroll_area")
            .max_height(available_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                for (idx, tab) in self.tabs.iter().enumerate() {
                    let is_active = idx == self.active_tab;
                    let bg_fill = if is_active {
                        Color32::from_rgb(40, 48, 68)
                    } else {
                        Color32::from_rgb(27, 31, 41)
                    };
                    let stroke = if is_active {
                        Stroke::new(1.2_f32, tab.badge_color)
                    } else {
                        Stroke::new(1.0_f32, Color32::from_rgb(36, 41, 54))
                    };

                    egui::Frame::NONE
                        .inner_margin(Margin::symmetric(8, 5))
                        .outer_margin(Margin::symmetric(8, 0))
                        .fill(bg_fill)
                        .stroke(stroke)
                        .corner_radius(CornerRadius::same(7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("✖")
                                            .on_hover_text("Fermer cet onglet (Ctrl+W)")
                                            .clicked()
                                        {
                                            tab_to_close = Some(idx);
                                        }

                                        let name_area = ui.available_rect_before_wrap();
                                        let row = ui
                                            .interact(
                                                name_area,
                                                ui.id().with(("tab_row", tab.id)),
                                                Sense::click(),
                                            )
                                            .on_hover_text(tab.path.display().to_string());
                                        if row.clicked() {
                                            tab_to_select = Some(idx);
                                        }
                                        row.context_menu(|ui| tab_context_menu(ui, idx, tab, &mut tab_menu_action));

                                        let mut name = egui::text::LayoutJob::single_section(
                                            format!(
                                                "{}{}",
                                                tab.title,
                                                if tab.is_modified { " *" } else { "" }
                                            ),
                                            egui::TextFormat {
                                                font_id: FontId::proportional(12.0),
                                                color: if is_active {
                                                    Color32::WHITE
                                                } else {
                                                    Color32::from_rgb(185, 194, 212)
                                                },
                                                ..Default::default()
                                            },
                                        );
                                        name.wrap.max_rows = 2;
                                        name.wrap.break_anywhere = true;
                                        name.wrap.overflow_character = Some('…');
                                        ui.with_layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(name)
                                                        .wrap()
                                                        .selectable(false)
                                                        .sense(Sense::hover()),
                                                );
                                            },
                                        );
                                    },
                                );
                            });
                        });
                }
            });

        // 5. Tiroir du bas : inspecteur d'annotation, résultats de recherche ou sommaire, avec un
        // séparateur à glisser pour régler sa hauteur.
        if has_drawer {
            let (splitter, response) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), DRAWER_SPLITTER_PX),
                Sense::drag(),
            );
            let active = response.hovered() || response.dragged();
            ui.painter().line_segment(
                [Pos2::new(splitter.left() + 8.0, splitter.center().y), Pos2::new(splitter.right() - 8.0, splitter.center().y)],
                Stroke::new(if active { 2.0_f32 } else { 1.0_f32 }, Color32::from_rgb(if active { 110 } else { 50 }, if active { 150 } else { 56 }, if active { 230 } else { 72 })),
            );
            if active {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
            if response.dragged() {
                self.sidebar.drawer_height = (self.sidebar.drawer_height - response.drag_delta().y).clamp(80.0, max_drawer_h);
            }
            let drawer_height = self.sidebar.drawer_height;
            let width = ui.available_width();
            let mut drawer_outcome = (None, None);
            ui.allocate_ui_with_layout(Vec2::new(width, drawer_height), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_height(drawer_height);
            ui.set_clip_rect(ui.max_rect());

            drawer_outcome = self.render_drawer_contents(ui, drawer_height);
            });
            pending_cmd = drawer_outcome.0.or(pending_cmd);
            match_to_select = drawer_outcome.1.or(match_to_select);
        }
        ui.separator();

        // 5. Navigation : page, zoom et ajustement. L'état de l'OCR n'apparaît que pendant son travail.
        let ocr_status = self.image_ocr_status();
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                let muted = Color32::from_rgb(140, 150, 172);
                let viewport_size = self.canvas_size;
                self.render_deep_ocr_progress(ui);
                let Some(tab) = self.tabs.get_mut(self.active_tab) else {
                    return;
                };

                if let Some((label, detail, working)) = ocr_status {
                    ui.horizontal(|ui| {
                        if working {
                            ui.add(egui::Spinner::new().size(11.0).color(muted));
                        }
                        let response = ui.label(egui::RichText::new(label).size(10.5).color(muted));
                        if let Some(detail) = detail {
                            response.on_hover_text(detail);
                        }
                    });
                    ui.add_space(2.0);
                }

                let pages = tab.page_dimensions.clone();
                let page_count = pages.len();
                let current_page = tab.viewport.current_page_index(viewport_size, &pages) + 1;

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(current_page > 1, egui::Button::new("◀").small())
                        .on_hover_text("Page précédente")
                        .clicked()
                    {
                        tab.viewport.jump_to_page(current_page - 2, viewport_size, &pages);
                    }
                    ui.label(
                        egui::RichText::new(format!("{current_page} / {page_count}"))
                            .size(12.0)
                            .color(Color32::WHITE),
                    );
                    if ui
                        .add_enabled(current_page < page_count, egui::Button::new("▶").small())
                        .on_hover_text("Page suivante")
                        .clicked()
                    {
                        tab.viewport.jump_to_page(current_page, viewport_size, &pages);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("+").on_hover_text("Zoom avant").clicked() {
                            tab.viewport.apply_zoom_around_anchor(
                                1.2,
                                viewport_size * 0.5,
                                viewport_size,
                                &pages,
                            );
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(format!("{:.0} %", tab.viewport.zoom * 100.0))
                                        .size(11.5),
                                )
                                .frame(false),
                            )
                            .on_hover_text("Revenir à 100 %")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::ZoomReset100);
                        }
                        if ui.small_button("−").on_hover_text("Zoom arrière").clicked() {
                            tab.viewport.apply_zoom_around_anchor(
                                1.0 / 1.2,
                                viewport_size * 0.5,
                                viewport_size,
                                &pages,
                            );
                        }
                    });
                });

                ui.horizontal(|ui| {
                    let half = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                    if ui
                        .add_sized([half, 20.0], egui::Button::new("↔ Largeur"))
                        .on_hover_text("Ajuster à la largeur (Ctrl+9)")
                        .clicked()
                    {
                        pending_cmd = Some(PaletteCommand::FitToWidth);
                    }
                    if ui
                        .add_sized([half, 20.0], egui::Button::new("↕ Page entière"))
                        .on_hover_text("Afficher la page entière (Ctrl+0)")
                        .clicked()
                    {
                        pending_cmd = Some(PaletteCommand::FitToPage);
                    }
                });
            });

        if let Some((idx, action)) = tab_menu_action {
            self.apply_tab_menu_action(idx, action);
        }
        if let Some(idx) = tab_to_select {
            self.switch_to_tab(idx);
        }
        if let Some(idx) = tab_to_close {
            self.close_tab(idx);
        }
        if let Some(m_idx) = match_to_select {
            self.search_bar.current_match_index = m_idx;
            self.scroll_to_current_search_match(self.canvas_size);
        }
        if let Some(cmd) = pending_cmd {
            self.execute_palette_command(cmd, self.canvas_size);
        }
    }

    /// Ce que le tiroir doit montrer : (inspecteur d'annotation, résultats de recherche, sommaire).
    fn drawer_kinds(&self) -> (bool, bool, bool) {
        let annotation = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.annotations.selected())
            .is_some();
        let search = self.search_bar.is_open && !self.search_bar.matches.is_empty();
        let outline = self.outline_open && !self.tabs.is_empty();
        (annotation, search, outline)
    }

    /// Contenu du tiroir (bas de la barre latérale, ou volet du bas en portrait) : inspecteur de
    /// l'annotation choisie, résultats de `Ctrl+F` ou sommaire. Renvoie la commande et le résultat choisis.
    fn render_drawer_contents(&mut self, ui: &mut egui::Ui, drawer_height: f32) -> (Option<PaletteCommand>, Option<usize>) {
        let mut pending_cmd: Option<PaletteCommand> = None;
        let mut match_to_select: Option<usize> = None;
        let (has_annot_inspector, has_search_drawer, has_outline_drawer) = self.drawer_kinds();
            if has_annot_inspector {
                ui.separator();
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let mut delete_clicked = false;
                    let mut picked_fill = None;
                    let mut picked_border = None;
                    let mut picked_size = None;
                    egui::Frame::NONE
                        .inner_margin(Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            if let Some(annot) = tab.annotations.selected_mut() {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!("✏ {}", annot.type_badge()))
                                            .size(10.5)
                                            .strong()
                                            .color(Color32::from_rgb(130, 200, 255)),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("🗑 Suppr")
                                                .on_hover_text("Supprimer l'annotation (Touche Suppr)")
                                                .clicked()
                                            {
                                                delete_clicked = true;
                                            }
                                        },
                                    );
                                });

                                if let AnnotationKind::FreeText {
                                    font_size,
                                    fill_rgb,
                                    border_rgb,
                                    ..
                                } = &annot.kind
                                {
                                    picked_fill = color_choice_row(ui, "Fond", *fill_rgb, &FREETEXT_FILL_CHOICES);
                                    picked_border =
                                        color_choice_row(ui, "Contour", *border_rgb, &FREETEXT_BORDER_CHOICES);
                                    picked_size = font_size_row(ui, *font_size);
                                } else {
                                    // Sélecteur de couleur rapide
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Couleur :").size(10.5));
                                        for (cname, crgb) in PALETTE_COLORS {
                                            let c32 = Color32::from_rgb(crgb.0, crgb.1, crgb.2);
                                            let is_curr = annot.color_rgb == crgb;
                                            let btn = egui::Button::new(if is_curr { "✔" } else { " " })
                                                .fill(c32)
                                                .min_size(Vec2::new(22.0, 16.0));
                                            if ui.add(btn).on_hover_text(cname).clicked() {
                                                annot.color_rgb = crgb;
                                            }
                                        }
                                    });
                                }

                                // Édition directe du contenu texte pour FreeText et StickyNote
                                match &mut annot.kind {
                                    AnnotationKind::FreeText { text, .. } => {
                                        let field = ui.add(
                                            egui::TextEdit::multiline(text)
                                                .desired_rows(2)
                                                .desired_width(f32::INFINITY),
                                        );
                                        self.text_field_focused_now |= field.has_focus();
                                    }
                                    AnnotationKind::StickyNote { comment, .. } => {
                                        let field = ui.add(
                                            egui::TextEdit::multiline(comment)
                                                .desired_rows(2)
                                                .desired_width(f32::INFINITY),
                                        );
                                        self.text_field_focused_now |= field.has_focus();
                                    }
                                    _ => {}
                                }
                            }
                        });
                    if picked_fill.is_some() || picked_border.is_some() || picked_size.is_some() {
                        tab.annotations.push_undo_snapshot();
                        if let Some(PdfAnnotation {
                            kind:
                                AnnotationKind::FreeText {
                                    font_size,
                                    fill_rgb,
                                    border_rgb,
                                    ..
                                },
                            ..
                        }) = tab.annotations.selected_mut()
                        {
                            *fill_rgb = picked_fill.unwrap_or(*fill_rgb);
                            *border_rgb = picked_border.unwrap_or(*border_rgb);
                            *font_size = picked_size.unwrap_or(*font_size);
                            self.freetext_style = Some((*fill_rgb, *border_rgb));
                            self.freetext_font_size = *font_size;
                        }
                        tab.is_modified = true;
                    }
                    if delete_clicked {
                        tab.annotations.delete_selected();
                        tab.is_modified = true;
                    }
                }
            } else if has_search_drawer {
                ui.separator();
                egui::Frame::NONE
                    .inner_margin(Margin::symmetric(10, 4))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "🔎 RÉSULTATS CTRL+F ({})",
                                self.search_bar.matches.len()
                            ))
                            .size(10.5)
                            .strong()
                            .color(Color32::from_rgb(255, 185, 70)),
                        );
                        egui::ScrollArea::vertical()
                            .id_salt("search_results_drawer_scroll")
                            .max_height(drawer_height - 28.0)
                            .show(ui, |ui| {
                                for (m_idx, m) in self.search_bar.matches.iter().enumerate() {
                                    let is_curr = m_idx == self.search_bar.current_match_index;
                                    let fill = if is_curr {
                                        Color32::from_rgb(75, 52, 22)
                                    } else {
                                        Color32::from_rgb(28, 32, 42)
                                    };
                                    egui::Frame::NONE
                                        .fill(fill)
                                        .corner_radius(CornerRadius::same(5))
                                        .inner_margin(Margin::symmetric(6, 3))
                                        .show(ui, |ui| {
                                            let ocr_tag = if m.is_ocr { " · image" } else { "" };
                                            let btn = egui::Button::new(
                                                egui::RichText::new(format!(
                                                    "p. {}{} : {}",
                                                    m.page_index + 1,
                                                    ocr_tag,
                                                    m.snippet
                                                ))
                                                .size(10.5)
                                                .color(if is_curr {
                                                    Color32::from_rgb(255, 220, 140)
                                                } else {
                                                    Color32::from_rgb(190, 200, 218)
                                                }),
                                            )
                                            .frame(false);
                                            if ui.add(btn).clicked() {
                                                match_to_select = Some(m_idx);
                                            }
                                        });
                                }
                            });
                    });
            } else if has_outline_drawer {
                let chosen = egui::Frame::NONE
                    .inner_margin(Margin::symmetric(10, 2))
                    .show(ui, |ui| self.render_outline(ui, drawer_height))
                    .inner;
                if let Some((page, y)) = chosen {
                    pending_cmd = Some(match y {
                        Some(y) => PaletteCommand::JumpToPageAt(page, y),
                        None => PaletteCommand::JumpToPage(page),
                    });
                }
            }
        (pending_cmd, match_to_select)
    }

    /// Lecture approfondie en cours sur l'onglet affiché.
    fn deep_ocr_visible(&self) -> bool {
        self.deep_ocr.as_ref().is_some_and(|run| {
            self.tabs.get(self.active_tab).is_some_and(|t| t.id == run.tab_id)
                && run.progress.lock().is_ok_and(|p| !p.finished)
        })
    }

    /// État de la lecture du texte des images, s'il y a quelque chose à dire :
    /// (texte, détail au survol, travail en cours).
    fn image_ocr_status(&mut self) -> Option<(String, Option<String>, bool)> {
        let image_ocr = self.prefs.image_ocr;
        let installing = self.ocr_install.is_some();
        let tab = self.tabs.get(self.active_tab)?;
        if tab.ocr.deep_running {
            // L'avancement de la lecture approfondie est affiché à part.
            return None;
        }
        if installing {
            return Some(("Installation de la lecture des images".to_string(), None, true));
        }
        if let Some(reason) = &tab.ocr.unavailable_reason {
            return Some(("Lecture des images indisponible".to_string(), Some(reason.clone()), false));
        }
        // Affiché tant qu'il reste des pages, pas seulement pendant le calcul : en mode éco, l'OCR se
        // repose entre deux pages, et la ligne disparaissait puis réapparaissait.
        if image_ocr_working(image_ocr, tab.ocr.mode) && tab.ocr.page_scanned.iter().any(|scanned| !scanned) {
            if let Some(page) = tab.ocr.in_flight_page {
                self.last_image_ocr_page = Some((tab.id, page));
            }
            let label = match self.last_image_ocr_page {
                Some((tab_id, page)) if tab_id == tab.id => format!("Lecture des images, page {}", page + 1),
                _ => "Lecture des images…".to_string(),
            };
            return Some((label, None, true));
        }
        None
    }

    /// Barre du haut (écran portrait) : tout sur une ligne. Onglets rangés dans un bouton, outils,
    /// annuler/rétablir/enregistrer, puis à droite page, zoom, sommaire, recherche et réglages.
    /// Si la fenêtre est étroite, les outils puis le zoom passent dans un menu.
    fn render_top_bar(&mut self, ui: &mut egui::Ui) {
        #[derive(Clone, Copy)]
        enum Nav {
            Previous,
            Next,
            ZoomIn,
            ZoomOut,
        }
        let mut pending_cmd: Option<PaletteCommand> = None;
        let mut tab_to_select: Option<usize> = None;
        let mut tab_to_close: Option<usize> = None;
        let mut tab_menu_action: Option<(usize, TabMenuAction)> = None;
        let mut nav: Option<Nav> = None;

        let gap = 4.0;
        ui.spacing_mut().item_spacing = Vec2::new(gap, 0.0);
        let button = TOP_BAR_BUTTON;
        let sep = 9.0;
        // Largeur de tout sauf le bouton des onglets, selon ce qui est replié.
        let fixed = |full_tools: bool, full_zoom: bool| {
            let tools = if full_tools { 8.0 * button.x + 7.0 * gap } else { 52.0 };
            let zoom = if full_zoom { 4.0 * button.x + 56.0 + 4.0 * gap } else { 78.0 };
            let edit = 3.0 * button.x + 2.0 * gap;
            let page = 2.0 * button.x + 62.0 + 2.0 * gap;
            let right = 3.0 * button.x + 2.0 * gap;
            26.0 + tools + edit + page + zoom + right + 5.0 * sep + 10.0 * gap + 6.0
        };
        let width = ui.available_width();
        let (full_tools, full_zoom) = if width - fixed(true, true) >= TOP_BAR_MIN_TABS_WIDTH {
            (true, true)
        } else if width - fixed(false, true) >= TOP_BAR_MIN_TABS_WIDTH {
            (false, true)
        } else {
            (false, false)
        };
        let tabs_width = (width - fixed(full_tools, full_zoom)).clamp(90.0, 300.0);

        let active_fill = Color32::from_rgb(52, 105, 210);
        let icon_button = |text: &str, selected: bool| {
            let mut b = egui::Button::new(
                egui::RichText::new(text)
                    .size(14.0)
                    .color(if selected { Color32::WHITE } else { Color32::from_rgb(190, 200, 220) }),
            )
            .min_size(button)
            .corner_radius(CornerRadius::same(7));
            if selected {
                b = b.fill(active_fill);
            }
            b
        };

        let viewport_size = self.canvas_size;
        let page_info = self.tabs.get(self.active_tab).map(|tab| {
            let current = tab.viewport.current_page_index(viewport_size, &tab.page_dimensions) + 1;
            (current, tab.page_dimensions.len(), tab.viewport.zoom)
        });
        let has_tabs = !self.tabs.is_empty();
        let ocr_status = self.image_ocr_status();

        ui.horizontal_centered(|ui| {
            // Logo
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(22.0)));
            }

            // Onglets : le document affiché et le nombre d'onglets ; la liste s'ouvre au clic.
            let popup_id = ui.make_persistent_id("kite_tabs_popup");
            let popup_open = ui.memory(|m| m.is_popup_open(popup_id));
            let (rect, response) = ui.allocate_exact_size(Vec2::new(tabs_width, 30.0), Sense::click());
            let response = response.on_hover_text("Onglets ouverts (Ctrl+Tab : onglet suivant)");
            let active = self.tabs.get(self.active_tab);
            let accent = active.map(|t| t.badge_color).unwrap_or(Color32::from_rgb(80, 120, 220));
            let fill = if popup_open || response.hovered() {
                Color32::from_rgb(46, 56, 80)
            } else {
                Color32::from_rgb(36, 43, 61)
            };
            let painter = ui.painter();
            painter.rect(rect, CornerRadius::same(7), fill, Stroke::new(1.2_f32, accent), StrokeKind::Inside);
            let count = egui::RichText::new(format!("{} ⏷", self.tabs.len())).size(11.5).strong();
            let count_galley = egui::WidgetText::from(count).into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                f32::INFINITY,
                egui::TextStyle::Button,
            );
            let title = match active {
                Some(tab) => format!("{}{}", tab.title, if tab.is_modified { " *" } else { "" }),
                None => "Aucun PDF ouvert".to_string(),
            };
            let mut job = egui::text::LayoutJob::single_section(
                title,
                egui::TextFormat {
                    font_id: FontId::proportional(12.5),
                    color: Color32::WHITE,
                    ..Default::default()
                },
            );
            job.wrap.max_width = (rect.width() - count_galley.size().x - 28.0).max(10.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            job.wrap.overflow_character = Some('…');
            let title_galley = ui.fonts(|f| f.layout_job(job));
            let painter = ui.painter();
            painter.galley(
                Pos2::new(rect.left() + 9.0, rect.center().y - title_galley.size().y * 0.5),
                title_galley,
                Color32::WHITE,
            );
            painter.galley(
                Pos2::new(rect.right() - 9.0 - count_galley.size().x, rect.center().y - count_galley.size().y * 0.5),
                count_galley,
                Color32::from_rgb(170, 195, 255),
            );
            if response.clicked() || std::mem::take(&mut self.open_tabs_menu_pending) {
                ui.memory_mut(|m| m.toggle_popup(popup_id));
            }
            egui::popup_below_widget(
                ui,
                popup_id,
                &response,
                egui::PopupCloseBehavior::CloseOnClickOutside,
                |ui| {
                    ui.set_min_width(420.0);
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                    egui::ScrollArea::vertical().max_height(520.0).show(ui, |ui| {
                        for (idx, tab) in self.tabs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let is_active = idx == self.active_tab;
                                // Ligne cliquable : pastille de couleur de l'onglet et nom, aligné à gauche.
                                let row_width = (ui.available_width() - 30.0).max(120.0);
                                let (rect, row) = ui.allocate_exact_size(Vec2::new(row_width, 28.0), Sense::click());
                                let fill = if is_active {
                                    Color32::from_rgb(40, 52, 80)
                                } else if row.hovered() {
                                    Color32::from_rgb(34, 40, 54)
                                } else {
                                    Color32::TRANSPARENT
                                };
                                ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
                                ui.painter().circle_filled(
                                    Pos2::new(rect.left() + 12.0, rect.center().y),
                                    4.0,
                                    tab.badge_color,
                                );
                                let mut name = egui::text::LayoutJob::single_section(
                                    format!("{}{}", tab.title, if tab.is_modified { " *" } else { "" }),
                                    egui::TextFormat {
                                        font_id: FontId::proportional(12.5),
                                        color: if is_active { Color32::WHITE } else { Color32::from_rgb(190, 200, 218) },
                                        ..Default::default()
                                    },
                                );
                                name.wrap.max_width = rect.width() - 34.0;
                                name.wrap.max_rows = 1;
                                name.wrap.break_anywhere = true;
                                name.wrap.overflow_character = Some('…');
                                let galley = ui.fonts(|f| f.layout_job(name));
                                ui.painter().galley(
                                    Pos2::new(rect.left() + 24.0, rect.center().y - galley.size().y * 0.5),
                                    galley,
                                    Color32::WHITE,
                                );
                                let row = row.on_hover_text(tab.path.display().to_string());
                                if row.clicked() {
                                    tab_to_select = Some(idx);
                                }
                                row.context_menu(|ui| tab_context_menu(ui, idx, tab, &mut tab_menu_action));
                                if ui.small_button("✖").on_hover_text("Fermer cet onglet (Ctrl+W)").clicked() {
                                    tab_to_close = Some(idx);
                                }
                            });
                        }
                    });
                    if has_tabs {
                        ui.separator();
                    }
                    if ui.button("Ouvrir un PDF…   Ctrl+O").clicked() {
                        pending_cmd = Some(PaletteCommand::OpenFileDialog);
                        ui.memory_mut(|m| m.close_popup());
                    }
                    if ui.button("Rouvrir un PDF récent…   Ctrl+T").clicked() {
                        self.command_palette.open();
                        ui.memory_mut(|m| m.close_popup());
                    }
                },
            );
            if tab_to_select.is_some() || tab_to_close.is_some() {
                ui.memory_mut(|m| m.close_popup());
            }

            ui.separator();

            // Outils
            if full_tools {
                for tool in ActiveTool::all() {
                    let (icon, label, shortcut) = tool.icon_and_label();
                    if ui
                        .add(icon_button(icon, self.active_tool == *tool))
                        .on_hover_text(format!("{label} (touche {shortcut})"))
                        .clicked()
                    {
                        self.active_tool = *tool;
                    }
                }
            } else {
                let (icon, _, _) = self.active_tool.icon_and_label();
                let tools_popup = ui.make_persistent_id("kite_tools_popup");
                let response = ui
                    .add(icon_button(&format!("{icon} ⏷"), true).min_size(Vec2::new(52.0, button.y)))
                    .on_hover_text("Outil");
                if response.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(tools_popup));
                }
                egui::popup_below_widget(
                    ui,
                    tools_popup,
                    &response,
                    egui::PopupCloseBehavior::CloseOnClick,
                    |ui| {
                        ui.set_min_width(220.0);
                        for tool in ActiveTool::all() {
                            let (icon, label, shortcut) = tool.icon_and_label();
                            let text = format!("{icon}  {label}   {shortcut}");
                            if ui.selectable_label(self.active_tool == *tool, text).clicked() {
                                self.active_tool = *tool;
                            }
                        }
                    },
                );
            }

            ui.separator();

            // Annuler, rétablir, enregistrer
            if ui.add(icon_button("↩", false)).on_hover_text("Annuler (Ctrl+Z)").clicked() {
                pending_cmd = Some(PaletteCommand::UndoAnnotation);
            }
            if ui.add(icon_button("↪", false)).on_hover_text("Rétablir (Ctrl+Y)").clicked() {
                pending_cmd = Some(PaletteCommand::RedoAnnotation);
            }
            if ui
                .add(icon_button("💾", false))
                .on_hover_text("Enregistrer les annotations dans le PDF (Ctrl+S)")
                .clicked()
            {
                pending_cmd = Some(PaletteCommand::SaveAnnotatedPdf);
            }

            // À droite, de droite à gauche : réglages, recherche, sommaire, zoom, page.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(icon_button("⚙", false)).on_hover_text("Réglages (Ctrl+,)").clicked() {
                    self.settings_open = true;
                }
                if ui
                    .add(icon_button("🔎", self.search_bar.is_open))
                    .on_hover_text("Chercher un mot dans le PDF (Ctrl+F)")
                    .clicked()
                {
                    pending_cmd = Some(PaletteCommand::OpenSearch);
                }
                if ui
                    .add_enabled(has_tabs, icon_button("☰", self.outline_open))
                    .on_hover_text("Sommaire du document")
                    .clicked()
                {
                    self.outline_open = !self.outline_open;
                }
                ui.separator();

                let zoom = page_info.map(|(_, _, z)| z).unwrap_or(1.0);
                if full_zoom {
                    if ui
                        .add_enabled(has_tabs, icon_button("↕", false))
                        .on_hover_text("Page entière (Ctrl+0)")
                        .clicked()
                    {
                        pending_cmd = Some(PaletteCommand::FitToPage);
                    }
                    if ui
                        .add_enabled(has_tabs, icon_button("↔", false))
                        .on_hover_text("Toute la largeur (Ctrl+9)")
                        .clicked()
                    {
                        pending_cmd = Some(PaletteCommand::FitToWidth);
                    }
                    if ui.add_enabled(has_tabs, icon_button("+", false)).on_hover_text("Zoom avant").clicked() {
                        nav = Some(Nav::ZoomIn);
                    }
                    let label = egui::Button::new(egui::RichText::new(format!("{:.0} %", zoom * 100.0)).size(12.0))
                        .frame(false);
                    if ui
                        .add_enabled_ui(has_tabs, |ui| ui.add_sized([56.0, 28.0], label))
                        .inner
                        .on_hover_text("Revenir à 100 %")
                        .clicked()
                    {
                        pending_cmd = Some(PaletteCommand::ZoomReset100);
                    }
                    if ui.add_enabled(has_tabs, icon_button("−", false)).on_hover_text("Zoom arrière").clicked() {
                        nav = Some(Nav::ZoomOut);
                    }
                } else {
                    ui.add_enabled_ui(has_tabs, |ui| {
                        ui.menu_button(egui::RichText::new(format!("{:.0} % ⏷", zoom * 100.0)).size(12.5), |ui| {
                            let entry = |ui: &mut egui::Ui, text: &str| {
                                let clicked = ui.button(text).clicked();
                                if clicked {
                                    ui.close_menu();
                                }
                                clicked
                            };
                            if entry(ui, "＋  Zoom avant") {
                                nav = Some(Nav::ZoomIn);
                            }
                            if entry(ui, "−  Zoom arrière") {
                                nav = Some(Nav::ZoomOut);
                            }
                            if entry(ui, "100 %") {
                                pending_cmd = Some(PaletteCommand::ZoomReset100);
                            }
                            if entry(ui, "↔  Toute la largeur   Ctrl+9") {
                                pending_cmd = Some(PaletteCommand::FitToWidth);
                            }
                            if entry(ui, "↕  Page entière   Ctrl+0") {
                                pending_cmd = Some(PaletteCommand::FitToPage);
                            }
                        });
                    });
                }
                ui.separator();

                let (current, count) = page_info.map(|(c, n, _)| (c, n)).unwrap_or((0, 0));
                if ui
                    .add_enabled(current < count, icon_button("▶", false))
                    .on_hover_text("Page suivante")
                    .clicked()
                {
                    nav = Some(Nav::Next);
                }
                let page_label = egui::Button::new(
                    egui::RichText::new(format!("{current} / {count}")).size(12.0).color(Color32::WHITE),
                )
                .frame(false);
                if ui
                    .add_enabled_ui(has_tabs, |ui| ui.add_sized([62.0, 28.0], page_label))
                    .inner
                    .on_hover_text("Aller à une page : Ctrl+T, puis le numéro")
                    .clicked()
                {
                    self.command_palette.open();
                }
                if ui
                    .add_enabled(current > 1, icon_button("◀", false))
                    .on_hover_text("Page précédente")
                    .clicked()
                {
                    nav = Some(Nav::Previous);
                }

                // Lecture des images en cours : une roue discrète, le détail au survol.
                if let Some((label, detail, working)) = &ocr_status {
                    let hover = match detail {
                        Some(detail) => format!("{label}\n{detail}"),
                        None => label.clone(),
                    };
                    if *working {
                        ui.add(egui::Spinner::new().size(13.0).color(Color32::from_rgb(140, 150, 172)))
                            .on_hover_text(hover);
                    } else {
                        ui.label(egui::RichText::new("⚠").color(Color32::from_rgb(250, 200, 110)))
                            .on_hover_text(hover);
                    }
                }
            });
        });

        if let (Some(nav), Some(tab)) = (nav, self.tabs.get_mut(self.active_tab)) {
            let pages = tab.page_dimensions.clone();
            let current = tab.viewport.current_page_index(viewport_size, &pages);
            match nav {
                Nav::Previous => {
                    tab.viewport.jump_to_page(current.saturating_sub(1), viewport_size, &pages);
                }
                Nav::Next => {
                    tab.viewport.jump_to_page(current + 1, viewport_size, &pages);
                }
                Nav::ZoomIn => {
                    tab.viewport.apply_zoom_around_anchor(1.2, viewport_size * 0.5, viewport_size, &pages);
                }
                Nav::ZoomOut => {
                    tab.viewport.apply_zoom_around_anchor(1.0 / 1.2, viewport_size * 0.5, viewport_size, &pages);
                }
            }
        }
        if let Some((idx, action)) = tab_menu_action {
            self.apply_tab_menu_action(idx, action);
        }
        if let Some(idx) = tab_to_select {
            self.switch_to_tab(idx);
        }
        if let Some(idx) = tab_to_close {
            self.close_tab(idx);
        }
        if let Some(cmd) = pending_cmd {
            self.execute_palette_command(cmd, self.canvas_size);
        }
    }

    /// Volet du bas (disposition portrait) : lecture approfondie en cours et tiroir (sommaire,
    /// résultats de recherche, annotation choisie), avec un bord à glisser pour sa hauteur.
    fn render_bottom_drawer(&mut self, ctx: &Context) {
        let (annotation, search, outline) = self.drawer_kinds();
        let has_drawer = annotation || search || outline;
        let deep_ocr = self.deep_ocr_visible();
        if !has_drawer && !deep_ocr {
            return;
        }
        let max_height = (ctx.screen_rect().height() * 0.6).max(120.0);
        self.sidebar.drawer_height = self.sidebar.drawer_height.clamp(80.0, max_height);
        let drawer_height = self.sidebar.drawer_height;
        let mut outcome = (None, None);
        egui::TopBottomPanel::bottom("kite_bottom_drawer")
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(23, 26, 34))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 50, 66)))
                    .inner_margin(Margin::symmetric(8, 0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                if has_drawer {
                    let (splitter, response) =
                        ui.allocate_exact_size(Vec2::new(ui.available_width(), DRAWER_SPLITTER_PX), Sense::drag());
                    let active = response.hovered() || response.dragged();
                    let handle = Rect::from_center_size(splitter.center(), Vec2::new(48.0, 4.0));
                    ui.painter().rect_filled(
                        handle,
                        CornerRadius::same(2),
                        if active { Color32::from_rgb(110, 150, 230) } else { Color32::from_rgb(58, 69, 96) },
                    );
                    if active {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }
                    if response.dragged() {
                        self.sidebar.drawer_height =
                            (self.sidebar.drawer_height - response.drag_delta().y).clamp(80.0, max_height);
                    }
                }
                if deep_ocr {
                    ui.add_space(4.0);
                    self.render_deep_ocr_progress(ui);
                }
                if has_drawer {
                    let width = ui.available_width();
                    ui.allocate_ui_with_layout(
                        Vec2::new(width, drawer_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_height(drawer_height);
                            ui.set_clip_rect(ui.max_rect());
                            outcome = self.render_drawer_contents(ui, drawer_height);
                        },
                    );
                }
            });
        let (cmd, matched) = outcome;
        if let Some(m_idx) = matched {
            self.search_bar.current_match_index = m_idx;
            self.scroll_to_current_search_match(self.canvas_size);
        }
        if let Some(cmd) = cmd {
            self.execute_palette_command(cmd, self.canvas_size);
        }
    }

    /// Bord droit de la barre latérale, à glisser pour changer sa largeur (épinglée ou non).
    fn render_sidebar_resize_handle(&mut self, ctx: &Context) {
        if self.sidebar.anim_progress < 0.98 {
            self.sidebar.resizing = false;
            return;
        }
        // Barre épinglée : bord à sa largeur ; barre au survol : elle glisse depuis la gauche.
        let edge = self.sidebar.width * self.sidebar.anim_progress;
        let screen = ctx.screen_rect();
        let grab = Rect::from_min_max(
            Pos2::new(edge - SIDEBAR_HANDLE_PX * 0.5, screen.top()),
            Pos2::new(edge + SIDEBAR_HANDLE_PX * 0.5, screen.bottom()),
        );
        egui::Area::new(egui::Id::new("kite_sidebar_resize"))
            .fixed_pos(grab.min)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(grab.size(), Sense::drag());
                let active = response.hovered() || response.dragged();
                if active {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    ui.painter().line_segment(
                        [Pos2::new(rect.center().x, rect.top()), Pos2::new(rect.center().x, rect.bottom())],
                        Stroke::new(2.0_f32, Color32::from_rgb(110, 150, 230)),
                    );
                }
                self.sidebar.resizing = response.dragged();
                if response.dragged() {
                    self.sidebar.width = (self.sidebar.width + response.drag_delta().x)
                        .clamp(*SIDEBAR_WIDTH_RANGE.start(), *SIDEBAR_WIDTH_RANGE.end());
                }
            });
    }

    fn render_autohide_sidebar_overlay(&mut self, ctx: &Context, canvas_rect: Rect) {
        let painter = ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("kite_edge_indicator"),
        ));

        let edge_strip = Rect::from_min_size(canvas_rect.min, Vec2::new(4.0, canvas_rect.height()));
        painter.rect_filled(
            edge_strip,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(80, 145, 255, 110),
        );

        if self.sidebar.anim_progress < 0.05 {
            return;
        }

        let x_offset = -self.sidebar.width * (1.0 - self.sidebar.anim_progress);
        let overlay_rect = Rect::from_min_size(
            Pos2::new(x_offset, 8.0),
            Vec2::new(self.sidebar.width, canvas_rect.height() - 16.0),
        );

        egui::Area::new(egui::Id::new("kite_floating_sidebar_overlay"))
            .fixed_pos(overlay_rect.min)
            // Sans cela egui ramène la zone dans l'écran et la fait apparaître en fondu : la barre
            // surgissait en place au lieu de glisser depuis le bord.
            .constrain(false)
            .fade_in(false)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(22, 25, 34, 248))
                    .stroke(Stroke::new(1.5_f32, Color32::from_rgb(80, 130, 230)))
                    .corner_radius(CornerRadius::same(12))
                    .show(ui, |ui| {
                        ui.set_width(self.sidebar.width);
                        ui.set_height(overlay_rect.height());
                        self.render_sidebar_contents(ui);
                    });
            });
    }

    /// Page Réglages : applique et enregistre chaque changement.
    fn render_settings(&mut self, ctx: &Context) {
        if !self.settings_open {
            self.confirm_clear_history = false;
            return;
        }
        use crate::settings_page::{Action, OcrStatus, PageInfo};
        let ocr = if self.ocr_install.is_some() {
            OcrStatus::Downloading
        } else if crate::deps::missing().iter().any(|d| !d.required) {
            OcrStatus::Missing
        } else {
            OcrStatus::Installed
        };
        let info = PageInfo {
            ocr,
            history_len: self.recent_files.items.len(),
            confirm_clear_history: self.confirm_clear_history,
            update_check: crate::updates::manual_status(),
        };
        let mut prefs = self.prefs.clone();
        let actions = crate::settings_page::show(ctx, &mut prefs, &info);
        if prefs != self.prefs {
            self.prefs = prefs;
            if let Some(settings) = &mut self.settings {
                settings.prefs = self.prefs.clone();
                settings.save();
            }
        }
        for action in actions {
            match action {
                Action::Close => self.settings_open = false,
                Action::AskClearHistory => self.confirm_clear_history = true,
                Action::CancelClearHistory => self.confirm_clear_history = false,
                Action::ConfirmClearHistory => {
                    self.recent_files.clear();
                    self.confirm_clear_history = false;
                }
                Action::InstallOcr => {
                    let missing: Vec<_> = crate::deps::missing().into_iter().filter(|d| !d.required).collect();
                    if self.ocr_install.is_none() && !missing.is_empty() {
                        self.ocr_install = Some(crate::setup::InstallJob::start(missing, ctx.clone()));
                    }
                }
                Action::MakeDefaultReader => {
                    let uri = format!("ms-settings:defaultapps?registeredAppUser={}", crate::APP_NAME);
                    let _ = std::process::Command::new("explorer").arg(uri).spawn();
                }
                Action::CheckUpdatesNow => crate::updates::check_now(ctx.clone()),
                Action::OpenNewRelease => {
                    if let crate::updates::ManualCheck::Available(release) = crate::updates::manual_status() {
                        let _ = std::process::Command::new("explorer").arg(&release.page_url).spawn();
                    }
                }
                Action::OpenProjectPage => {
                    let _ = std::process::Command::new("explorer").arg("https://github.com/padimato/kite-pdf").spawn();
                }
                Action::ShowShortcuts => {
                    self.settings_open = false;
                    self.shortcuts_open = true;
                }
                Action::ShowWelcome => {
                    self.settings_open = false;
                    self.welcome_open = true;
                }
            }
        }
    }

    /// Accueil du premier lancement et aide-mémoire des raccourcis.
    fn render_help(&mut self, ctx: &Context) {
        if self.welcome_open {
            if let Some(action) = crate::help::show_welcome(ctx, self.logo.as_ref()) {
                self.welcome_open = false;
                if let Some(settings) = &mut self.settings {
                    settings.welcome_seen = true;
                    settings.save();
                }
                match action {
                    crate::help::WelcomeAction::OpenFile => self.execute_palette_command(PaletteCommand::OpenFileDialog, self.canvas_size),
                    crate::help::WelcomeAction::ShowShortcuts => self.shortcuts_open = true,
                    crate::help::WelcomeAction::Start => {
                        if self.tabs.is_empty() {
                            self.command_palette.open();
                        }
                    }
                }
            }
        } else if self.shortcuts_open {
            self.shortcuts_open = crate::help::show_shortcuts(ctx);
        }
    }

    /// Bandeau discret en bas de la fenêtre : une nouvelle version est publiée.
    fn render_update_banner(&mut self, ctx: &Context, canvas_rect: Rect) {
        if self.update_banner_closed {
            return;
        }
        let Some(release) = crate::updates::available() else {
            return;
        };
        egui::Area::new(egui::Id::new("kite_update_banner"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -16.0))
            .constrain_to(canvas_rect)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(24, 32, 52))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(70, 95, 150)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("Kite PDF {} est disponible.", release.version))
                                .size(12.5)
                                .color(Color32::WHITE),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Voir la nouvelle version").clicked() {
                                let _ = std::process::Command::new("explorer").arg(&release.page_url).spawn();
                                self.update_banner_closed = true;
                            }
                            if ui.button("Ignorer cette version").clicked() {
                                crate::updates::skip(&release.version);
                            }
                            if ui.button("Plus tard").clicked() {
                                self.update_banner_closed = true;
                            }
                        });
                    });
            });
    }

    /// Bandeau signalant une dépendance installée dans une version autre que celle testée.
    fn render_dependency_warning(&mut self, ctx: &Context, canvas_rect: Rect) {
        if self.dependency_warning_closed {
            return;
        }
        let warnings = self.dependency_warnings.lock().ok().and_then(|w| w.clone()).unwrap_or_default();
        if warnings.is_empty() {
            return;
        }
        let width = (canvas_rect.width() - 48.0).min(620.0);
        egui::Area::new(egui::Id::new("kite_dependency_warning"))
            .fixed_pos(Pos2::new(canvas_rect.center().x - width * 0.5, canvas_rect.min.y + 12.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(52, 42, 20))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(150, 115, 40)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(width - 24.0);
                        for warning in &warnings {
                            ui.label(
                                egui::RichText::new(format!("⚠ {}", warning.warning()))
                                    .size(12.0)
                                    .color(Color32::from_rgb(250, 225, 170)),
                            );
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Ne plus afficher").clicked() {
                                for warning in &warnings {
                                    crate::setup::ignore_version(&warning.found_sha256);
                                }
                                self.dependency_warning_closed = true;
                            }
                            if ui.button("Fermer").clicked() {
                                self.dependency_warning_closed = true;
                            }
                        });
                    });
            });
    }

    /// Ouvre la fenêtre de choix de l'OCR approfondi pour l'onglet actif.
    fn open_deep_ocr_dialog(&mut self, viewport_size: Vec2) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        // Le texte de toutes les pages est nécessaire pour savoir lesquelles en manquent.
        tab.request_all_text(&self.workers);
        let current_page = tab.viewport.current_page_index(viewport_size, &tab.page_dimensions);
        self.deep_ocr_dialog = Some(DeepOcrDialog { scope: DeepOcrScope::WholeDocument, current_page });
    }

    /// Lecture approfondie en cours pour cet onglet.
    fn deep_ocr_running_for(&self, tab_id: u64) -> bool {
        self.deep_ocr.as_ref().is_some_and(|run| {
            run.tab_id == tab_id && !run.progress.lock().map(|p| p.finished).unwrap_or(true)
        })
    }

    /// Lance l'OCR approfondi des `pages` de l'onglet actif, en commençant par la page affichée.
    fn start_deep_ocr(&mut self, mut pages: Vec<usize>, current_page: usize) {
        if pages.is_empty() {
            return;
        }
        pages.sort_by_key(|&p| (p as i64 - current_page as i64).unsigned_abs());
        let Some(tab_id) = self.tabs.get(self.active_tab).map(|t| t.id) else {
            return;
        };
        if let Some(run) = &self.deep_ocr {
            if !run.progress.lock().map(|p| p.finished).unwrap_or(true) {
                // Une seule lecture à la fois : sur le même onglet, les pages demandées passent devant.
                if run.tab_id == tab_id {
                    if let Ok(mut progress) = run.progress.lock() {
                        progress.prioritize(&pages);
                        progress.paused = false;
                    }
                    self.no_text_hint = None;
                }
                return;
            }
        }
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        tab.ocr.deep_running = true;
        let progress = self.workers.start_deep_ocr(tab.id, tab.path.clone(), pages);
        self.deep_ocr = Some(DeepOcrRun { tab_id: tab.id, progress, acknowledged: false });
        self.no_text_hint = None;
    }

    /// Fenêtre de l'OCR approfondi : cette page, les pages autour, ou tout le document.
    fn render_deep_ocr_dialog(&mut self, ctx: &Context) {
        if self.deep_ocr_dialog.is_none() {
            return;
        }
        // Une lecture sur un autre onglet bloque ; sur cet onglet, les pages demandées passent devant.
        let Some(tab_id) = self.tabs.get(self.active_tab).map(|t| t.id) else {
            self.deep_ocr_dialog = None;
            return;
        };
        let running_here = self.deep_ocr_running_for(tab_id);
        let busy_elsewhere = !running_here
            && self.deep_ocr.as_ref().is_some_and(|r| !r.progress.lock().map(|p| p.finished).unwrap_or(true));
        let Some(dialog) = self.deep_ocr_dialog.as_mut() else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let page_count = tab.page_count();
        let current = dialog.current_page.min(page_count.saturating_sub(1));
        let around = current.saturating_sub(DEEP_OCR_PAGES_AROUND)..=(current + DEEP_OCR_PAGES_AROUND).min(page_count.saturating_sub(1));
        let pending: Vec<usize> = match dialog.scope {
            DeepOcrScope::CurrentPage => tab.deep_ocr_pending_pages(current..=current),
            DeepOcrScope::Around => tab.deep_ocr_pending_pages(around.clone()),
            DeepOcrScope::WholeDocument => tab.deep_ocr_pending_pages(0..=page_count.saturating_sub(1)),
        };
        let (_, lacking) = tab.text_coverage();
        let seconds_per_page = self
            .deep_ocr
            .as_ref()
            .and_then(|r| r.progress.lock().ok().map(|p| p.seconds_per_page(DEFAULT_DEEP_OCR_SECONDS_PER_PAGE)))
            .unwrap_or(DEFAULT_DEEP_OCR_SECONDS_PER_PAGE);

        let muted = Color32::from_rgb(150, 160, 182);
        let mut start = false;
        let mut close = false;
        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(Order::Foreground, egui::Id::new("deep_ocr_backdrop")))
            .rect_filled(screen, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(6, 8, 12, 150));
        let width = 470.0_f32.min(screen.width() - 32.0);
        egui::Area::new(egui::Id::new("kite_deep_ocr_dialog"))
            .fixed_pos(Pos2::new(screen.center().x - width * 0.5, screen.min.y + screen.height() * 0.2))
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(24, 28, 39))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(70, 100, 170)))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(width - 36.0);
                        ui.label(egui::RichText::new("OCR approfondi").size(17.0).color(Color32::WHITE));
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "L'OCR approfondi relit chaque page entière comme une image, figures \
                                 comprises. Il retrouve le texte scanné ou dessiné que la recherche ne peut \
                                 pas lire, mais il est lent : plusieurs secondes par page. Une fois le \
                                 document enregistré, ce texte est cherchable ici et dans les autres \
                                 lecteurs PDF.",
                            )
                            .size(12.0)
                            .color(muted),
                        );
                        ui.add_space(10.0);
                        ui.radio_value(
                            &mut dialog.scope,
                            DeepOcrScope::CurrentPage,
                            format!("Cette page (p. {})", current + 1),
                        );
                        ui.radio_value(
                            &mut dialog.scope,
                            DeepOcrScope::Around,
                            format!("Pages {} à {} (autour de celle-ci)", around.start() + 1, around.end() + 1),
                        );
                        ui.radio_value(
                            &mut dialog.scope,
                            DeepOcrScope::WholeDocument,
                            format!("Tout le document ({page_count} pages)"),
                        );
                        ui.add_space(10.0);
                        let summary = if pending.is_empty() {
                            "Ces pages ont déjà été lues par l'OCR approfondi.".to_string()
                        } else {
                            format!(
                                "{} page{} à lire · {}",
                                pending.len(),
                                if pending.len() > 1 { "s" } else { "" },
                                format_duration(pending.len() as f32 * seconds_per_page)
                            )
                        };
                        ui.label(egui::RichText::new(summary).size(12.5).color(Color32::WHITE));
                        let mut notes = Vec::new();
                        if lacking > 0 {
                            notes.push(format!("{lacking} pages n'ont aucun texte lisible."));
                        }
                        notes.push("Le texte déjà présent n'est pas dupliqué.".to_string());
                        if running_here {
                            notes.push("Une lecture est en cours : ces pages passeront en premier.".to_string());
                        } else {
                            notes.push("Vous pouvez continuer à lire, mettre en pause ou arrêter.".to_string());
                        }
                        for note in notes {
                            ui.label(egui::RichText::new(note).size(11.0).color(muted));
                        }
                        if busy_elsewhere {
                            ui.label(
                                egui::RichText::new("Une lecture est en cours dans un autre onglet : attendez sa fin ou arrêtez-la.")
                                    .size(11.0)
                                    .color(Color32::from_rgb(250, 200, 110)),
                            );
                        }
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!pending.is_empty() && !busy_elsewhere, egui::Button::new("Lancer l'OCR approfondi"))
                                .clicked()
                            {
                                start = true;
                            }
                            if ui.button("Annuler").clicked() {
                                close = true;
                            }
                        });
                    });
            });

        if start {
            self.deep_ocr_dialog = None;
            self.start_deep_ocr(pending, current);
        } else if close {
            self.deep_ocr_dialog = None;
        }
    }

    /// Avancement dans la barre latérale : pages lues, temps restant, pause et arrêt.
    fn render_deep_ocr_progress(&mut self, ui: &mut egui::Ui) {
        let Some(run) = &self.deep_ocr else {
            return;
        };
        let is_active_tab = self.tabs.get(self.active_tab).is_some_and(|t| t.id == run.tab_id);
        let Ok(progress) = run.progress.lock().map(|p| p.clone()) else {
            return;
        };
        if progress.finished || !is_active_tab {
            return;
        }
        let muted = Color32::from_rgb(150, 160, 182);
        let remaining = (progress.total - progress.done) as f32
            * progress.seconds_per_page(DEFAULT_DEEP_OCR_SECONDS_PER_PAGE);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("OCR approfondi {} / {}", progress.done, progress.total))
                    .size(11.0)
                    .color(Color32::WHITE),
            );
            ui.label(
                egui::RichText::new(if progress.paused { "en pause".to_string() } else { format_duration(remaining) })
                    .size(10.5)
                    .color(muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Arrêter").on_hover_text("Le texte déjà lu est conservé").clicked() {
                    if let Ok(mut p) = run.progress.lock() {
                        p.cancelled = true;
                    }
                }
                let label = if progress.paused { "Reprendre" } else { "Pause" };
                if ui.small_button(label).clicked() {
                    if let Ok(mut p) = run.progress.lock() {
                        p.paused = !p.paused;
                    }
                }
            });
        });
        let fraction = progress.done as f32 / progress.total.max(1) as f32;
        ui.add(egui::ProgressBar::new(fraction).desired_height(4.0));
        ui.add_space(4.0);
    }

    /// Fin de lecture : invitation à enregistrer pour garder le texte.
    fn render_deep_ocr_result(&mut self, ctx: &Context, canvas_rect: Rect) {
        let Some(run) = &self.deep_ocr else {
            return;
        };
        if run.acknowledged {
            return;
        }
        let Ok(progress) = run.progress.lock().map(|p| p.clone()) else {
            return;
        };
        if !progress.finished {
            return;
        }
        let Some(tab_index) = self.tabs.iter().position(|t| t.id == run.tab_id) else {
            self.deep_ocr = None;
            return;
        };
        let (message, can_save) = match &progress.error {
            Some(error) => (format!("L'OCR approfondi s'est arrêté : {error}"), progress.read > 0),
            None if progress.read == 0 => ("OCR approfondi arrêté avant la première page.".to_string(), false),
            None => (
                format!(
                    "OCR approfondi {} : texte lu sur {} page{}. Enregistrez le document pour le \
                     garder : il sera cherchable ici et dans les autres lecteurs PDF.",
                    if progress.cancelled { "arrêté" } else { "terminé" },
                    progress.read,
                    if progress.read > 1 { "s" } else { "" }
                ),
                true,
            ),
        };
        let mut save = false;
        let mut dismiss = false;
        let width = (canvas_rect.width() - 48.0).min(520.0);
        egui::Area::new(egui::Id::new("kite_deep_ocr_result"))
            .fixed_pos(Pos2::new(canvas_rect.center().x - width * 0.5, canvas_rect.max.y - 130.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(26, 34, 48))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(80, 130, 210)))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(width - 28.0);
                        ui.label(egui::RichText::new(message).size(12.5).color(Color32::WHITE));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if can_save && ui.button("Enregistrer").clicked() {
                                save = true;
                            }
                            if ui.button(if can_save { "Plus tard" } else { "Fermer" }).clicked() {
                                dismiss = true;
                            }
                        });
                    });
            });
        if save || dismiss {
            if let Some(run) = &mut self.deep_ocr {
                run.acknowledged = true;
            }
        }
        if save {
            if self.active_tab != tab_index {
                self.switch_to_tab(tab_index);
            }
            self.save_active_tab_annotations();
        }
    }

    /// Bulle proposée quand on essaie de sélectionner du texte sur une page qui n'en a pas.
    fn render_no_text_hint(&mut self, ctx: &Context) {
        let Some(hint) = &self.no_text_hint else {
            return;
        };
        if hint.shown_at.elapsed() > NO_TEXT_HINT_DURATION {
            self.no_text_hint = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let (page, position) = (hint.page, hint.position);
        let mut read_page = false;
        let mut read = false;
        let mut close = false;
        egui::Area::new(egui::Id::new("kite_no_text_hint"))
            .fixed_pos(position + Vec2::new(12.0, 12.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(26, 34, 48))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(80, 130, 210)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Cette page n'a pas de texte sélectionnable.")
                                .size(12.0)
                                .color(Color32::WHITE),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .button("Lire cette page")
                                .on_hover_text("OCR approfondi de cette page seulement (quelques secondes)")
                                .clicked()
                            {
                                read_page = true;
                            }
                            if ui.button("OCR approfondi…").clicked() {
                                read = true;
                            }
                            if ui.small_button("✖").clicked() {
                                close = true;
                            }
                        });
                    });
            });
        if read_page {
            self.start_deep_ocr(vec![page], page);
        }
        if read {
            let viewport = self.canvas_size;
            self.open_deep_ocr_dialog(viewport);
        }
        if read_page || read || close {
            self.no_text_hint = None;
        }
    }

    /// Barre centrale (`Ctrl+T`) : retrouve un PDF de l'historique, un onglet, une page ou un outil.
    fn render_command_palette(&mut self, ctx: &Context, canvas_rect: Rect) {
        let tabs_info: Vec<(usize, String, usize, PathBuf)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.title.clone(), t.page_count(), t.path.clone()))
            .collect();
        let active_pages = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.page_count())
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let entries = self.command_palette.filtered_entries(
            &tabs_info,
            active_pages,
            &self.recent_files,
            now,
            &self.number_history,
        );
        let typed_bare_number =
            crate::tabs::bare_number(&crate::recent_files::normalize(&self.command_palette.query)).is_some();
        if self.command_palette.selected_index >= entries.len() {
            self.command_palette.selected_index = entries.len().saturating_sub(1);
        }

        let mut command_to_run: Option<PaletteCommand> = None;
        let mut should_close = false;
        let mut selection_moved_by_keys = false;

        ctx.input(|input| {
            if input.key_pressed(Key::ArrowDown) && !entries.is_empty() {
                self.command_palette.selected_index =
                    (self.command_palette.selected_index + 1) % entries.len();
                selection_moved_by_keys = true;
            }
            if input.key_pressed(Key::ArrowUp) && !entries.is_empty() {
                self.command_palette.selected_index =
                    (self.command_palette.selected_index + entries.len() - 1) % entries.len();
                selection_moved_by_keys = true;
            }
            if input.key_pressed(Key::Enter) {
                if let Some(chosen) = entries.get(self.command_palette.selected_index) {
                    command_to_run = Some(chosen.command.clone());
                    should_close = true;
                }
            }
        });

        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("palette_backdrop"),
        ))
        .rect_filled(screen, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(6, 8, 12, 170));

        let bar_w = 640.0_f32.min(screen.width() - 32.0);
        let bar_pos = Pos2::new(
            screen.center().x - bar_w * 0.5,
            screen.min.y + (screen.height() * 0.2).max(24.0),
        );
        let muted = Color32::from_rgb(135, 145, 168);

        let area = egui::Area::new(egui::Id::new("kite_command_palette_modal"))
            .fixed_pos(bar_pos)
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(24, 28, 39))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(70, 100, 170)))
                    .corner_radius(CornerRadius::same(14))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 36,
                        spread: 0,
                        color: Color32::from_black_alpha(140),
                    })
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(bar_w - 24.0);

                        // Champ de saisie, grand et sans cadre, comme une barre de recherche système.
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("🔍").size(18.0).color(muted));
                            let text_edit =
                                egui::TextEdit::singleline(&mut self.command_palette.query)
                                    .hint_text("Rechercher un PDF, un outil, une page…")
                                    .font(FontId::proportional(18.0))
                                    .frame(false)
                                    .desired_width(ui.available_width());
                            let resp = ui.add(text_edit);
                            self.text_field_focused_now |= resp.has_focus();
                            if self.command_palette.focus_requested {
                                resp.request_focus();
                                self.command_palette.focus_requested = false;
                            }
                            if resp.changed() {
                                self.command_palette.selected_index = 0;
                            }
                        });

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);

                        if entries.is_empty() {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("Aucun résultat dans l'historique ni les outils.")
                                    .color(muted),
                            );
                            ui.add_space(6.0);
                        } else if self.command_palette.query.trim().is_empty()
                            && entries.iter().any(|e| e.category == "Récent")
                        {
                            ui.label(egui::RichText::new("RÉCENTS").size(10.5).strong().color(muted));
                            ui.add_space(2.0);
                        }

                        egui::ScrollArea::vertical()
                            .max_height(400.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                for (idx, entry) in entries.iter().enumerate() {
                                    let is_selected = idx == self.command_palette.selected_index;
                                    let row = egui::Frame::NONE
                                        .fill(if is_selected {
                                            Color32::from_rgb(45, 78, 150)
                                        } else {
                                            Color32::TRANSPARENT
                                        })
                                        .corner_radius(CornerRadius::same(8))
                                        .inner_margin(Margin::symmetric(10, 6))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.horizontal(|ui| {
                                                let (icon, icon_color) = palette_entry_icon(entry);
                                                ui.label(egui::RichText::new(icon).size(16.0).color(icon_color));
                                                ui.vertical(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(&entry.label)
                                                            .size(14.0)
                                                            .color(Color32::WHITE),
                                                    );
                                                    if !entry.detail.is_empty() {
                                                        ui.label(
                                                            egui::RichText::new(&entry.detail)
                                                                .size(11.0)
                                                                .color(if is_selected {
                                                                    Color32::from_rgb(190, 205, 235)
                                                                } else {
                                                                    muted
                                                                }),
                                                        );
                                                    }
                                                });
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(egui::Align::Center),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(entry.shortcut)
                                                                .monospace()
                                                                .size(11.0)
                                                                .color(muted),
                                                        );
                                                    },
                                                );
                                            });
                                        })
                                        .response;
                                    let row = ui.interact(
                                        row.rect,
                                        egui::Id::new(("palette_row", idx)),
                                        Sense::click(),
                                    );
                                    if row.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                                        self.command_palette.selected_index = idx;
                                    }
                                    if row.clicked() {
                                        command_to_run = Some(entry.command.clone());
                                        should_close = true;
                                    }
                                    if is_selected && selection_moved_by_keys {
                                        row.scroll_to_me(None);
                                    }
                                }
                            });

                        ui.add_space(6.0);
                        let hint = Color32::from_rgb(105, 115, 138);
                        ui.horizontal(|ui| {
                            // Les flèches n'existent que dans la police à chasse fixe d'egui.
                            ui.label(egui::RichText::new("↑↓").monospace().size(11.0).color(hint));
                            ui.label(
                                egui::RichText::new("choisir   ·   Entrée ouvrir   ·   Échap fermer")
                                    .size(10.5)
                                    .color(hint),
                            );
                        });
                    });
            });

        // Un clic hors de la barre la ferme.
        let clicked_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p))
        });
        if clicked_outside {
            should_close = true;
        }

        if should_close {
            self.command_palette.close();
        }
        if let Some(cmd) = command_to_run {
            if typed_bare_number {
                match cmd {
                    PaletteCommand::JumpToPage(_) => self.number_history.record(crate::tabs::NumberChoice::Page),
                    PaletteCommand::SetZoomPercent(_) => self.number_history.record(crate::tabs::NumberChoice::Zoom),
                    _ => {}
                }
            }
            self.execute_palette_command(cmd, canvas_rect.size());
        }
    }
}

/// Icône et couleur d'une ligne de la barre centrale selon sa nature.
fn palette_entry_icon(entry: &crate::tabs::PaletteEntry) -> (&'static str, Color32) {
    match entry.category {
        "Récent" => ("📄", Color32::from_rgb(235, 120, 110)),
        "Zoom" => ("🔍", Color32::from_rgb(140, 220, 170)),
        "Onglet" => ("📑", Color32::from_rgb(120, 190, 255)),
        "Page" => ("↪", Color32::from_rgb(140, 220, 170)),
        "Outil" => ("🛠", Color32::from_rgb(250, 205, 110)),
        _ => ("⚡", Color32::from_rgb(170, 160, 250)),
    }
}

/// L'OCR des images travaille-t-il en ce moment, d'après les réglages ? (sinon, pas de ligne d'état)
/// La barre latérale occupe le bord gauche de l'écran (épinglée, ou ouverte au survol). Dans la
/// disposition portrait, la barre du haut la remplace : rien à gauche.
fn sidebar_shown(sidebar: &SidebarState, top_layout: bool) -> bool {
    !top_layout && sidebar.is_expanded()
}

/// Menu du clic droit sur un onglet (liste de la barre latérale ou de la barre du haut).
fn tab_context_menu(ui: &mut egui::Ui, idx: usize, tab: &DocumentTab, chosen: &mut Option<(usize, TabMenuAction)>) {
    let mut choose = |ui: &mut egui::Ui, label: &str, action: TabMenuAction| {
        if ui.button(label).clicked() {
            *chosen = Some((idx, action));
            ui.close_menu();
        }
    };
    choose(ui, "Afficher dans l'explorateur de fichiers", TabMenuAction::RevealInExplorer);
    choose(ui, "Ouvrir avec l'application par défaut", TabMenuAction::OpenWithDefaultApp);
    ui.separator();
    if ui.button("Copier le chemin complet").clicked() {
        let full = std::path::absolute(&tab.path).unwrap_or_else(|_| tab.path.clone());
        ui.ctx().copy_text(full.display().to_string());
        ui.close_menu();
    }
    if ui.button("Copier le nom du fichier").clicked() {
        ui.ctx().copy_text(tab.title.clone());
        ui.close_menu();
    }
    ui.separator();
    choose(ui, "Fermer", TabMenuAction::CloseThis);
    choose(ui, "Fermer les autres onglets", TabMenuAction::CloseOthers);
    choose(ui, "Fermer les onglets en dessous", TabMenuAction::CloseBelow);
}

fn image_ocr_working(setting: crate::settings::ImageOcr, mode: crate::ocr_engine::OcrSpeedMode) -> bool {
    match setting {
        crate::settings::ImageOcr::Auto => true,
        crate::settings::ImageOcr::OnSearch => mode == crate::ocr_engine::OcrSpeedMode::TurboSearch,
        crate::settings::ImageOcr::Never => false,
    }
}

/// Cerf-volant du logo, sans fond (la barre latérale est déjà sombre), lissé pour un affichage en petite taille.
fn load_logo(ctx: &Context) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(include_bytes!("../assets/icon/kite-mark-64.png")).ok()?.into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    let options = egui::TextureOptions {
        mipmap_mode: Some(egui::TextureFilter::Linear),
        ..egui::TextureOptions::LINEAR
    };
    Some(ctx.load_texture("kite_logo", pixels, options))
}
