use crate::pdf_engine::{ensure_custom_sample_pdf, PdfEngine};
use crate::tabs::{
    ActiveTool, CommandPaletteState, DocumentTab, PaletteCommand, SidebarState, SIDEBAR_WIDTH,
};
use crate::viewport::{ViewportState, BASE_DPI_SCALE};
use egui::{
    Align2, Color32, Context, CornerRadius, FontId, Key, Margin, Order, Pos2, Rect, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2, ViewportCommand,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Mode initial de la sidebar pour les tests automatisés QA
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
    pub screenshot_output: Option<PathBuf>,
}

/// Clé de cache pour une texture de page rendue : (id_onglet, index_page, niveau_zoom_quantifié)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PageTextureKey {
    tab_id: u64,
    page_index: usize,
    zoom_bucket: u32,
}

pub struct SuperPdfApp {
    /// Moteur PDFium alloué sur le tas pour stabilité d'adresse
    engine: &'static PdfEngine,
    /// Liste des onglets PDF ouverts dans la barre verticale Arc
    pub tabs: Vec<DocumentTab>,
    /// Index de l'onglet actuellement affiché
    pub active_tab: usize,
    /// Prochain identifiant unique d'onglet
    next_tab_id: u64,
    /// État de la barre latérale gauche style Arc Browser
    pub sidebar: SidebarState,
    /// Outil actif (Sélection, Main, Surligneur, Texte, Dessin, Formes, Note, Mesure)
    pub active_tool: ActiveTool,
    /// État de la palette de commandes flottante (`Ctrl+K`)
    pub command_palette: CommandPaletteState,
    /// Cache de textures GPU pour les pages visibles
    texture_cache: HashMap<PageTextureKey, TextureHandle>,
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

const DEMO_TAB_SPECS: [(&str, &str, usize); 6] = [
    ("01_Architecture_Rust_GPU.pdf", "ARCHITECTURE RUST & GPU", 5),
    ("02_Rapport_Graphiques_OCR.pdf", "RAPPORT GRAPHIQUES & OCR", 7),
    ("03_Norme_ISO_32000_Annotations.pdf", "NORME ISO 32000 ANNOTATIONS", 9),
    ("04_Analyse_Capteurs_ISEN.pdf", "ANALYSE CAPTEURS ISEN", 4),
    ("05_Benchmark_RAM_15_Onglets.pdf", "BENCHMARK RAM 15 ONGLETS", 6),
    ("06_Notes_Reunion_Projet.pdf", "NOTES REUNION PROJET", 3),
];

impl SuperPdfApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, automation: AutomationConfig) -> Self {
        let engine_box = Box::new(
            PdfEngine::new().expect("Échec d'initialisation de PDFium (pdfium.dll manquant)"),
        );
        let engine: &'static PdfEngine = Box::leak(engine_box);

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

        let mut app = Self {
            engine,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            sidebar,
            active_tool: automation.initial_tool.unwrap_or(ActiveTool::Select),
            command_palette: CommandPaletteState::default(),
            texture_cache: HashMap::new(),
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
                app.active_tab = target_tab.saturating_sub(1).min(app.tabs.len() - 1);
            }
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

        app
    }

    /// Ouvre un fichier PDF dans un nouvel onglet et bascule dessus
    pub fn open_pdf_in_new_tab(&mut self, path: &Path) {
        let color_idx = self.tabs.len() % TAB_BADGE_COLORS.len();
        let (badge_color, _) = TAB_BADGE_COLORS[color_idx];

        match self.engine.load_document(path) {
            Ok(doc) => {
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                self.tabs.push(DocumentTab {
                    id,
                    document: doc,
                    viewport: ViewportState::default(),
                    is_modified: false,
                    badge_color,
                });
                self.active_tab = self.tabs.len() - 1;
                self.error_message = None;
            }
            Err(err) => {
                self.error_message = Some(format!("{:#}", err));
            }
        }
    }

    /// Génère et ouvre un nouvel onglet PDF de démonstration distinct
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

    /// Ferme un onglet par son index
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(index);
        self.texture_cache.retain(|k, _| k.tab_id != removed.id);

        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    /// Quantifie le facteur de zoom par paliers de 5%
    fn zoom_bucket(zoom: f32) -> u32 {
        ((zoom * 20.0).round() as u32).max(1)
    }

    /// Exécute une commande issue de la Palette `Ctrl+K` ou d'un raccourci
    fn execute_palette_command(&mut self, cmd: PaletteCommand, viewport_size: Vec2) {
        match cmd {
            PaletteCommand::SwitchTab(idx) => {
                if idx < self.tabs.len() {
                    self.active_tab = idx;
                }
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
                        .fit_to_width(viewport_size, &tab.document.page_dimensions);
                }
            }
            PaletteCommand::FitToPage => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport
                        .fit_to_page(viewport_size, &tab.document.page_dimensions);
                }
            }
            PaletteCommand::ZoomReset100 => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport.zoom = 1.0;
                    tab.viewport.scroll_offset.x = 0.0;
                    tab.viewport
                        .clamp_scroll(viewport_size, &tab.document.page_dimensions);
                    tab.viewport.last_gesture_label = "Zoom 100%".to_string();
                }
            }
            PaletteCommand::JumpToPage(page_idx) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.viewport
                        .jump_to_page(page_idx, viewport_size, &tab.document.page_dimensions);
                }
            }
        }
    }

    /// Gère l'ensemble des entrées Trackpad Windows Precision, Survol Sidebar Arc et Raccourcis Clavier
    fn handle_inputs(&mut self, ctx: &Context, canvas_rect: Rect) {
        let viewport_size = canvas_rect.size();

        // Application du saut de page initial demandé par CLI une fois la taille du viewport connue
        if self.frame_count == 1 {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if let Some(target_page) = self.automation.initial_page {
                    tab.viewport.jump_to_page(
                        target_page.saturating_sub(1),
                        viewport_size,
                        &tab.document.page_dimensions,
                    );
                }
                if self.automation.initial_scroll_x.is_some()
                    || self.automation.initial_scroll_y.is_some()
                {
                    tab.viewport
                        .clamp_scroll(viewport_size, &tab.document.page_dimensions);
                }
            }
        }

        let mut pending_commands: Vec<PaletteCommand> = Vec::new();
        let mut tab_cycle_dir: i32 = 0;

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

            // 2. Détection du survol du bord gauche au Trackpad pour la Sidebar Arc (sauf si forcée en mode test)
            if self.automation.sidebar_mode != InitialSidebarMode::AutoHideHover {
                let pointer_x = input.pointer.hover_pos().map(|p| p.x);
                self.sidebar.update_hover_hysteresis(pointer_x);
            }

            let ctrl = input.modifiers.ctrl || input.modifiers.command;
            let shift = input.modifiers.shift;

            // 3. Raccourcis globaux (Palette Ctrl+K / Ctrl+P, Sidebar Ctrl+B, Onglets Ctrl+Tab / Ctrl+W / Ctrl+T / Ctrl+1..9)
            if ctrl && (input.key_pressed(Key::K) || input.key_pressed(Key::P)) {
                self.command_palette.toggle();
            }
            if input.key_pressed(Key::Escape) && self.command_palette.is_open {
                self.command_palette.close();
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

            // Ctrl+1..8 pour accès direct aux onglets (Ctrl+9 = Ajuster largeur, Ctrl+0 = Ajuster page)
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

            // Si la palette Ctrl+K est ouverte, les flèches et touches d'outils sont réservées à la palette
            if self.command_palette.is_open {
                return;
            }

            // 4. Raccourcis d'outils à 1 touche (hors modificateur Ctrl)
            if !ctrl {
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
            let pages = &tab.document.page_dimensions;

            // Ignorer le scroll du PDF si le pointeur survole la sidebar Arc déployée
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

            // Défilement clavier
            let step_y = 75.0;
            let step_x = 60.0;
            if !ctrl {
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

        if self.sidebar.step_animation(dt.max(0.016)) {
            ctx.request_repaint();
        }

        // Thème sombre Arc
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(18, 20, 26);
        ctx.set_visuals(visuals);

        // Si la sidebar est épinglée, elle occupe un SidePanel gauche fixe de 264 px
        if self.sidebar.pinned {
            egui::SidePanel::left("arc_sidebar_pinned")
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

                if let Some(tab) = self.tabs.get(self.active_tab) {
                    if tab.viewport.is_panning {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    } else if self.active_tool == ActiveTool::Pan {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                }

                let painter = ui.painter_at(canvas_rect);

                if let Some(err) = &self.error_message {
                    painter.text(
                        canvas_rect.center(),
                        Align2::CENTER_CENTER,
                        format!("Erreur : {}", err),
                        FontId::proportional(16.0),
                        Color32::from_rgb(255, 110, 110),
                    );
                } else if let Some(tab) = self.tabs.get(self.active_tab) {
                    let doc = &tab.document;
                    let viewport_size = canvas_rect.size();
                    let (layouts, _, total_h) = tab.viewport.compute_layout(&doc.page_dimensions);
                    let bucket = Self::zoom_bucket(tab.viewport.zoom);
                    let bucket_zoom = (bucket as f32) / 20.0;

                    for layout in &layouts {
                        let page_rect = tab.viewport.page_screen_rect(layout, canvas_rect);
                        let expanded_canvas = canvas_rect.expand(200.0);
                        if !expanded_canvas.intersects(page_rect) {
                            continue;
                        }

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
                            page_index: layout.page_index,
                            zoom_bucket: bucket,
                        };

                        if !self.texture_cache.contains_key(&key) {
                            let dim = doc.page_dimensions[layout.page_index];
                            let target_w =
                                (dim.width_pt * BASE_DPI_SCALE * bucket_zoom).round() as i32;
                            let target_h =
                                (dim.height_pt * BASE_DPI_SCALE * bucket_zoom).round() as i32;

                            if let Ok(color_img) =
                                doc.render_page(layout.page_index, target_w, target_h)
                            {
                                let tex = ctx.load_texture(
                                    format!("pdf_t{}_p{}_z{}", tab.id, layout.page_index, bucket),
                                    color_img,
                                    TextureOptions::LINEAR,
                                );
                                self.texture_cache.insert(key, tex);
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

                // Si la sidebar est en mode Auto-Hide (100% plein écran), on dessine la lisière réactive gauche
                // et la sidebar flottante si elle est déployée au survol
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
    /// Dessine le contenu complet de la Sidebar gauche style Arc Browser
    fn render_sidebar_contents(&mut self, ui: &mut egui::Ui) {
        let mut pending_cmd: Option<PaletteCommand> = None;
        let mut tab_to_close: Option<usize> = None;
        let mut tab_to_select: Option<usize> = None;

        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

        // 1. En-tête de la Sidebar Arc
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("⚡ ARC PDF")
                            .strong()
                            .size(14.0)
                            .color(Color32::from_rgb(130, 180, 255)),
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
                            .small_button("⌘K")
                            .on_hover_text("Palette de commandes rapide (Ctrl+K)")
                            .clicked()
                        {
                            self.command_palette.toggle();
                        }
                    });
                });

                ui.add_space(6.0);

                // Barre de déclenchement rapide Ctrl+K style Arc
                let search_btn = egui::Button::new(
                    egui::RichText::new("🔍 Rechercher onglet, outil, page... (Ctrl+K)")
                        .size(11.5)
                        .color(Color32::from_rgb(175, 185, 208)),
                )
                .fill(Color32::from_rgb(33, 38, 50))
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(SIDEBAR_WIDTH - 26.0, 28.0));

                if ui.add(search_btn).clicked() {
                    self.command_palette.open();
                }

                ui.add_space(8.0);

                // 2. Grille d'outils compacts façon Arc / Adobe Acrobat (2 lignes x 4 colonnes)
                ui.label(
                    egui::RichText::new("OUTILS RAPIDES (1 TOUCHE)")
                        .size(10.0)
                        .strong()
                        .color(Color32::from_rgb(120, 132, 158)),
                );
                egui::Grid::new("arc_tools_grid")
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
                            .min_size(Vec2::new(54.0, 36.0));

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

                ui.add_space(8.0);

                // 3. En-tête de la liste verticale des onglets
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("ONGLETS OUVERTS ({})", self.tabs.len()))
                            .size(10.5)
                            .strong()
                            .color(Color32::from_rgb(120, 132, 158)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("➕ Démo")
                            .on_hover_text("Ouvrir un nouvel onglet de démonstration (Ctrl+T)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::NewDemoTab);
                        }
                        if ui
                            .small_button("📂 Ouvrir")
                            .on_hover_text("Ouvrir un fichier PDF (Ctrl+O)")
                            .clicked()
                        {
                            pending_cmd = Some(PaletteCommand::OpenFileDialog);
                        }
                    });
                });
            });

        ui.separator();

        // 4. Liste déroulante verticale des onglets (compatible scroll 2 doigts Trackpad)
        let available_h = (ui.available_height() - 108.0).max(120.0);
        egui::ScrollArea::vertical()
            .max_height(available_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                for (idx, tab) in self.tabs.iter().enumerate() {
                    let is_active = idx == self.active_tab;
                    let bg_fill = if is_active {
                        Color32::from_rgb(40, 48, 68)
                    } else {
                        Color32::from_rgb(27, 31, 41)
                    };
                    let stroke = if is_active {
                        Stroke::new(1.0_f32, tab.badge_color)
                    } else {
                        Stroke::new(1.0_f32, Color32::from_rgb(36, 41, 54))
                    };

                    egui::Frame::NONE
                        .inner_margin(Margin::symmetric(10, 6))
                        .outer_margin(Margin::symmetric(8, 0))
                        .fill(bg_fill)
                        .stroke(stroke)
                        .corner_radius(CornerRadius::same(8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // Pastille de couleur de l'onglet + numéro
                                let badge_text = format!("#{}", idx + 1);
                                ui.colored_label(tab.badge_color, badge_text);

                                let current_p = tab.viewport.current_page_index(
                                    Vec2::new(900.0, 800.0),
                                    &tab.document.page_dimensions,
                                ) + 1;
                                let total_p = tab.page_count();

                                let short_title = if tab.document.title.chars().count() > 24 {
                                    let s: String = tab.document.title.chars().take(22).collect();
                                    format!("{}...", s)
                                } else {
                                    tab.document.title.clone()
                                };

                                let title_btn = egui::Button::new(
                                    egui::RichText::new(format!(
                                        "{}\np.{}/{} • {:.0}%",
                                        short_title,
                                        current_p,
                                        total_p,
                                        tab.viewport.zoom * 100.0
                                    ))
                                    .size(11.5)
                                    .color(if is_active {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_rgb(190, 198, 215)
                                    }),
                                )
                                .frame(false);

                                if ui
                                    .add_sized([154.0, 30.0], title_btn)
                                    .on_hover_text(format!(
                                        "{} (Ctrl+{})",
                                        tab.document.title,
                                        idx + 1
                                    ))
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

        ui.separator();

        // 5. Pied de la Sidebar Arc : contrôles rapides de Page & Zoom
        egui::Frame::NONE
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let pages = tab.document.page_dimensions.clone();
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
                                .size(12.0)
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
                                    .size(11.5)
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
            self.active_tab = idx;
        }
        if let Some(idx) = tab_to_close {
            self.close_tab(idx);
        }
        if let Some(cmd) = pending_cmd {
            self.execute_palette_command(cmd, Vec2::new(1000.0, 800.0));
        }
    }

    /// Dessine la lisière gauche réactive et la Sidebar flottante en surimpression lorsque `sidebar.pinned == false`
    fn render_autohide_sidebar_overlay(&mut self, ctx: &Context, canvas_rect: Rect) {
        let painter = ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("arc_edge_indicator"),
        ));

        // Fine lisière lumineuse sur le bord gauche pour indiquer la zone de survol Trackpad
        let edge_strip = Rect::from_min_size(canvas_rect.min, Vec2::new(4.0, canvas_rect.height()));
        painter.rect_filled(
            edge_strip,
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(80, 145, 255, 110),
        );

        // Si masquée, afficher un discret badge en haut à gauche permettant aussi d'ouvrir au clic
        if self.sidebar.anim_progress < 0.05 {
            egui::Area::new(egui::Id::new("arc_collapsed_trigger_pill"))
                .fixed_pos(Pos2::new(10.0, 12.0))
                .order(Order::Foreground)
                .show(ctx, |ui| {
                    let btn = egui::Button::new(
                        egui::RichText::new("▶ Arc (Survol / Ctrl+B)")
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

        // Sidebar flottante animée en surimpression
        let x_offset = -SIDEBAR_WIDTH * (1.0 - self.sidebar.anim_progress);
        let overlay_rect = Rect::from_min_size(
            Pos2::new(x_offset, 8.0),
            Vec2::new(SIDEBAR_WIDTH, canvas_rect.height() - 16.0),
        );

        egui::Area::new(egui::Id::new("arc_floating_sidebar_overlay"))
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

    /// Dessine la pilule d'état flottante en bas à droite (affiche l'outil actif, la page, le zoom et le FPS)
    fn render_status_pill(&mut self, ui: &mut egui::Ui, canvas_rect: Rect) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let pages = &tab.document.page_dimensions;
        let viewport_size = canvas_rect.size();
        let current_page = tab.viewport.current_page_index(viewport_size, pages) + 1;
        let page_count = pages.len();
        let (tool_icon, tool_name, tool_key) = self.active_tool.icon_and_label();
        let mode_label = if self.sidebar.pinned {
            "Sidebar Épinglée"
        } else if self.sidebar.hover_revealed {
            "Sidebar Survol (Auto-Hide)"
        } else {
            "Plein Écran 100% (Auto-Hide)"
        };

        let pill_pos = Pos2::new(
            (canvas_rect.max.x - 590.0).max(canvas_rect.min.x + 12.0),
            canvas_rect.max.y - 40.0,
        );

        egui::Area::new(egui::Id::new("status_pill_jalon2"))
            .fixed_pos(pill_pos)
            .order(Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(22, 26, 36, 230))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(56, 66, 88)))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::symmetric(12, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.colored_label(
                                Color32::from_rgb(130, 185, 255),
                                format!("{} {} ({})", tool_icon, tool_name, tool_key),
                            );
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "Onglet {}/{} • Page {}/{} • {:.0}%",
                                    self.active_tab + 1,
                                    self.tabs.len(),
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
                                    "{} | ⚡ {:.0} FPS",
                                    mode_label, self.fps_smoothed
                                ))
                                .size(11.0)
                                .color(Color32::from_rgb(155, 168, 190)),
                            );
                        });
                    });
            });
    }

    /// Dessine la Palette de Commandes rapide (`Ctrl+K` / `Ctrl+P`) inspirée d'Arc Browser
    fn render_command_palette(&mut self, ctx: &Context, canvas_rect: Rect) {
        let tabs_info: Vec<(usize, String, usize)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.document.title.clone(), t.page_count()))
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

        // Voile sombre semi-transparent derrière la palette
        let backdrop_painter = ctx.layer_painter(egui::LayerId::new(
            Order::Foreground,
            egui::Id::new("palette_backdrop"),
        ));
        backdrop_painter.rect_filled(
            ctx.screen_rect(),
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(8, 10, 15, 150),
        );

        let palette_w = 540.0;
        let palette_pos = Pos2::new(
            ctx.screen_rect().center().x - palette_w * 0.5,
            (ctx.screen_rect().min.y + 85.0).max(24.0),
        );

        egui::Area::new(egui::Id::new("arc_command_palette_modal"))
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
                                egui::RichText::new("⌘ PALETTE DE COMMANDES ARC")
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
