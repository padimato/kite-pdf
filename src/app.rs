use crate::annotations::{
    save_annotations_and_ocr_to_pdf, AnnotationKind, PdfAnnotation, ShapeKind, PALETTE_COLORS,
};
use crate::memory_manager::{
    query_process_memory_bytes, LruTextureCache, MultiplicativeHighlightQuad, PageTextureKey,
    DEFAULT_TEXTURE_BUDGET_BYTES,
};
use crate::ocr_engine::ensure_hybrid_word_jpg_pdf;
use crate::pdf_engine::{ensure_custom_sample_pdf, PdfEngine};
use crate::tabs::{
    ActiveTool, CommandPaletteState, DocumentTab, PaletteCommand, SidebarState, SIDEBAR_WIDTH,
};
use crate::text_search::{PdfTopDownRect, SearchBarState, SelectionGestureMode};
use crate::viewport::BASE_DPI_SCALE;
use egui::{
    Align2, Color32, Context, CornerRadius, FontId, Key, Margin, Order, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2, ViewportCommand,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    pub initial_pdf: PathBuf,
    pub extra_pdfs: Vec<PathBuf>,
    pub initial_zoom: Option<f32>,
    pub initial_page: Option<usize>,
    pub initial_scroll_x: Option<f32>,
    pub initial_scroll_y: Option<f32>,
    pub demo_tabs_count: Option<usize>,
    pub active_tab_index: Option<usize>,
    pub sidebar_mode: InitialSidebarMode,
    pub open_palette_query: Option<String>,
    pub initial_tool: Option<ActiveTool>,
    pub initial_search_query: Option<String>,
    pub initial_match_index: Option<usize>,
    pub initial_select_rect: Option<PdfTopDownRect>,
    pub initial_select_stream: Option<(usize, usize)>,
    pub initial_highlight_stream: Option<(usize, usize)>,
    pub demo_annotations: bool,
    pub hybrid_ocr_demo: bool,
    pub ocr_turbo: bool,
    pub save_annotated_output: Option<PathBuf>,
    pub screenshot_output: Option<PathBuf>,
}

pub struct SuperPdfApp {
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
    /// État du moteur de recherche `Ctrl+F`
    pub search_bar: SearchBarState,
    /// Gestionnaire de cache de textures GPU LRU à budget mémoire plafonné (48 Mo max)
    pub texture_cache: LruTextureCache,
    /// Consommation RAM réelle du processus Windows (Working Set en octets)
    pub process_working_set_bytes: usize,
    /// Indique si un glisser de déplacement d'annotation est en cours
    dragging_annotation_id: Option<u64>,
    last_drag_pt: Option<(f32, f32)>,
    /// Dernière erreur éventuelle à afficher
    error_message: Option<String>,
    /// Compteur de frames pour le mode capture d'écran automatisée
    frame_count: u64,
    /// Indique si la commande Screenshot a déjà été envoyée
    screenshot_requested: bool,
    /// Configuration d'automatisation QA
    automation: AutomationConfig,
    /// Mesure du temps de frame / FPS
    last_frame_instant: Instant,
    fps_smoothed: f32,
}

const TAB_BADGE_COLORS: [(Color32, (f32, f32, f32)); 6] = [
    (Color32::from_rgb(66, 135, 245), (0.10, 0.24, 0.48)),
    (Color32::from_rgb(46, 184, 138), (0.12, 0.42, 0.34)),
    (Color32::from_rgb(186, 85, 211), (0.42, 0.16, 0.38)),
    (Color32::from_rgb(235, 145, 55), (0.52, 0.28, 0.10)),
    (Color32::from_rgb(225, 85, 105), (0.48, 0.14, 0.20)),
    (Color32::from_rgb(80, 185, 225), (0.10, 0.36, 0.46)),
];

const DEMO_TAB_SPECS: [(&str, &str, usize); 15] = [
    ("01_Architecture_Rust_GPU.pdf", "ARCHITECTURE RUST & GPU", 6),
    ("02_Rapport_Graphiques_OCR.pdf", "RAPPORT GRAPHIQUES & OCR", 8),
    ("03_Norme_ISO_32000_Annotations.pdf", "NORME ISO 32000 ANNOTATIONS", 10),
    ("04_Analyse_Capteurs_ISEN.pdf", "ANALYSE CAPTEURS ISEN", 5),
    ("05_Benchmark_RAM_15_Onglets.pdf", "BENCHMARK RAM 15 ONGLETS", 7),
    ("06_Notes_Reunion_Projet.pdf", "NOTES REUNION PROJET", 4),
    ("07_Schema_Electronique_PCB.pdf", "SCHEMA ELECTRONIQUE PCB", 6),
    ("08_Publication_Traitement_Signal.pdf", "TRAITEMENT DU SIGNAL", 9),
    ("09_Datasheet_Microcontroleur.pdf", "DATASHEET STM32 / ARM", 12),
    ("10_Cahier_Charges_Systeme.pdf", "CAHIER DES CHARGES SYSTEME", 5),
    ("11_Mesures_Thermiques_Labo.pdf", "MESURES THERMIQUES LABO", 6),
    ("12_Protocole_Reseau_Temps_Reel.pdf", "PROTOCOLE TEMPS REEL", 8),
    ("13_Algorithmes_Vision_ONNX.pdf", "ALGORITHMES VISION ONNX", 7),
    ("14_Synthese_Budget_Materiel.pdf", "SYNTHESE BUDGET MATERIEL", 4),
    ("15_Manuel_Validation_Qualite.pdf", "MANUEL VALIDATION QUALITE", 6),
];

impl SuperPdfApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, automation: AutomationConfig) -> Self {
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

        let (ws_bytes, _) = query_process_memory_bytes();

        let mut app = Self {
            engine,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            sidebar,
            active_tool: automation.initial_tool.unwrap_or(ActiveTool::Select),
            command_palette: CommandPaletteState::default(),
            search_bar: SearchBarState::default(),
            texture_cache: LruTextureCache::new(DEFAULT_TEXTURE_BUDGET_BYTES),
            process_working_set_bytes: ws_bytes,
            dragging_annotation_id: None,
            last_drag_pt: None,
            error_message: None,
            frame_count: 0,
            screenshot_requested: false,
            automation: automation.clone(),
            last_frame_instant: Instant::now(),
            fps_smoothed: 60.0,
        };

        if automation.hybrid_ocr_demo {
            let hybrid_path = PathBuf::from("test_pdfs/hybrid_word_jpg_ocr.pdf");
            let _ = ensure_hybrid_word_jpg_pdf(&hybrid_path);
            app.open_pdf_in_new_tab(&hybrid_path);
            for extra in &automation.extra_pdfs {
                app.open_pdf_in_new_tab(extra);
            }
        } else if let Some(count) = automation.demo_tabs_count {
            for _ in 0..count.max(1) {
                app.spawn_next_demo_tab();
            }
        } else {
            app.open_pdf_in_new_tab(&automation.initial_pdf);
            for extra in &automation.extra_pdfs {
                app.open_pdf_in_new_tab(extra);
            }
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

        // Vitrine d'annotations ISO 32000 si demandée par CLI
        if automation.demo_annotations {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let target_page = automation.initial_page.unwrap_or(1).saturating_sub(1);
                tab.annotations.populate_demo_showcase(target_page);
                tab.is_modified = true;
            }
        }

        // Sauvegarde du PDF (Annotations ISO 32000 + Couche Texte OCR invisible `3 Tr`) si demandée par CLI
        if let Some(save_path) = &automation.save_annotated_output {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                let _ = save_annotations_and_ocr_to_pdf(
                    &tab.path,
                    save_path,
                    &tab.annotations.items,
                    &tab.text_layers,
                    &tab.page_dimensions,
                );
                tab.annotations.saved_feedback_frames = 120;
                tab.ocr.has_unsaved_ocr = false;
            }
        }

        let (ws_after, _) = query_process_memory_bytes();
        app.process_working_set_bytes = ws_after;

        app
    }

    pub fn switch_to_tab(&mut self, target_index: usize) {
        if target_index >= self.tabs.len() {
            return;
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != target_index {
                tab.hibernate(&mut self.texture_cache);
            }
        }
        if let Some(active) = self.tabs.get_mut(target_index) {
            if let Err(err) = active.ensure_awake(self.engine) {
                self.error_message = Some(format!("{:#}", err));
            } else {
                self.error_message = None;
            }
            if self.search_bar.is_open && !self.search_bar.query.trim().is_empty() {
                active.ensure_text_layers(self.engine);
                self.search_bar.execute_search(&active.text_layers);
            }
        }
        self.active_tab = target_index;
    }

    pub fn open_pdf_in_new_tab(&mut self, path: &Path) {
        let color_idx = self.tabs.len() % TAB_BADGE_COLORS.len();
        let (badge_color, _) = TAB_BADGE_COLORS[color_idx];

        for existing in &mut self.tabs {
            existing.hibernate(&mut self.texture_cache);
        }

        match self.engine.load_document(path) {
            Ok(doc) => {
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                self.tabs.push(DocumentTab::new_active(id, doc, badge_color));
                self.active_tab = self.tabs.len() - 1;
                self.error_message = None;
            }
            Err(err) => {
                self.error_message = Some(format!("{:#}", err));
            }
        }
    }

    pub fn spawn_next_demo_tab(&mut self) {
        let idx = self.tabs.len();
        let (filename, title, pages) = DEMO_TAB_SPECS[idx % DEMO_TAB_SPECS.len()];
        let (_, rgb) = TAB_BADGE_COLORS[idx % TAB_BADGE_COLORS.len()];
        let actual_filename = if idx < DEMO_TAB_SPECS.len() {
            filename.to_string()
        } else {
            format!("{:02}_{}", idx + 1, filename)
        };
        let path = PathBuf::from("test_pdfs").join(actual_filename);
        let _ = ensure_custom_sample_pdf(&path, title, rgb, pages);
        self.open_pdf_in_new_tab(&path);
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(index);
        self.texture_cache.evict_tab(removed.id);

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
        ((zoom * 20.0).round() as u32).max(1)
    }

    /// Enregistre les annotations ISO 32000 ET la couche de texte OCR invisible (`3 Tr` + `Tm`) dans le PDF (`Ctrl+S`)
    pub fn save_active_tab_annotations(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            // Relâcher temporairement le handle PDFium sur Windows pour écrire dans le fichier en place
            let tab_id = tab.id;
            tab.hibernate(&mut self.texture_cache);
            let res = save_annotations_and_ocr_to_pdf(
                &tab.path,
                &tab.path,
                &tab.annotations.items,
                &tab.text_layers,
                &tab.page_dimensions,
            );
            let _ = tab.ensure_awake(self.engine);
            self.texture_cache.evict_tab(tab_id);
            match res {
                Ok(()) => {
                    tab.is_modified = false;
                    tab.ocr.has_unsaved_ocr = false;
                    tab.annotations.saved_feedback_frames = 120;
                    self.error_message = None;
                }
                Err(e) => {
                    self.error_message = Some(format!("Erreur sauvegarde PDF : {:#}", e));
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
                self.sidebar.toggle_pinned();
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
            PaletteCommand::RunHybridOcrTurbo => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let p = tab.viewport.current_page_index(viewport_size, &tab.page_dimensions);
                    tab.run_turbo_ocr_for_search(p, self.engine);
                    if self.search_bar.is_open {
                        self.search_bar.execute_search(&tab.text_layers);
                    }
                }
            }
            PaletteCommand::OpenHybridWordJpgDemo => {
                let hybrid_path = PathBuf::from("test_pdfs/hybrid_word_jpg_ocr.pdf");
                let _ = ensure_hybrid_word_jpg_pdf(&hybrid_path);
                self.open_pdf_in_new_tab(&hybrid_path);
            }
            PaletteCommand::PopulateDemoAnnotations => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let p = tab.viewport.current_page_index(viewport_size, &tab.page_dimensions);
                    tab.annotations.populate_demo_showcase(p);
                    tab.is_modified = true;
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
            PaletteCommand::NewDemoTab => {
                self.spawn_next_demo_tab();
            }
            PaletteCommand::Open15TabsBenchmark => {
                while self.tabs.len() < 15 {
                    self.spawn_next_demo_tab();
                }
                self.switch_to_tab(self.active_tab);
            }
            PaletteCommand::HibernateInactiveTabs => {
                let current = self.active_tab;
                for (i, tab) in self.tabs.iter_mut().enumerate() {
                    if i != current {
                        tab.hibernate(&mut self.texture_cache);
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
        let text_input_focused = ctx.wants_keyboard_input();

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
                pending_commands.push(PaletteCommand::SaveAnnotatedPdf);
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

            if ctrl && (input.key_pressed(Key::K) || input.key_pressed(Key::P)) {
                self.command_palette.toggle();
            }
            if input.key_pressed(Key::Escape) {
                if self.command_palette.is_open {
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
                pending_commands.push(PaletteCommand::NewDemoTab);
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

            if self.command_palette.is_open || text_input_focused {
                return;
            }

            // 4. Raccourcis d'outils à 1 touche
            if !ctrl && !self.search_bar.is_open {
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
            let pointer_over_sidebar = self.sidebar.is_expanded()
                && pointer_pos.map(|p| p.x <= SIDEBAR_WIDTH).unwrap_or(false);

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
            let space_held = input.key_down(Key::Space) || self.active_tool == ActiveTool::Pan;
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
            if !ctrl && !self.search_bar.is_open {
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
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                let p = tab
                    .viewport
                    .current_page_index(viewport_size, &tab.page_dimensions);
                tab.ensure_text_layers(self.engine);
                tab.run_turbo_ocr_for_search(p, self.engine);
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
        if self.command_palette.is_open {
            return;
        }
        let pointer_over_sidebar = self.sidebar.is_expanded()
            && ctx
                .input(|i| i.pointer.hover_pos())
                .map(|p| p.x <= SIDEBAR_WIDTH)
                .unwrap_or(false);
        if pointer_over_sidebar {
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
        if matches!(self.active_tool, ActiveTool::Select | ActiveTool::Highlight) {
            tab.ensure_page_text_layer(page_index, self.engine);
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

        // --- CAS 1 : OUTIL SÉLECTION (V) OU SURLIGNER (H) ---
        if matches!(self.active_tool, ActiveTool::Select | ActiveTool::Highlight) {
            // Double-clic sur un mot : sélectionne le mot entier !
            if double_clicked {
                tab.ensure_page_text_layer(page_index, self.engine);
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
                // 1a. Vérifier d'abord si l'utilisateur clique sur une annotation existante pour la sélectionner / déplacer
                if self.active_tool == ActiveTool::Select && !alt_held {
                    if let Some(hit_id) = tab
                        .annotations
                        .items
                        .iter()
                        .rev()
                        .find(|a| a.page_index == page_index && a.hit_test(pt_x, pt_y))
                        .map(|a| a.id)
                    {
                        tab.annotations.selected_id = Some(hit_id);
                        tab.annotations.push_undo_snapshot();
                        self.dragging_annotation_id = Some(hit_id);
                        self.last_drag_pt = Some((pt_x, pt_y));
                        tab.text_selection.clear();
                        return;
                    }
                }

                // 1b. Sinon, démarrer une sélection de texte (Flux Curseur I par défaut, ou Rectangle si Alt maintenu)
                tab.annotations.selected_id = None;
                self.dragging_annotation_id = None;
                tab.ensure_page_text_layer(page_index, self.engine);
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
                // Déplacement en direct d'une annotation sélectionnée
                if let (Some(drag_id), Some((lx, ly))) =
                    (self.dragging_annotation_id, self.last_drag_pt)
                {
                    let dx = pt_x - lx;
                    let dy = pt_y - ly;
                    if dx.abs() > 0.2 || dy.abs() > 0.2 {
                        if let Some(annot) =
                            tab.annotations.items.iter_mut().find(|a| a.id == drag_id)
                        {
                            annot.translate(dx, dy);
                            tab.is_modified = true;
                        }
                        self.last_drag_pt = Some((pt_x, pt_y));
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
                self.dragging_annotation_id = None;
                self.last_drag_pt = None;

                // Si l'outil actif est Surligner (H) et qu'une sélection vient d'être faite, créer l'annotation /Highlight ISO 32000 !
                if self.active_tool == ActiveTool::Highlight && tab.text_selection.has_selection() {
                    let quads = tab.text_selection.highlight_boxes.clone();
                    if let Some(first) = quads.first() {
                        let union_rect = quads.iter().fold(*first, |acc, q| acc.union(q));
                        let color = tab.annotations.active_color_rgb;
                        tab.annotations.add_annotation(PdfAnnotation {
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

        // --- CAS 2 : OUTIL BLOC DE TEXTE (/FreeText - Touche T) OU NOTE (/Text - Touche N) ---
        if matches!(self.active_tool, ActiveTool::FreeText | ActiveTool::StickyNote) {
            if primary_pressed {
                let color = tab.annotations.active_color_rgb;
                if self.active_tool == ActiveTool::FreeText {
                    let rect = PdfTopDownRect {
                        x_min: pt_x,
                        y_min: pt_y,
                        x_max: (pt_x + 185.0).min(page_dim.width_pt - 10.0),
                        y_max: (pt_y + 48.0).min(page_dim.height_pt - 10.0),
                        angle_deg: 0.0,
                    };
                    tab.annotations.add_annotation(PdfAnnotation {
                        id: 0,
                        page_index,
                        rect,
                        color_rgb: color,
                        kind: AnnotationKind::FreeText {
                            text: "Nouveau bloc texte ISO 32000".to_string(),
                            font_size: 11.5,
                        },
                    });
                } else {
                    let rect = PdfTopDownRect {
                        x_min: pt_x,
                        y_min: pt_y,
                        x_max: (pt_x + 175.0).min(page_dim.width_pt - 10.0),
                        y_max: (pt_y + 46.0).min(page_dim.height_pt - 10.0),
                        angle_deg: 0.0,
                    };
                    tab.annotations.add_annotation(PdfAnnotation {
                        id: 0,
                        page_index,
                        rect,
                        color_rgb: color,
                        kind: AnnotationKind::StickyNote {
                            comment: "Nouvelle note PDF".to_string(),
                            is_popup_open: true,
                        },
                    });
                }
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
                if pts.len() >= 2 {
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
                                ShapeKind::ArrowLine
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
                    tab.annotations.add_annotation(PdfAnnotation {
                        id: 0,
                        page_index,
                        rect,
                        color_rgb: color,
                        kind,
                    });
                    tab.is_modified = true;
                }
            }
        }
    }
}

impl eframe::App for SuperPdfApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.frame_count += 1;
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_instant).as_secs_f32();
        self.last_frame_instant = now;
        if dt > 0.0001 {
            let instant_fps = (1.0 / dt).clamp(1.0, 240.0);
            self.fps_smoothed = self.fps_smoothed * 0.88 + instant_fps * 0.12;
        }

        if self.frame_count <= 3 || self.frame_count % 15 == 0 {
            let (ws_bytes, _) = query_process_memory_bytes();
            self.process_working_set_bytes = ws_bytes;
        }

        if self.sidebar.step_animation(dt.max(0.016)) {
            ctx.request_repaint();
        }

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(18, 20, 26);
        ctx.set_visuals(visuals);

        if self.sidebar.pinned {
            egui::SidePanel::left("superpdf_sidebar_pinned")
                .exact_width(SIDEBAR_WIDTH)
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(23, 26, 34))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 50, 66))),
                )
                .show(ctx, |ui| {
                    self.render_sidebar_contents(ui);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(18, 20, 26)))
            .show(ctx, |ui| {
                let canvas_rect = ui.max_rect();
                let _response = ui.allocate_rect(canvas_rect, Sense::click_and_drag());

                self.handle_inputs(ctx, canvas_rect);

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
                    // Phase 2 : Ordonnanceur OCR Mode Éco Tâche de fond au repos (1 page / tick sans bloquer les 60 FPS)
                    if tab.viewport.inertia_velocity.length_sq() < 0.1
                        && (self.frame_count == 2 || self.frame_count % 6 == 0)
                    {
                        let vis_p = tab
                            .viewport
                            .current_page_index(canvas_rect.size(), &tab.page_dimensions);
                        let added = tab.step_eco_ocr_background(vis_p, self.engine);
                        if added > 0 && self.search_bar.is_open && !self.search_bar.query.trim().is_empty() {
                            self.search_bar.execute_search(&tab.text_layers);
                        }
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

                for (p_idx, p_rect) in &visible_pages_info {
                    self.handle_page_canvas_interactions(ctx, *p_idx, *p_rect);
                }

                let painter = ui.painter_at(canvas_rect);
                let mut selection_toolbar_anchor: Option<(
                    Pos2,
                    String,
                    usize,
                    bool,
                    SelectionGestureMode,
                )> = None;

                if let Some(err) = &self.error_message {
                    painter.text(
                        canvas_rect.center(),
                        Align2::CENTER_CENTER,
                        format!("Erreur : {}", err),
                        FontId::proportional(16.0),
                        Color32::from_rgb(255, 110, 110),
                    );
                } else if let Some(tab) = self.tabs.get(self.active_tab) {
                    let viewport_size = canvas_rect.size();
                    let (_, _, total_h) = tab.viewport.compute_layout(&tab.page_dimensions);
                    let bucket = Self::zoom_bucket(tab.viewport.zoom);
                    let bucket_zoom = (bucket as f32) / 20.0;

                    if tab.viewport.inertia_velocity.length_sq() < 0.1 {
                        self.texture_cache.prune_stale_zoom_buckets(tab.id, bucket);
                    }

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
                            if let AnnotationKind::Highlight { quads } = &annot.kind {
                                for q in quads {
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
                        };

                        if !self.texture_cache.contains_key(&key) {
                            if let Some(doc) = &tab.document {
                                let target_w = (page_dim.width_pt * BASE_DPI_SCALE * bucket_zoom)
                                    .round() as i32;
                                let target_h = (page_dim.height_pt * BASE_DPI_SCALE * bucket_zoom)
                                    .round() as i32;

                                if let Ok(color_img) =
                                    doc.render_page(page_index, target_w, target_h)
                                {
                                    self.texture_cache.insert_color_image(
                                        ctx,
                                        key,
                                        color_img,
                                        &mult_highlights,
                                    );
                                }
                            }
                        }

                        if let Some(tex) =
                            self.texture_cache.get_with_highlights(&key, &mult_highlights)
                        {
                            painter.image(
                                tex.id(),
                                page_rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }

                        // --- 1. DESSIN DES ANNOTATIONS ISO 32000 SUR LA PAGE (JALON 5) ---
                        Self::paint_page_annotations(
                            &painter,
                            &tab.annotations.items,
                            tab.annotations.selected_id,
                            page_index,
                            page_rect,
                            page_dim,
                            tab.viewport.zoom,
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
                                        let ocr_prefix = if m.is_ocr { "OCR JPG " } else { "" };
                                        let label = if m.angle_deg.abs() > 1.0 {
                                            format!(
                                                "🔎 {}#{}/{} (Orienté {:+.0}°)",
                                                ocr_prefix,
                                                m_idx + 1,
                                                self.search_bar.matches.len(),
                                                m.angle_deg
                                            )
                                        } else if m.is_ocr {
                                            format!(
                                                "🔎 OCR JPG #{}/{} (0°)",
                                                m_idx + 1,
                                                self.search_bar.matches.len()
                                            )
                                        } else {
                                            format!(
                                                "🔎 Occurrence #{}/{}",
                                                m_idx + 1,
                                                self.search_bar.matches.len()
                                            )
                                        };
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

                            if tab.text_selection.has_selection() {
                                if let Some(last_b) = tab.text_selection.highlight_boxes.last() {
                                    let sb = last_b.to_screen_rect(page_rect, page_dim);
                                    selection_toolbar_anchor = Some((
                                        Pos2::new(
                                            sb.min.x.max(canvas_rect.min.x + 16.0),
                                            (sb.max.y + 8.0).min(canvas_rect.max.y - 78.0),
                                        ),
                                        tab.text_selection.selected_text.clone(),
                                        tab.text_selection.selected_text.chars().count(),
                                        tab.text_selection.copied_feedback_frames > 0,
                                        tab.text_selection.mode,
                                    ));
                                }
                            }
                        }

                        painter.rect_stroke(
                            page_rect,
                            CornerRadius::same(2),
                            Stroke::new(1.0_f32, Color32::from_rgb(55, 60, 72)),
                            StrokeKind::Outside,
                        );
                    }

                    // Mini barre de défilement verticale ultra-fine à droite (4 px)
                    if total_h > viewport_size.y {
                        let scrollbar_track_h = viewport_size.y - 24.0;
                        let thumb_ratio = (viewport_size.y / total_h).clamp(0.06, 1.0);
                        let thumb_h = scrollbar_track_h * thumb_ratio;
                        let scroll_progress = (tab.viewport.scroll_offset.y
                            / (total_h - viewport_size.y))
                            .clamp(0.0, 1.0);
                        let thumb_y = canvas_rect.min.y
                            + 12.0
                            + scroll_progress * (scrollbar_track_h - thumb_h);

                        let thumb_rect = Rect::from_min_size(
                            Pos2::new(canvas_rect.max.x - 8.0, thumb_y),
                            Vec2::new(4.0, thumb_h),
                        );
                        painter.rect_filled(
                            thumb_rect,
                            CornerRadius::same(2),
                            Color32::from_rgba_unmultiplied(150, 170, 210, 140),
                        );
                    }
                }

                // Barre flottante d'action sur le texte sélectionné (`📋 Copier Ctrl+C` + `✒ Surligner H`)
                if let Some((anchor_pos, sel_text, char_count, copied_ok, sel_mode)) =
                    selection_toolbar_anchor
                {
                    self.render_selection_floating_bar(
                        ctx,
                        anchor_pos,
                        &sel_text,
                        char_count,
                        copied_ok,
                        sel_mode,
                    );
                }

                // Barre de recherche flottante `Ctrl+F`
                if self.search_bar.is_open {
                    self.render_floating_search_bar(ctx, canvas_rect);
                }

                if !self.sidebar.pinned {
                    self.render_autohide_sidebar_overlay(ctx, canvas_rect);
                }

                self.render_status_pill(ui, canvas_rect);

                if self.command_palette.is_open {
                    self.render_command_palette(ctx, canvas_rect);
                }
            });

        if self.automation.screenshot_output.is_some() {
            if self.frame_count < 5 {
                ctx.request_repaint();
            } else if !self.screenshot_requested {
                self.screenshot_requested = true;
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
                ctx.request_repaint();
            }
        }
    }
}

impl SuperPdfApp {
    /// Dessine toutes les annotations ISO 32000 d'une page sur le canevas GPU
    fn paint_page_annotations(
        painter: &egui::Painter,
        annotations: &[PdfAnnotation],
        selected_id: Option<u64>,
        page_index: usize,
        page_rect: Rect,
        page_dim: crate::pdf_engine::PageDimensions,
        zoom: f32,
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
                AnnotationKind::FreeText { text, font_size } => {
                    // Carte de texte visible directement sur la page (style Adobe Acrobat FreeText)
                    painter.rect_filled(
                        screen_rect,
                        CornerRadius::same(4),
                        Color32::from_rgba_unmultiplied(255, 253, 242, 242),
                    );
                    painter.rect_stroke(
                        screen_rect,
                        CornerRadius::same(4),
                        Stroke::new(1.5_f32, stroke_col),
                        StrokeKind::Outside,
                    );
                    // Bandeau d'en-tête discret
                    let header_rect = Rect::from_min_size(
                        screen_rect.min,
                        Vec2::new(screen_rect.width(), (16.0 * zoom).clamp(13.0, 22.0)),
                    );
                    painter.rect_filled(header_rect, CornerRadius::same(3), stroke_col);
                    painter.text(
                        Pos2::new(header_rect.min.x + 6.0, header_rect.center().y),
                        Align2::LEFT_CENTER,
                        "🅰 /FreeText (ISO 32000)",
                        FontId::proportional((9.5 * zoom).clamp(8.5, 13.0)),
                        Color32::WHITE,
                    );

                    let scaled_font = (font_size * zoom * 1.15).clamp(9.0, 24.0);
                    painter.text(
                        Pos2::new(screen_rect.min.x + 7.0, header_rect.max.y + 4.0),
                        Align2::LEFT_TOP,
                        text,
                        FontId::proportional(scaled_font),
                        Color32::from_rgb(20, 24, 34),
                    );
                }
                AnnotationKind::Ink { strokes, thickness } => {
                    for stroke in strokes {
                        if stroke.len() >= 2 {
                            let pts: Vec<Pos2> =
                                stroke.iter().copied().map(to_screen_pos).collect();
                            painter.add(egui::Shape::line(
                                pts,
                                Stroke::new((*thickness * zoom).clamp(1.5, 8.0), stroke_col),
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
                    ShapeKind::ArrowLine => {
                        let p1 = to_screen_pos(*start_pt);
                        let p2 = to_screen_pos(*end_pt);
                        painter.line_segment([p1, p2], Stroke::new(2.2_f32, stroke_col));

                        // Pointe de flèche vectorielle (/ClosedArrow)
                        let dir = (p2 - p1).normalized();
                        if dir.length_sq() > 0.1 {
                            let perp = Vec2::new(-dir.y, dir.x);
                            let tip_len = (11.0 * zoom).clamp(8.0, 18.0);
                            let tip_w = (5.5 * zoom).clamp(4.0, 9.0);
                            let base = p2 - dir * tip_len;
                            painter.add(egui::Shape::convex_polygon(
                                vec![p2, base + perp * tip_w, base - perp * tip_w],
                                stroke_col,
                                Stroke::NONE,
                            ));
                        }
                    }
                },
                AnnotationKind::StickyNote {
                    comment,
                    is_popup_open,
                } => {
                    painter.rect_filled(
                        screen_rect,
                        CornerRadius::same(6),
                        Color32::from_rgba_unmultiplied(255, 246, 205, 245),
                    );
                    painter.rect_stroke(
                        screen_rect,
                        CornerRadius::same(6),
                        Stroke::new(1.5_f32, stroke_col),
                        StrokeKind::Outside,
                    );
                    let header_h = (17.0 * zoom).clamp(14.0, 22.0);
                    let header_r =
                        Rect::from_min_size(screen_rect.min, Vec2::new(screen_rect.width(), header_h));
                    painter.rect_filled(header_r, CornerRadius::same(4), stroke_col);
                    painter.text(
                        Pos2::new(header_r.min.x + 6.0, header_r.center().y),
                        Align2::LEFT_CENTER,
                        "💬 Note /Text",
                        FontId::proportional((9.5 * zoom).clamp(8.5, 13.0)),
                        Color32::WHITE,
                    );
                    if *is_popup_open {
                        painter.text(
                            Pos2::new(screen_rect.min.x + 6.0, header_r.max.y + 4.0),
                            Align2::LEFT_TOP,
                            comment,
                            FontId::proportional((10.5 * zoom).clamp(9.0, 16.0)),
                            Color32::from_rgb(35, 30, 20),
                        );
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

            // Cadre de sélection et poignées si cette annotation est sélectionnée
            if is_selected {
                let sel_box = screen_rect.expand(4.0);
                painter.rect_stroke(
                    sel_box,
                    CornerRadius::same(4),
                    Stroke::new(1.8_f32, Color32::from_rgb(90, 175, 255)),
                    StrokeKind::Outside,
                );
                let corners = [
                    sel_box.left_top(),
                    sel_box.right_top(),
                    sel_box.left_bottom(),
                    sel_box.right_bottom(),
                ];
                for c in corners {
                    painter.rect_filled(
                        Rect::from_center_size(c, Vec2::splat(7.0)),
                        CornerRadius::same(1),
                        Color32::from_rgb(90, 175, 255),
                    );
                }
            }
        }
    }

    /// Dessine la barre flottante de copie (`Ctrl+C`) et de surlignage direct (`✒ Surligner H`)
    fn render_selection_floating_bar(
        &mut self,
        ctx: &Context,
        pos: Pos2,
        sel_text: &str,
        char_count: usize,
        copied_ok: bool,
        sel_mode: SelectionGestureMode,
    ) {
        let preview: String = if sel_text.chars().count() > 34 {
            let s: String = sel_text.chars().take(32).collect();
            format!("\"{}...\"", s)
        } else {
            format!("\"{}\"", sel_text)
        };
        let mode_badge = match sel_mode {
            SelectionGestureMode::StreamIBeam => "Curseur I (Flux)",
            SelectionGestureMode::RectangularBlock => "Bloc (Alt+Glisser)",
        };

        let mut create_highlight = false;

        egui::Area::new(egui::Id::new("superpdf_text_selection_bar"))
            .fixed_pos(pos)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(20, 26, 40, 248))
                    .stroke(Stroke::new(1.5_f32, Color32::from_rgb(75, 145, 255)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let btn_label = if copied_ok {
                                "✅ Copié !"
                            } else {
                                "📋 Copier (Ctrl+C)"
                            };
                            if ui.small_button(btn_label).clicked() {
                                ctx.copy_text(sel_text.to_string());
                                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                    tab.text_selection.copied_feedback_frames = 90;
                                }
                            }
                            if ui
                                .small_button("✒ Surligner (H)")
                                .on_hover_text(
                                    "Convertir la sélection en annotation /Highlight ISO 32000",
                                )
                                .clicked()
                            {
                                create_highlight = true;
                            }
                            ui.separator();
                            ui.colored_label(
                                Color32::from_rgb(135, 195, 255),
                                format!("[{}]", mode_badge),
                            );
                            ui.label(
                                egui::RichText::new(format!("{} car. : {}", char_count, preview))
                                    .size(11.5)
                                    .color(Color32::from_rgb(215, 228, 255)),
                            );
                            if ui.small_button("✖").clicked() {
                                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                    tab.text_selection.clear();
                                }
                            }
                        });
                    });
            });

        if create_highlight {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if let Some(page_index) = tab.text_selection.page_index {
                    let quads = tab.text_selection.highlight_boxes.clone();
                    if let Some(first) = quads.first() {
                        let union_rect = quads.iter().fold(*first, |acc, q| acc.union(q));
                        let color = tab.annotations.active_color_rgb;
                        tab.annotations.add_annotation(PdfAnnotation {
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
        }
    }

    fn render_floating_search_bar(&mut self, ctx: &Context, canvas_rect: Rect) {
        let bar_w = 430.0;
        let bar_pos = Pos2::new(
            (canvas_rect.max.x - bar_w - 20.0).max(canvas_rect.min.x + 12.0),
            canvas_rect.min.y + 14.0,
        );

        let mut run_search = false;
        let mut step_dir: i32 = 0;
        let mut close_search = false;

        egui::Area::new(egui::Id::new("superpdf_floating_ctrl_f"))
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
                                .on_hover_text("Occurrence précédente (Shift+Entrée)")
                                .clicked()
                            {
                                step_dir = -1;
                            }
                            if ui
                                .add_enabled(total > 0, egui::Button::new("▶").small())
                                .on_hover_text("Occurrence suivante (Entrée)")
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
                    });
            });

        ctx.input(|input| {
            if input.key_pressed(Key::Enter) && !self.command_palette.is_open {
                step_dir = if input.modifiers.shift { -1 } else { 1 };
            }
        });

        if run_search {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                let vis_p = tab
                    .viewport
                    .current_page_index(canvas_rect.size(), &tab.page_dimensions);
                tab.ensure_text_layers(self.engine);
                // Phase 3 : Mode Turbo OCR déclenché automatiquement lors d'une recherche `Ctrl+F`
                tab.run_turbo_ocr_for_search(vis_p, self.engine);
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

        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);

        // 1. En-tête de la barre latérale
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("⚡ SUPER PDF")
                            .strong()
                            .size(14.0)
                            .color(Color32::from_rgb(130, 185, 255)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pin_label = if self.sidebar.pinned {
                            "📌 Épinglée"
                        } else {
                            "👁 Survol"
                        };
                        if ui
                            .small_button(pin_label)
                            .on_hover_text("Basculer Épinglée / Auto-Hide plein écran (Ctrl+B)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::ToggleSidebarPin);
                        }

                        if ui
                            .small_button("🔎")
                            .on_hover_text("Recherche dans le PDF (Ctrl+F)")
                            .clicked()
                        {
                            self.search_bar.open();
                            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                tab.ensure_text_layers(self.engine);
                            }
                        }

                        if ui
                            .small_button("⌘K")
                            .on_hover_text("Palette de commandes rapide (Ctrl+K)")
                            .clicked()
                        {
                            self.command_palette.toggle();
                        }
                    });
                });

                ui.add_space(3.0);

                // Barre de déclenchement rapide Ctrl+K
                let search_btn = egui::Button::new(
                    egui::RichText::new("🔍 Palette : onglet, outil, page... (Ctrl+K)")
                        .size(11.5)
                        .color(Color32::from_rgb(175, 185, 208)),
                )
                .fill(Color32::from_rgb(33, 38, 50))
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(SIDEBAR_WIDTH - 26.0, 25.0));

                if ui.add(search_btn).clicked() {
                    self.command_palette.open();
                }

                ui.add_space(5.0);

                // 2. Grille d'outils compacts (2 lignes x 4 colonnes) + Actions Annotations ISO 32000
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("OUTILS ISO 32000")
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

                egui::Grid::new("superpdf_tools_grid")
                    .num_columns(4)
                    .spacing([5.0, 4.0])
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
                            .min_size(Vec2::new(55.0, 33.0));

                            if ui
                                .add(btn)
                                .on_hover_text(format!("{} (Raccourci : {})", label, shortcut))
                                .clicked()
                            {
                                self.active_tool = *tool;
                            }
                            if (i + 1) % 4 == 0 {
                                ui.end_row();
                            }
                        }
                    });

                ui.add_space(4.0);

                // 3. En-tête de la liste verticale des onglets
                let hibernated_count = self.tabs.iter().filter(|t| t.is_hibernated()).count();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "ONGLETS ({} | {} 💤)",
                            self.tabs.len(),
                            hibernated_count
                        ))
                        .size(10.5)
                        .strong()
                        .color(Color32::from_rgb(120, 132, 158)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("⚡ 15 PDF")
                            .on_hover_text("Ouvrir 15 PDF en simultané avec hibernation automatique")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::Open15TabsBenchmark);
                        }
                        if ui
                            .small_button("➕")
                            .on_hover_text("Ouvrir un nouvel onglet de démonstration (Ctrl+T)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::NewDemoTab);
                        }
                        if ui
                            .small_button("📂")
                            .on_hover_text("Ouvrir un fichier PDF (Ctrl+O)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::OpenFileDialog);
                        }
                    });
                });
            });

        ui.separator();

        let has_search_drawer = self.search_bar.is_open && !self.search_bar.matches.is_empty();
        let has_annot_inspector = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.annotations.selected())
            .is_some();
        let tabs_reserved_bottom = if has_search_drawer || has_annot_inspector {
            285.0
        } else {
            155.0
        };
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
                    let is_hib = tab.is_hibernated();
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
                                let badge_text = format!("#{}", idx + 1);
                                ui.colored_label(tab.badge_color, badge_text);

                                let current_p = tab.viewport.current_page_index(
                                    Vec2::new(900.0, 800.0),
                                    &tab.page_dimensions,
                                ) + 1;
                                let total_p = tab.page_count();

                                let short_title = if tab.title.chars().count() > 21 {
                                    let s: String = tab.title.chars().take(19).collect();
                                    format!("{}...", s)
                                } else {
                                    tab.title.clone()
                                };
                                let mod_star = if tab.is_modified { " *" } else { "" };
                                let ram_state = if is_hib { "💤 0 Mo" } else { "⚡ Actif" };

                                let title_btn = egui::Button::new(
                                    egui::RichText::new(format!(
                                        "{}{}\np.{}/{} • {:.0}% • {}",
                                        short_title,
                                        mod_star,
                                        current_p,
                                        total_p,
                                        tab.viewport.zoom * 100.0,
                                        ram_state
                                    ))
                                    .size(11.0)
                                    .color(if is_active {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_rgb(185, 194, 212)
                                    }),
                                )
                                .frame(false);

                                if ui
                                    .add_sized([154.0, 28.0], title_btn)
                                    .on_hover_text(&tab.title)
                                    .clicked()
                                {
                                    tab_to_select = Some(idx);
                                }

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
                                    },
                                );
                            });
                        });
                }
            });

        // Tiroir Inspecteur d'Annotation ISO 32000 (édition texte FreeText/Note, couleur, suppression)
        if has_annot_inspector {
            ui.separator();
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                let mut delete_clicked = false;
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

                            // Édition directe du contenu texte pour FreeText et StickyNote
                            match &mut annot.kind {
                                AnnotationKind::FreeText { text, .. } => {
                                    ui.add(
                                        egui::TextEdit::multiline(text)
                                            .desired_rows(2)
                                            .desired_width(SIDEBAR_WIDTH - 24.0),
                                    );
                                }
                                AnnotationKind::StickyNote { comment, .. } => {
                                    ui.add(
                                        egui::TextEdit::multiline(comment)
                                            .desired_rows(2)
                                            .desired_width(SIDEBAR_WIDTH - 24.0),
                                    );
                                }
                                _ => {}
                            }
                        }
                    });
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
                        .max_height(105.0)
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
                                        let ocr_tag = if m.is_ocr { " [OCR JPG]" } else { "" };
                                        let angle_tag = if m.angle_deg.abs() > 1.0 {
                                            format!(" [↗{:+.0}°]", m.angle_deg)
                                        } else {
                                            String::new()
                                        };
                                        let btn = egui::Button::new(
                                            egui::RichText::new(format!(
                                                "P.{}{}{} : {}",
                                                m.page_index + 1,
                                                ocr_tag,
                                                angle_tag,
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
        }

        ui.separator();

        // 5. Jauge Mémoire Temps Réel + Contrôles Page/Zoom
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(10, 5))
            .show(ui, |ui| {
                let ram_mb = self.process_working_set_bytes as f32 / (1024.0 * 1024.0);
                let cache_mb = self.texture_cache.allocated_bytes as f32 / (1024.0 * 1024.0);
                let budget_mb = self.texture_cache.max_budget_bytes as f32 / (1024.0 * 1024.0);
                let cache_ratio = (cache_mb / budget_mb).clamp(0.0, 1.0);

                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("⚡ RAM Processus : {:.1} Mo", ram_mb))
                            .size(11.0)
                            .strong()
                            .color(Color32::from_rgb(125, 225, 165)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("💤 Purger")
                            .on_hover_text(
                                "Vider le cache de textures et hiberner les onglets inactifs",
                            )
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::HibernateInactiveTabs);
                        }
                    });
                });

                ui.label(
                    egui::RichText::new(format!(
                        "Cache LRU : {:.1}/{:.0} Mo ({} p. | {} évic.)",
                        cache_mb,
                        budget_mb,
                        self.texture_cache.len(),
                        self.texture_cache.total_evictions
                    ))
                    .size(10.5)
                    .color(Color32::from_rgb(160, 175, 200)),
                );

                if let Some(tab) = self.tabs.get(self.active_tab) {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "👁 {} ({})",
                                tab.ocr.status_summary,
                                tab.ocr.mode.badge_label()
                            ))
                            .size(10.0)
                            .color(Color32::from_rgb(245, 175, 220)),
                        );
                    });
                }

                let bar_w = SIDEBAR_WIDTH - 24.0;
                let (bar_rect, _) =
                    ui.allocate_exact_size(Vec2::new(bar_w, 5.0), Sense::hover());
                ui.painter().rect_filled(
                    bar_rect,
                    CornerRadius::same(2),
                    Color32::from_rgb(35, 40, 54),
                );
                let fill_w = (bar_w * cache_ratio).max(3.0);
                ui.painter().rect_filled(
                    Rect::from_min_size(bar_rect.min, Vec2::new(fill_w, 5.0)),
                    CornerRadius::same(2),
                    Color32::from_rgb(72, 165, 245),
                );

                ui.add_space(3.0);

                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let pages = tab.page_dimensions.clone();
                    let page_count = pages.len();
                    let est_vp = Vec2::new(1000.0, 800.0);
                    let current_page = tab.viewport.current_page_index(est_vp, &pages) + 1;

                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(current_page > 1, egui::Button::new("◀").small())
                            .clicked()
                        {
                            tab.viewport
                                .jump_to_page(current_page - 2, est_vp, &pages);
                        }
                        ui.label(
                            egui::RichText::new(format!("Page {} / {}", current_page, page_count))
                                .strong()
                                .size(11.5)
                                .color(Color32::WHITE),
                        );
                        if ui
                            .add_enabled(
                                current_page < page_count,
                                egui::Button::new("▶").small(),
                            )
                            .clicked()
                        {
                            tab.viewport.jump_to_page(current_page, est_vp, &pages);
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("➕").clicked() {
                                tab.viewport.apply_zoom_around_anchor(
                                    1.2,
                                    est_vp * 0.5,
                                    est_vp,
                                    &pages,
                                );
                            }
                            ui.label(
                                egui::RichText::new(format!("{:.0}%", tab.viewport.zoom * 100.0))
                                    .monospace()
                                    .size(11.0)
                                    .color(Color32::from_rgb(130, 220, 170)),
                            );
                            if ui.small_button("➖").clicked() {
                                tab.viewport.apply_zoom_around_anchor(
                                    1.0 / 1.2,
                                    est_vp * 0.5,
                                    est_vp,
                                    &pages,
                                );
                            }
                        });
                    });

                    ui.horizontal(|ui| {
                        if ui.small_button("↔ Largeur (Ctrl+9)").clicked() {
                            pending_cmd = Some(PaletteCommand::FitToWidth);
                        }
                        if ui.small_button("↕ Page (Ctrl+0)").clicked() {
                            pending_cmd = Some(PaletteCommand::FitToPage);
                        }
                    });
                }
            });

        if let Some(idx) = tab_to_select {
            self.switch_to_tab(idx);
        }
        if let Some(idx) = tab_to_close {
            self.close_tab(idx);
        }
        if let Some(m_idx) = match_to_select {
            self.search_bar.current_match_index = m_idx;
            self.scroll_to_current_search_match(Vec2::new(1000.0, 800.0));
        }
        if let Some(cmd) = pending_cmd {
            self.execute_palette_command(cmd, Vec2::new(1000.0, 800.0));
        }
    }

    fn render_autohide_sidebar_overlay(&mut self, ctx: &Context, canvas_rect: Rect) {
        let painter = ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("superpdf_edge_indicator"),
        ));

        let edge_strip = Rect::from_min_size(canvas_rect.min, Vec2::new(4.0, canvas_rect.height()));
        painter.rect_filled(
            edge_strip,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(80, 145, 255, 110),
        );

        if self.sidebar.anim_progress < 0.05 {
            egui::Area::new(egui::Id::new("superpdf_collapsed_trigger_pill"))
                .fixed_pos(Pos2::new(10.0, 12.0))
                .order(Order::Foreground)
                .show(ctx, |ui| {
                    let btn = egui::Button::new(
                        egui::RichText::new("▶ Barre latérale (Survol / Ctrl+B)")
                            .size(11.0)
                            .color(Color32::from_rgb(190, 210, 250)),
                    )
                    .fill(Color32::from_rgba_unmultiplied(24, 28, 40, 220))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(65, 95, 160)))
                    .corner_radius(CornerRadius::same(14));

                    if ui.add(btn).clicked() {
                        self.sidebar.pinned = true;
                    }
                });
            return;
        }

        let x_offset = -SIDEBAR_WIDTH * (1.0 - self.sidebar.anim_progress);
        let overlay_rect = Rect::from_min_size(
            Pos2::new(x_offset, 8.0),
            Vec2::new(SIDEBAR_WIDTH, canvas_rect.height() - 16.0),
        );

        egui::Area::new(egui::Id::new("superpdf_floating_sidebar_overlay"))
            .fixed_pos(overlay_rect.min)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(22, 25, 34, 248))
                    .stroke(Stroke::new(1.5_f32, Color32::from_rgb(80, 130, 230)))
                    .corner_radius(CornerRadius::same(12))
                    .show(ui, |ui| {
                        ui.set_width(SIDEBAR_WIDTH);
                        ui.set_height(overlay_rect.height());
                        self.render_sidebar_contents(ui);
                    });
            });
    }

    fn render_status_pill(&mut self, ui: &mut egui::Ui, canvas_rect: Rect) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let pages = &tab.page_dimensions;
        let viewport_size = canvas_rect.size();
        let current_page = tab.viewport.current_page_index(viewport_size, pages) + 1;
        let page_count = pages.len();
        let (tool_icon, tool_name, tool_key) = self.active_tool.icon_and_label();
        let hibernated_count = self.tabs.iter().filter(|t| t.is_hibernated()).count();
        let ram_mb = self.process_working_set_bytes as f32 / (1024.0 * 1024.0);
        let annot_count = tab.annotations.items.len();

        let pill_pos = Pos2::new(
            (canvas_rect.max.x - 690.0).max(canvas_rect.min.x + 12.0),
            canvas_rect.max.y - 40.0,
        );

        egui::Area::new(egui::Id::new("status_pill_jalon5"))
            .fixed_pos(pill_pos)
            .order(Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(22, 26, 36, 235))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(56, 66, 88)))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::symmetric(12, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            ui.colored_label(
                                Color32::from_rgb(130, 185, 255),
                                format!("{} {} ({})", tool_icon, tool_name, tool_key),
                            );
                            ui.separator();
                            let saved_tag = if tab.annotations.saved_feedback_frames > 0 {
                                "✅ ISO 32000 + Tr3 Enregistré"
                            } else if tab.ocr.recognized_ocr_chars_count > 0 {
                                "⚡ OCR JPG Actif"
                            } else {
                                ""
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "Onglet {}/{} ({} 💤) • P.{}/{} • {} Annot. {}",
                                    self.active_tab + 1,
                                    self.tabs.len(),
                                    hibernated_count,
                                    current_page,
                                    page_count,
                                    annot_count,
                                    saved_tag
                                ))
                                .strong()
                                .size(11.5)
                                .color(Color32::WHITE),
                            );
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "RAM {:.1} Mo | ⚡ {:.0} FPS",
                                    ram_mb, self.fps_smoothed
                                ))
                                .size(11.0)
                                .color(Color32::from_rgb(130, 225, 175)),
                            );
                        });
                    });
            });
    }

    fn render_command_palette(&mut self, ctx: &Context, canvas_rect: Rect) {
        let tabs_info: Vec<(usize, String, usize, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.title.clone(), t.page_count(), t.is_hibernated()))
            .collect();
        let active_pages = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.page_count())
            .unwrap_or(0);

        let entries = self
            .command_palette
            .filtered_entries(&tabs_info, active_pages);
        if self.command_palette.selected_index >= entries.len() {
            self.command_palette.selected_index = entries.len().saturating_sub(1);
        }

        let mut command_to_run: Option<PaletteCommand> = None;
        let mut should_close = false;

        ctx.input(|input| {
            if input.key_pressed(Key::ArrowDown) && !entries.is_empty() {
                self.command_palette.selected_index =
                    (self.command_palette.selected_index + 1) % entries.len();
            }
            if input.key_pressed(Key::ArrowUp) && !entries.is_empty() {
                self.command_palette.selected_index =
                    (self.command_palette.selected_index + entries.len() - 1) % entries.len();
            }
            if input.key_pressed(Key::Enter) {
                if let Some(chosen) = entries.get(self.command_palette.selected_index) {
                    command_to_run = Some(chosen.command.clone());
                    should_close = true;
                }
            }
        });

        let backdrop_painter = ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("palette_backdrop"),
        ));
        backdrop_painter.rect_filled(
            ctx.screen_rect(),
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(8, 10, 15, 150),
        );

        let palette_w = 560.0;
        let palette_pos = Pos2::new(
            ctx.screen_rect().center().x - palette_w * 0.5,
            (ctx.screen_rect().min.y + 85.0).max(24.0),
        );

        egui::Area::new(egui::Id::new("superpdf_command_palette_modal"))
            .fixed_pos(palette_pos)
            .order(Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgb(26, 30, 42))
                    .stroke(Stroke::new(1.5_f32, Color32::from_rgb(88, 138, 235)))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.set_width(palette_w);

                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("⌘ PALETTE DE COMMANDES SUPER PDF")
                                    .strong()
                                    .size(12.0)
                                    .color(Color32::from_rgb(130, 180, 255)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Échap ✖").clicked() {
                                        should_close = true;
                                    }
                                },
                            );
                        });

                        ui.add_space(6.0);

                        let text_edit = egui::TextEdit::singleline(&mut self.command_palette.query)
                            .hint_text("Tapez le nom d'un onglet, d'un outil (Surligner, Texte...) ou d'une page...")
                            .desired_width(palette_w - 8.0);
                        let resp = ui.add(text_edit);
                        if self.command_palette.focus_requested {
                            resp.request_focus();
                            self.command_palette.focus_requested = false;
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(4.0);

                        egui::ScrollArea::vertical()
                            .max_height(290.0)
                            .show(ui, |ui| {
                                if entries.is_empty() {
                                    ui.label(
                                        egui::RichText::new("Aucune commande correspondante.")
                                            .color(Color32::from_rgb(140, 150, 170)),
                                    );
                                }
                                for (idx, entry) in entries.iter().enumerate() {
                                    let is_selected = idx == self.command_palette.selected_index;
                                    let row_fill = if is_selected {
                                        Color32::from_rgb(50, 92, 185)
                                    } else {
                                        Color32::from_rgb(32, 37, 50)
                                    };

                                    egui::Frame::NONE
                                        .fill(row_fill)
                                        .corner_radius(CornerRadius::same(7))
                                        .inner_margin(Margin::symmetric(10, 6))
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.colored_label(
                                                    Color32::from_rgb(150, 200, 255),
                                                    format!("[{}]", entry.category),
                                                );
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new(&entry.label)
                                                                .color(Color32::WHITE)
                                                                .size(12.5),
                                                        )
                                                        .frame(false),
                                                    )
                                                    .clicked()
                                                {
                                                    command_to_run = Some(entry.command.clone());
                                                    should_close = true;
                                                }
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(entry.shortcut)
                                                                .monospace()
                                                                .size(11.0)
                                                                .color(Color32::from_rgb(
                                                                    180, 220, 180,
                                                                )),
                                                        );
                                                    },
                                                );
                                            });
                                        });
                                    ui.add_space(3.0);
                                }
                            });
                    });
            });

        if should_close {
            self.command_palette.close();
        }
        if let Some(cmd) = command_to_run {
            self.execute_palette_command(cmd, canvas_rect.size());
        }
    }
}
