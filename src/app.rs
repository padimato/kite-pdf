use crate::pdf_engine::{LoadedPdfDocument, PdfEngine};
use crate::viewport::{ViewportState, BASE_DPI_SCALE};
use egui::{
    Align2, Color32, Context, CornerRadius, FontId, Key, Margin, Pos2, Rect, Sense, Stroke,
    StrokeKind, TextureHandle, TextureOptions, Vec2, ViewportCommand,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Configuration d'automatisation (utilisée par les sous-agents QA pour les tests visuels et captures d'écran)
#[derive(Debug, Clone, Default)]
pub struct AutomationConfig {
    pub initial_pdf: PathBuf,
    pub initial_zoom: Option<f32>,
    pub initial_page: Option<usize>,
    pub initial_scroll_x: Option<f32>,
    pub initial_scroll_y: Option<f32>,
    pub screenshot_output: Option<PathBuf>,
}

/// Clé de cache pour une texture de page rendue : (index_page, niveau_zoom_quantifié)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PageTextureKey {
    page_index: usize,
    zoom_bucket: u32,
}

pub struct SuperPdfApp {
    /// Moteur PDFium alloué sur le tas pour stabilité d'adresse
    engine: &'static PdfEngine,
    /// Document PDF actuellement ouvert
    document: Option<LoadedPdfDocument<'static>>,
    /// État du viewport (zoom, scroll 2D, inertie trackpad)
    pub viewport: ViewportState,
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

impl SuperPdfApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, automation: AutomationConfig) -> Self {
        let engine_box = Box::new(
            PdfEngine::new().expect("Échec d'initialisation de PDFium (pdfium.dll manquant)"),
        );
        let engine: &'static PdfEngine = Box::leak(engine_box);

        let mut app = Self {
            engine,
            document: None,
            viewport: ViewportState::default(),
            texture_cache: HashMap::new(),
            error_message: None,
            frame_count: 0,
            screenshot_requested: false,
            automation: automation.clone(),
            last_frame_instant: Instant::now(),
            fps_smoothed: 60.0,
        };

        app.open_pdf_file(&automation.initial_pdf);

        if let Some(z) = automation.initial_zoom {
            app.viewport.zoom = z;
            app.viewport.last_gesture_label = format!("Zoom initial {:.0}%", z * 100.0);
        }
        if let Some(sx) = automation.initial_scroll_x {
            app.viewport.scroll_offset.x = sx;
        }
        if let Some(sy) = automation.initial_scroll_y {
            app.viewport.scroll_offset.y = sy;
        }

        app
    }

    pub fn open_pdf_file(&mut self, path: &Path) {
        self.texture_cache.clear();
        match self.engine.load_document(path) {
            Ok(doc) => {
                self.viewport = ViewportState::default();
                self.error_message = None;
                self.document = Some(doc);
            }
            Err(err) => {
                self.error_message = Some(format!("{:#}", err));
            }
        }
    }

    /// Quantifie le facteur de zoom par paliers de 5% pour éviter de régénérer la texture à chaque micro-delta du trackpad
    fn zoom_bucket(zoom: f32) -> u32 {
        ((zoom * 20.0).round() as u32).max(1)
    }

    /// Gère l'ensemble des entrées Trackpad Windows Precision (Scroll 2D, Pinch-to-Zoom, Space+Drag) et Clavier
    fn handle_inputs(&mut self, ctx: &Context, canvas_rect: Rect) {
        let Some(doc) = &self.document else {
            return;
        };
        let pages = &doc.page_dimensions;
        let viewport_size = canvas_rect.size();

        // Application du saut de page initial demandé par CLI une fois la taille du viewport connue
        if self.frame_count == 1 {
            if let Some(target_page) = self.automation.initial_page {
                self.viewport
                    .jump_to_page(target_page.saturating_sub(1), viewport_size, pages);
            }
            if self.automation.initial_scroll_x.is_some()
                || self.automation.initial_scroll_y.is_some()
            {
                self.viewport.clamp_scroll(viewport_size, pages);
            }
        }

        let mut open_file_requested = false;

        ctx.input(|input| {
            // 1. Vérifier si une capture d'écran GPU vient d'être reçue
            for event in &input.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    if let Some(out_path) = &self.automation.screenshot_output {
                        if let Some(parent) = out_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let w = image.width() as u32;
                        let h = image.height() as u32;
                        let rgba: Vec<u8> = image
                            .pixels
                            .iter()
                            .flat_map(|c| c.to_array())
                            .collect();
                        if let Some(buf) = image::RgbaImage::from_raw(w, h, rgba) {
                            let _ = buf.save(out_path);
                        }
                        std::process::exit(0);
                    }
                }
            }

            // Position d'ancrage du pointeur (ou centre de l'écran par défaut)
            let pointer_in_viewport = input
                .pointer
                .hover_pos()
                .map(|p| p - canvas_rect.min)
                .unwrap_or(viewport_size * 0.5);

            // 2. Trackpad Pinch-to-Zoom (2 doigts) & Ctrl+Molette
            let zoom_delta = input.zoom_delta();
            if (zoom_delta - 1.0).abs() > 1e-3 {
                self.viewport.apply_zoom_around_anchor(
                    zoom_delta,
                    pointer_in_viewport,
                    viewport_size,
                    pages,
                );
                self.viewport.last_gesture_label = format!(
                    "Trackpad Pinch-Zoom -> {:.0}%",
                    self.viewport.zoom * 100.0
                );
            }

            // 3. Trackpad Défilement Fluide 2 Doigts (Vertical + Horizontal)
            // Note : `smooth_scroll_delta` est alimenté par le Windows Precision Touchpad en pixels continus
            let scroll_delta = input.smooth_scroll_delta;
            if !input.modifiers.ctrl && scroll_delta.length_sq() > 0.01 {
                self.viewport
                    .apply_pan_delta(scroll_delta, viewport_size, pages);
                self.viewport.inertia_velocity = scroll_delta * 0.35;
                self.viewport.last_gesture_label = format!(
                    "Trackpad Scroll 2D (dx:{:+.0}, dy:{:+.0})",
                    scroll_delta.x, scroll_delta.y
                );
            } else if self.viewport.inertia_velocity.length_sq() > 0.5 {
                // Inertie douce après relâchement des 2 doigts sur le trackpad
                let v = self.viewport.inertia_velocity;
                self.viewport.apply_pan_delta(v, viewport_size, pages);
                self.viewport.inertia_velocity *= 0.86;
            } else {
                self.viewport.inertia_velocity = Vec2::ZERO;
            }

            // 4. Panoramique à 1 doigt sur le trackpad (Espace maintenu + glisser ou bouton central)
            let space_held = input.key_down(Key::Space);
            let drag_pan_active =
                (space_held && input.pointer.primary_down()) || input.pointer.middle_down();
            self.viewport.is_panning = drag_pan_active;
            if drag_pan_active {
                let drag_delta = input.pointer.delta();
                if drag_delta.length_sq() > 0.01 {
                    self.viewport
                        .apply_pan_delta(drag_delta, viewport_size, pages);
                    self.viewport.last_gesture_label =
                        "Panoramique 1 doigt (Space + Glisser)".to_string();
                }
            }

            // 5. Raccourcis Clavier
            let ctrl = input.modifiers.ctrl || input.modifiers.command;
            let shift = input.modifiers.shift;

            if ctrl && input.key_pressed(Key::O) {
                open_file_requested = true;
            }
            if ctrl && (input.key_pressed(Key::Plus) || input.key_pressed(Key::Equals)) {
                self.viewport.apply_zoom_around_anchor(
                    1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
                self.viewport.last_gesture_label =
                    format!("Zoom + ({:.0}%)", self.viewport.zoom * 100.0);
            }
            if ctrl && input.key_pressed(Key::Minus) {
                self.viewport.apply_zoom_around_anchor(
                    1.0 / 1.2,
                    viewport_size * 0.5,
                    viewport_size,
                    pages,
                );
                self.viewport.last_gesture_label =
                    format!("Zoom - ({:.0}%)", self.viewport.zoom * 100.0);
            }
            if ctrl && input.key_pressed(Key::Num0) {
                self.viewport.fit_to_page(viewport_size, pages);
            }
            if ctrl && input.key_pressed(Key::Num9) {
                self.viewport.fit_to_width(viewport_size, pages);
            }

            // Défilement clavier fluide
            let step_y = 75.0;
            let step_x = 60.0;
            if !ctrl {
                if input.key_pressed(Key::ArrowDown) || input.key_pressed(Key::J) {
                    self.viewport
                        .apply_pan_delta(Vec2::new(0.0, -step_y), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowUp) || input.key_pressed(Key::K) {
                    self.viewport
                        .apply_pan_delta(Vec2::new(0.0, step_y), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowRight) || input.key_pressed(Key::L) {
                    self.viewport
                        .apply_pan_delta(Vec2::new(-step_x, 0.0), viewport_size, pages);
                }
                if input.key_pressed(Key::ArrowLeft) || input.key_pressed(Key::H) {
                    self.viewport
                        .apply_pan_delta(Vec2::new(step_x, 0.0), viewport_size, pages);
                }
                if input.key_pressed(Key::PageDown) {
                    self.viewport.apply_pan_delta(
                        Vec2::new(0.0, -viewport_size.y * 0.85),
                        viewport_size,
                        pages,
                    );
                }
                if input.key_pressed(Key::PageUp)
                    || (shift && input.key_pressed(Key::Space))
                {
                    self.viewport.apply_pan_delta(
                        Vec2::new(0.0, viewport_size.y * 0.85),
                        viewport_size,
                        pages,
                    );
                }
                if input.key_pressed(Key::Home) {
                    self.viewport.jump_to_page(0, viewport_size, pages);
                }
                if input.key_pressed(Key::End) && !pages.is_empty() {
                    self.viewport
                        .jump_to_page(pages.len() - 1, viewport_size, pages);
                }
            }
        });

        if open_file_requested {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Document PDF", &["pdf"])
                .pick_file()
            {
                self.open_pdf_file(&path);
            }
        }

        if self.viewport.inertia_velocity.length_sq() > 0.5 {
            ctx.request_repaint();
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

        // Style global sombre épuré (Zéro barre supérieure, 100% hauteur dédiée au PDF)
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(18, 20, 26);
        ctx.set_visuals(visuals);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(18, 20, 26)))
            .show(ctx, |ui| {
                let canvas_rect = ui.max_rect();
                let _response = ui.allocate_rect(canvas_rect, Sense::click_and_drag());

                self.handle_inputs(ctx, canvas_rect);

                if self.viewport.is_panning {
                    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
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
                } else if let Some(doc) = &self.document {
                    let viewport_size = canvas_rect.size();
                    let (layouts, _, total_h) = self.viewport.compute_layout(&doc.page_dimensions);
                    let bucket = Self::zoom_bucket(self.viewport.zoom);
                    let bucket_zoom = (bucket as f32) / 20.0;

                    // Virtualisation du rendu : seules les pages qui intersectent le viewport (+ marge de 200px) sont dessinées
                    for layout in &layouts {
                        let page_rect = self.viewport.page_screen_rect(layout, canvas_rect);
                        let expanded_canvas = canvas_rect.expand(200.0);
                        if !expanded_canvas.intersects(page_rect) {
                            continue;
                        }

                        // Ombre portée douce sous la feuille PDF
                        let shadow_rect = page_rect.translate(Vec2::new(0.0, 4.0)).expand(2.0);
                        painter.rect_filled(
                            shadow_rect,
                            CornerRadius::same(3),
                            Color32::from_rgba_premultiplied(0, 0, 0, 110),
                        );

                        // Fond blanc de la feuille
                        painter.rect_filled(page_rect, CornerRadius::same(2), Color32::WHITE);

                        let key = PageTextureKey {
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
                                    format!("pdf_p{}_z{}", layout.page_index, bucket),
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

                        // Bordure subtile autour de la feuille
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
                        let scroll_progress = (self.viewport.scroll_offset.y
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

                // HUD Flottant Compact en bas à droite (Zéro perte de hauteur d'écran en haut !)
                self.render_floating_hud(ui, canvas_rect);
            });

        // Déclenchement automatique de la capture d'écran après stabilisation du rendu GPU (frame 4)
        if self.automation.screenshot_output.is_some() {
            if self.frame_count < 4 {
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
    fn render_floating_hud(&mut self, ui: &mut egui::Ui, canvas_rect: Rect) {
        let Some(doc) = &self.document else {
            return;
        };
        let pages = doc.page_dimensions.clone();
        let page_count = pages.len();
        let viewport_size = canvas_rect.size();
        let current_page = self.viewport.current_page_index(viewport_size, &pages) + 1;
        let title = doc.title.clone();

        let hud_rect = Rect::from_min_size(
            Pos2::new(
                (canvas_rect.max.x - 650.0).max(16.0),
                canvas_rect.max.y - 46.0,
            ),
            Vec2::new(634.0, 34.0),
        );

        let mut open_clicked = false;

        egui::Area::new(egui::Id::new("floating_hud_jalon1"))
            .fixed_pos(hud_rect.min)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(24, 28, 38, 235))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(58, 68, 90)))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(Margin::symmetric(14, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;

                            if ui.small_button("📂 Ouvrir").clicked() {
                                open_clicked = true;
                            }

                            ui.colored_label(
                                Color32::from_rgb(180, 200, 240),
                                format!("📄 {}", title),
                            );
                            ui.separator();

                            // Navigation Pages
                            if ui
                                .add_enabled(current_page > 1, egui::Button::new("◀").small())
                                .clicked()
                            {
                                self.viewport
                                    .jump_to_page(current_page - 2, viewport_size, &pages);
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "Page {} / {}",
                                    current_page, page_count
                                ))
                                .strong()
                                .color(Color32::WHITE),
                            );
                            if ui
                                .add_enabled(
                                    current_page < page_count,
                                    egui::Button::new("▶").small(),
                                )
                                .clicked()
                            {
                                self.viewport
                                    .jump_to_page(current_page, viewport_size, &pages);
                            }

                            ui.separator();

                            // Contrôles de Zoom
                            if ui.small_button("➖").clicked() {
                                self.viewport.apply_zoom_around_anchor(
                                    1.0 / 1.2,
                                    viewport_size * 0.5,
                                    viewport_size,
                                    &pages,
                                );
                            }
                            ui.label(
                                egui::RichText::new(format!("{:.0}%", self.viewport.zoom * 100.0))
                                    .monospace()
                                    .color(Color32::from_rgb(130, 220, 170)),
                            );
                            if ui.small_button("➕").clicked() {
                                self.viewport.apply_zoom_around_anchor(
                                    1.2,
                                    viewport_size * 0.5,
                                    viewport_size,
                                    &pages,
                                );
                            }
                            if ui
                                .small_button("↔ Largeur")
                                .on_hover_text("Ajuster à la largeur (Ctrl+9)")
                                .clicked()
                            {
                                self.viewport.fit_to_width(viewport_size, &pages);
                            }
                            if ui
                                .small_button("↕ Page")
                                .on_hover_text("Ajuster à la page entière (Ctrl+0)")
                                .clicked()
                            {
                                self.viewport.fit_to_page(viewport_size, &pages);
                            }

                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "⚡ {:.0} FPS | 🖱 {}",
                                    self.fps_smoothed, self.viewport.last_gesture_label
                                ))
                                .size(11.0)
                                .color(Color32::from_rgb(155, 165, 185)),
                            );
                        });
                    });
            });

        if open_clicked {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Document PDF", &["pdf"])
                .pick_file()
            {
                self.open_pdf_file(&path);
            }
        }
    }
}
