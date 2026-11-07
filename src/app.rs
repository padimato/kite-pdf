use crate::memory_manager::{
    query_process_memory_bytes, LruTextureCache, PageTextureKey, DEFAULT_TEXTURE_BUDGET_BYTES,
};
use crate::pdf_engine::{ensure_custom_sample_pdf, PdfEngine};
use crate::tabs::{
    ActiveTool, CommandPaletteState, DocumentTab, PaletteCommand, SidebarState, SIDEBAR_WIDTH,
};
use crate::text_search::{PdfTopDownRect, SearchBarState};
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
            error_message: None,
            frame_count: 0,
            screenshot_requested: false,
            automation: automation.clone(),
            last_frame_instant: Instant::now(),
            fps_smoothed: 60.0,
        };

        if let Some(count) = automation.demo_tabs_count {
            for _ in 0..count.max(1) {
                app.spawn_next_demo_tab();
            }
        } else {
            app.open_pdf_in_new_tab(&automation.initial_pdf);
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

        // Recherche initiale Ctrl+F si demandée par CLI
        if let Some(sq) = &automation.initial_search_query {
            app.search_bar.open();
            app.search_bar.query = sq.clone();
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                tab.ensure_text_layers(app.engine);
                app.search_bar.execute_search(&tab.text_layers);
                if let Some(m_idx) = automation.initial_match_index {
                    if !app.search_bar.matches.is_empty() {
                        app.search_bar.current_match_index =
                            m_idx.saturating_sub(1).min(app.search_bar.matches.len() - 1);
                    }
                }
            }
        }

        // Sélection de texte initiale si demandée par CLI
        if let Some(sel_rect) = automation.initial_select_rect {
            if let Some(tab) = app.tabs.get_mut(app.active_tab) {
                tab.ensure_text_layers(app.engine);
                let target_page = automation.initial_page.unwrap_or(1).saturating_sub(1);
                if let Some(layer) = tab.text_layers.get(target_page) {
                    let (txt, boxes) = layer.select_in_rect(&sel_rect);
                    tab.text_selection.page_index = Some(target_page);
                    tab.text_selection.drag_start_pt = Some((sel_rect.x_min, sel_rect.y_min));
                    tab.text_selection.drag_current_pt = Some((sel_rect.x_max, sel_rect.y_max));
                    tab.text_selection.selected_text = txt;
                    tab.text_selection.highlight_boxes = boxes;
                }
            }
        }

        let (ws_after, _) = query_process_memory_bytes();
        app.process_working_set_bytes = ws_after;

        app
    }

    /// Bascule vers l'onglet `target_index`, réveille son document PDFium et met en hibernation les autres onglets.
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

    /// Ouvre un fichier PDF dans un nouvel onglet, hiberne les précédents et bascule dessus
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

    /// Centre la vue sur l'occurrence de recherche courante (`current_match_index`)
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

            // 3. Raccourcis globaux (Ctrl+F Recherche, Ctrl+C Copie, Ctrl+K Palette, Ctrl+B Sidebar)
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

            if self.command_palette.is_open {
                return;
            }

            // 4. Raccourcis d'outils à 1 touche (si aucun champ texte n'a le focus)
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

            // Zoom clavier Ctrl++ / Ctrl+-
            if ctrl && (input.key_pressed(Key::Plus) || input.key_pressed(Key::Equals)) {
                tab.viewport.apply_zoom_around_anchor(
                    1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
                tab.viewport.last_gesture_label =
                    format!("Zoom + ({:.0}%)", tab.viewport.zoom * 100.0);
            }
            if ctrl && input.key_pressed(Key::Minus) {
                tab.viewport.apply_zoom_around_anchor(
                    1.0 / 1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
                tab.viewport.last_gesture_label =
                    format!("Zoom - ({:.0}%)", tab.viewport.zoom * 100.0);
            }

            // Défilement clavier (hors barre de recherche ouverte)
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
                tab.ensure_text_layers(self.engine);
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

    /// Gère la sélection de texte par cliquer-glisser (Trackpad / Souris) sur une page visible lorsque `ActiveTool::Select` est actif
    fn handle_page_text_selection_drag(
        &mut self,
        ctx: &Context,
        page_index: usize,
        page_rect: Rect,
    ) {
        if self.active_tool != ActiveTool::Select || self.command_palette.is_open {
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

        let (press_pos, hover_pos, primary_pressed, primary_down, space_down) = ctx.input(|i| {
            (
                i.pointer.press_origin(),
                i.pointer.hover_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.key_down(Key::Space),
            )
        });

        if space_down {
            return;
        }

        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let page_dim = tab.page_dimensions[page_index];

        // Début du glisser sur cette page
        if primary_pressed {
            if let Some(origin) = press_pos {
                // Éviter d'intercepter les clics sur la barre de recherche en haut à droite
                if page_rect.contains(origin) && origin.y > 56.0 {
                    tab.ensure_text_layers(self.engine);
                    let pt_x = ((origin.x - page_rect.min.x) / page_rect.width().max(1.0))
                        * page_dim.width_pt;
                    let pt_y = ((origin.y - page_rect.min.y) / page_rect.height().max(1.0))
                        * page_dim.height_pt;
                    tab.text_selection.page_index = Some(page_index);
                    tab.text_selection.drag_start_pt = Some((pt_x, pt_y));
                    tab.text_selection.drag_current_pt = Some((pt_x, pt_y));
                    tab.text_selection.selected_text.clear();
                    tab.text_selection.highlight_boxes.clear();
                }
            }
        }

        // Mise à jour en temps réel pendant le glissement sur cette page
        if primary_down && tab.text_selection.page_index == Some(page_index) {
            if let Some(curr) = hover_pos {
                let pt_x = (((curr.x - page_rect.min.x) / page_rect.width().max(1.0))
                    * page_dim.width_pt)
                    .clamp(0.0, page_dim.width_pt);
                let pt_y = (((curr.y - page_rect.min.y) / page_rect.height().max(1.0))
                    * page_dim.height_pt)
                    .clamp(0.0, page_dim.height_pt);
                tab.text_selection.drag_current_pt = Some((pt_x, pt_y));

                if let Some(sel_rect) = tab.text_selection.active_drag_rect() {
                    if sel_rect.width() > 3.0 || sel_rect.height() > 3.0 {
                        if let Some(layer) = tab.text_layers.get(page_index) {
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

        // Si la barre latérale est épinglée, elle occupe un SidePanel gauche fixe
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

        // Zone principale du document PDF (100% de la hauteur d'écran !)
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
                    if tab.viewport.is_panning {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    } else if self.active_tool == ActiveTool::Pan {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                }

                // Collecter les géométries des pages visibles pour gérer la sélection et le dessin
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
                    self.handle_page_text_selection_drag(ctx, *p_idx, *p_rect);
                }

                let painter = ui.painter_at(canvas_rect);
                let mut selection_toolbar_anchor: Option<(Pos2, String, usize, bool)> = None;

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

                        let key = PageTextureKey {
                            tab_id: tab.id,
                            page_index,
                            zoom_bucket: bucket,
                        };

                        if !self.texture_cache.contains_key(&key) {
                            if let Some(doc) = &tab.document {
                                let dim = tab.page_dimensions[page_index];
                                let target_w =
                                    (dim.width_pt * BASE_DPI_SCALE * bucket_zoom).round() as i32;
                                let target_h =
                                    (dim.height_pt * BASE_DPI_SCALE * bucket_zoom).round() as i32;

                                if let Ok(color_img) =
                                    doc.render_page(page_index, target_w, target_h)
                                {
                                    self.texture_cache.insert_color_image(ctx, key, color_img);
                                }
                            }
                        }

                        if let Some(tex) = self.texture_cache.get(&key) {
                            painter.image(
                                tex.id(),
                                page_rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }

                        let page_dim = tab.page_dimensions[page_index];

                        // --- DESSIN DES SURBRILLANCES DE RECHERCHE `Ctrl+F` (JALON 4) ---
                        if self.search_bar.is_open && !self.search_bar.matches.is_empty() {
                            for (m_idx, m) in self.search_bar.matches.iter().enumerate() {
                                if m.page_index != page_index {
                                    continue;
                                }
                                let is_active_match = m_idx == self.search_bar.current_match_index;
                                let fill_col = if is_active_match {
                                    Color32::from_rgba_unmultiplied(255, 130, 15, 135)
                                } else {
                                    Color32::from_rgba_unmultiplied(255, 225, 20, 105)
                                };
                                let stroke_col = if is_active_match {
                                    Color32::from_rgb(235, 75, 0)
                                } else {
                                    Color32::from_rgb(210, 165, 0)
                                };

                                for b in &m.highlight_boxes {
                                    let screen_b = b.to_screen_rect(page_rect, page_dim).expand(2.5);
                                    painter.rect_filled(screen_b, CornerRadius::same(3), fill_col);
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

                                // Badge au-dessus de l'occurrence active (avec indication d'angle si le texte est en diagonale/vertical !)
                                if is_active_match {
                                    if let Some(first_b) = m.highlight_boxes.first() {
                                        let screen_b = first_b.to_screen_rect(page_rect, page_dim);
                                        let badge_rect = Rect::from_min_size(
                                            Pos2::new(screen_b.min.x, (screen_b.min.y - 24.0).max(page_rect.min.y + 4.0)),
                                            Vec2::new(
                                                if m.angle_deg.abs() > 1.0 { 195.0 } else { 130.0 },
                                                20.0,
                                            ),
                                        );
                                        painter.rect_filled(
                                            badge_rect,
                                            CornerRadius::same(5),
                                            Color32::from_rgb(225, 75, 15),
                                        );
                                        let label = if m.angle_deg.abs() > 1.0 {
                                            format!(
                                                "🔎 #{}/{} (Orienté {:+.0}°)",
                                                m_idx + 1,
                                                self.search_bar.matches.len(),
                                                m.angle_deg
                                            )
                                        } else {
                                            format!(
                                                "🔎 Occurrence #{}/{}",
                                                m_idx + 1,
                                                self.search_bar.matches.len()
                                            )
                                        };
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

                        // --- DESSIN DE LA SÉLECTION DE TEXTE (`Ctrl+C`) (JALON 4) ---
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

                            for b in &tab.text_selection.highlight_boxes {
                                let sb = b.to_screen_rect(page_rect, page_dim).expand(1.5);
                                painter.rect_filled(
                                    sb,
                                    CornerRadius::same(2),
                                    Color32::from_rgba_unmultiplied(55, 135, 255, 95),
                                );
                            }

                            if tab.text_selection.has_selection() {
                                if let Some(last_b) = tab.text_selection.highlight_boxes.last() {
                                    let sb = last_b.to_screen_rect(page_rect, page_dim);
                                    selection_toolbar_anchor = Some((
                                        Pos2::new(sb.min.x.max(canvas_rect.min.x + 16.0), sb.max.y + 8.0),
                                        tab.text_selection.selected_text.clone(),
                                        tab.text_selection.selected_text.chars().count(),
                                        tab.text_selection.copied_feedback_frames > 0,
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
                } else {
                    painter.text(
                        canvas_rect.center(),
                        Align2::CENTER_CENTER,
                        "Aucun onglet ouvert — Appuyez sur Ctrl+O ou Ctrl+T",
                        FontId::proportional(16.0),
                        Color32::from_rgb(150, 160, 180),
                    );
                }

                // Barre flottante d'action sur le texte sélectionné (`📋 Copier Ctrl+C`)
                if let Some((anchor_pos, sel_text, char_count, copied_ok)) = selection_toolbar_anchor
                {
                    self.render_selection_floating_bar(ctx, anchor_pos, &sel_text, char_count, copied_ok);
                }

                // Barre de recherche flottante `Ctrl+F` en haut à droite du canevas
                if self.search_bar.is_open {
                    self.render_floating_search_bar(ctx, canvas_rect);
                }

                // Si la barre latérale est en mode Auto-Hide (100% plein écran), on dessine la lisière réactive gauche
                if !self.sidebar.pinned {
                    self.render_autohide_sidebar_overlay(ctx, canvas_rect);
                }

                // Mini-pilule d'état flottante en bas à droite
                self.render_status_pill(ui, canvas_rect);

                // Palette de commandes flottante (`Ctrl+K`)
                if self.command_palette.is_open {
                    self.render_command_palette(ctx, canvas_rect);
                }
            });

        // Déclenchement automatique de la capture d'écran après stabilisation du rendu GPU
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
    /// Dessine la barre flottante de copie lorsque du texte est sélectionné (`Ctrl+C`)
    fn render_selection_floating_bar(
        &mut self,
        ctx: &Context,
        pos: Pos2,
        sel_text: &str,
        char_count: usize,
        copied_ok: bool,
    ) {
        let preview: String = if sel_text.chars().count() > 38 {
            let s: String = sel_text.chars().take(36).collect();
            format!("\"{}...\"", s)
        } else {
            format!("\"{}\"", sel_text)
        };

        egui::Area::new(egui::Id::new("superpdf_text_selection_bar"))
            .fixed_pos(pos)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(20, 26, 40, 245))
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
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!("{} car. : {}", char_count, preview))
                                    .size(11.5)
                                    .color(Color32::from_rgb(200, 220, 255)),
                            );
                            if ui.small_button("✖").clicked() {
                                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                    tab.text_selection.clear();
                                }
                            }
                        });
                    });
            });
    }

    /// Dessine la barre de recherche flottante `Ctrl+F` en haut à droite du canevas PDF
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
                                .add_enabled(total > 0, egui::Button::new("↑").small())
                                .on_hover_text("Occurrence précédente (Shift+Entrée)")
                                .clicked()
                            {
                                step_dir = -1;
                            }
                            if ui
                                .add_enabled(total > 0, egui::Button::new("↓").small())
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
                tab.ensure_text_layers(self.engine);
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

    /// Dessine le contenu complet de la barre latérale gauche
    fn render_sidebar_contents(&mut self, ui: &mut egui::Ui) {
        let mut pending_cmd: Option<PaletteCommand> = None;
        let mut tab_to_close: Option<usize> = None;
        let mut tab_to_select: Option<usize> = None;
        let mut match_to_select: Option<usize> = None;

        ui.spacing_mut().item_spacing = Vec2::new(6.0, 5.0);

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

                ui.add_space(4.0);

                // Barre de déclenchement rapide Ctrl+K
                let search_btn = egui::Button::new(
                    egui::RichText::new("🔍 Palette : onglet, outil, page... (Ctrl+K)")
                        .size(11.5)
                        .color(Color32::from_rgb(175, 185, 208)),
                )
                .fill(Color32::from_rgb(33, 38, 50))
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(SIDEBAR_WIDTH - 26.0, 26.0));

                if ui.add(search_btn).clicked() {
                    self.command_palette.open();
                }

                ui.add_space(6.0);

                // 2. Grille d'outils compacts (2 lignes x 4 colonnes)
                ui.label(
                    egui::RichText::new("OUTILS RAPIDES (1 TOUCHE)")
                        .size(10.0)
                        .strong()
                        .color(Color32::from_rgb(120, 132, 158)),
                );
                egui::Grid::new("superpdf_tools_grid")
                    .num_columns(4)
                    .spacing([5.0, 5.0])
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
                            .min_size(Vec2::new(55.0, 34.0));

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

                ui.add_space(6.0);

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

        // Si une recherche Ctrl+F est active avec des résultats, on réserve un tiroir en bas pour la liste des occurrences
        let has_search_drawer =
            self.search_bar.is_open && !self.search_bar.matches.is_empty();
        let tabs_reserved_bottom = if has_search_drawer { 285.0 } else { 155.0 };
        let available_h = (ui.available_height() - tabs_reserved_bottom).max(95.0);

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

                                let short_title = if tab.title.chars().count() > 22 {
                                    let s: String = tab.title.chars().take(20).collect();
                                    format!("{}...", s)
                                } else {
                                    tab.title.clone()
                                };

                                let ram_state = if is_hib { "💤 0 Mo" } else { "⚡ Actif" };

                                let title_btn = egui::Button::new(
                                    egui::RichText::new(format!(
                                        "{}\np.{}/{} • {:.0}% • {}",
                                        short_title,
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

        // Tiroir contextuel des résultats de recherche `Ctrl+F` dans la barre latérale
        if has_search_drawer {
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
                                        let angle_tag = if m.angle_deg.abs() > 1.0 {
                                            format!(" [↗{:+.0}°]", m.angle_deg)
                                        } else {
                                            String::new()
                                        };
                                        let btn = egui::Button::new(
                                            egui::RichText::new(format!(
                                                "P.{}{} : {}",
                                                m.page_index + 1,
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
                        egui::RichText::new(format!("🧠 RAM Processus : {:.1} Mo", ram_mb))
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
        let cache_mb = self.texture_cache.allocated_bytes as f32 / (1024.0 * 1024.0);

        let pill_pos = Pos2::new(
            (canvas_rect.max.x - 670.0).max(canvas_rect.min.x + 12.0),
            canvas_rect.max.y - 40.0,
        );

        egui::Area::new(egui::Id::new("status_pill_jalon4"))
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
                            ui.label(
                                egui::RichText::new(format!(
                                    "Onglet {}/{} ({} 💤) • Page {}/{} • {:.0}%",
                                    self.active_tab + 1,
                                    self.tabs.len(),
                                    hibernated_count,
                                    current_page,
                                    page_count,
                                    tab.viewport.zoom * 100.0
                                ))
                                .strong()
                                .size(11.5)
                                .color(Color32::WHITE),
                            );
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "🧠 RAM {:.1} Mo (Cache {:.1} Mo) | ⚡ {:.0} FPS",
                                    ram_mb, cache_mb, self.fps_smoothed
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
