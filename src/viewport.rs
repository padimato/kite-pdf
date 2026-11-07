use crate::pdf_engine::PageDimensions;
use egui::{Pos2, Rect, Vec2};

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 5.0;
pub const PAGE_GAP_PX: f32 = 18.0;
/// Facteur de conversion Points PDF (72 DPI) -> Pixels écran à zoom 100% (96 DPI = 1.3333)
pub const BASE_DPI_SCALE: f32 = 96.0 / 72.0;

/// Position calculée d'une page dans l'espace du document déroulant (en pixels à la taille courante de zoom)
#[derive(Debug, Clone, Copy)]
pub struct PageLayoutRect {
    pub page_index: usize,
    /// Coordonnée Y du haut de la page dans l'espace virtuel du document (px)
    pub y_top: f32,
    /// Largeur affichée (px)
    pub width: f32,
    /// Hauteur affichée (px)
    pub height: f32,
}

impl PageLayoutRect {
    pub fn y_bottom(&self) -> f32 {
        self.y_top + self.height
    }
}

/// État de navigation, zoom et gestionnaire Trackpad Windows Precision pour un onglet PDF
#[derive(Debug, Clone)]
pub struct ViewportState {
    /// Facteur de zoom courant (1.0 = 100%)
    pub zoom: f32,
    /// Décalage de défilement (X = panoramique horizontal, Y = défilement vertical depuis le haut du document)
    pub scroll_offset: Vec2,
    /// Vélocité d'inertie pour le défilement au trackpad
    pub inertia_velocity: Vec2,
    /// Indique si le mode panoramique à 1 doigt (barre d'espace ou clic milieu) est actif
    pub is_panning: bool,
    /// Dernier événement trackpad détecté (pour le HUD de diagnostic)
    pub last_gesture_label: String,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            scroll_offset: Vec2::ZERO,
            inertia_velocity: Vec2::ZERO,
            is_panning: false,
            last_gesture_label: "Prêt (Trackpad 2D + Pinch-to-Zoom)".to_string(),
        }
    }
}

impl ViewportState {
    /// Calcule la géométrie verticale de toutes les pages du document au niveau de zoom courant.
    pub fn compute_layout(&self, pages: &[PageDimensions]) -> (Vec<PageLayoutRect>, f32, f32) {
        let mut layouts = Vec::with_capacity(pages.len());
        let mut current_y = PAGE_GAP_PX;
        let mut max_width: f32 = 0.0;

        for (idx, dim) in pages.iter().enumerate() {
            let w = dim.width_pt * BASE_DPI_SCALE * self.zoom;
            let h = dim.height_pt * BASE_DPI_SCALE * self.zoom;
            max_width = max_width.max(w);

            layouts.push(PageLayoutRect {
                page_index: idx,
                y_top: current_y,
                width: w,
                height: h,
            });
            current_y += h + PAGE_GAP_PX;
        }

        (layouts, max_width, current_y)
    }

    /// Applique un zoom centré exactement sur la position `anchor_in_viewport` (curseur ou centre des 2 doigts sur le trackpad).
    pub fn apply_zoom_around_anchor(
        &mut self,
        zoom_factor: f32,
        anchor_in_viewport: Vec2,
        viewport_size: Vec2,
        pages: &[PageDimensions],
    ) {
        let old_zoom = self.zoom;
        let new_zoom = (old_zoom * zoom_factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old_zoom).abs() < 1e-5 {
            return;
        }

        let scale_ratio = new_zoom / old_zoom;

        // Point du document sous le pointeur avant le zoom : (scroll_offset + anchor)
        // Après le zoom, on ajuste scroll_offset pour que ce même point reste exactement sous `anchor_in_viewport`
        self.scroll_offset.y =
            (self.scroll_offset.y + anchor_in_viewport.y) * scale_ratio - anchor_in_viewport.y;
        self.scroll_offset.x = (self.scroll_offset.x + anchor_in_viewport.x
            - viewport_size.x * 0.5)
            * scale_ratio
            - (anchor_in_viewport.x - viewport_size.x * 0.5);

        self.zoom = new_zoom;
        self.clamp_scroll(viewport_size, pages);
    }

    /// Applique un défilement 2D fluide (Trackpad 2 doigts vertical + horizontal ou panoramique)
    pub fn apply_pan_delta(&mut self, delta: Vec2, viewport_size: Vec2, pages: &[PageDimensions]) {
        // Sur un trackpad Windows Precision, `smooth_scroll_delta` est positif quand on glisse vers le bas/droite,
        // ce qui diminue l'offset de scroll.
        self.scroll_offset.x -= delta.x;
        self.scroll_offset.y -= delta.y;
        self.clamp_scroll(viewport_size, pages);
    }

    /// Contraint `scroll_offset` dans les limites valides du document pour éviter de perdre la page hors écran.
    pub fn clamp_scroll(&mut self, viewport_size: Vec2, pages: &[PageDimensions]) {
        if pages.is_empty() {
            self.scroll_offset = Vec2::ZERO;
            return;
        }
        let (_, max_page_w, total_h) = self.compute_layout(pages);

        let max_scroll_y = (total_h - viewport_size.y).max(0.0);
        self.scroll_offset.y = self.scroll_offset.y.clamp(0.0, max_scroll_y);

        // En horizontal : si la page zoomée dépasse la largeur de la vue, on autorise le pan horizontal
        let horizontal_overflow = (max_page_w + 64.0 - viewport_size.x).max(0.0) * 0.5;
        self.scroll_offset.x = self
            .scroll_offset
            .x
            .clamp(-horizontal_overflow, horizontal_overflow);
    }

    /// Ajuste le zoom pour que la page courante occupe la largeur disponible ("Fit Width")
    pub fn fit_to_width(&mut self, viewport_size: Vec2, pages: &[PageDimensions]) {
        if let Some(first) = pages.first() {
            let base_w = first.width_pt * BASE_DPI_SCALE;
            if base_w > 0.0 {
                let target_w = (viewport_size.x - 64.0).max(200.0);
                self.zoom = (target_w / base_w).clamp(MIN_ZOOM, MAX_ZOOM);
                self.scroll_offset.x = 0.0;
                self.clamp_scroll(viewport_size, pages);
                self.last_gesture_label = format!("Ajusté à la largeur ({:.0}%)", self.zoom * 100.0);
            }
        }
    }

    /// Ajuste le zoom pour qu'une page entière tienne en hauteur dans la fenêtre ("Fit Page")
    pub fn fit_to_page(&mut self, viewport_size: Vec2, pages: &[PageDimensions]) {
        if let Some(first) = pages.first() {
            let base_h = first.height_pt * BASE_DPI_SCALE;
            if base_h > 0.0 {
                let target_h = (viewport_size.y - 40.0).max(200.0);
                self.zoom = (target_h / base_h).clamp(MIN_ZOOM, MAX_ZOOM);
                self.scroll_offset.x = 0.0;
                self.clamp_scroll(viewport_size, pages);
                self.last_gesture_label = format!("Ajusté à la page ({:.0}%)", self.zoom * 100.0);
            }
        }
    }

    /// Saute directement au début d'une page donnée (0-indexée)
    pub fn jump_to_page(
        &mut self,
        page_index: usize,
        viewport_size: Vec2,
        pages: &[PageDimensions],
    ) {
        let (layouts, _, _) = self.compute_layout(pages);
        if let Some(target) = layouts.get(page_index) {
            self.scroll_offset.y = (target.y_top - PAGE_GAP_PX * 0.5).max(0.0);
            self.clamp_scroll(viewport_size, pages);
            self.last_gesture_label = format!("Saut vers Page {}", page_index + 1);
        }
    }

    /// Retourne l'index (0-based) de la page actuellement au centre vertical du viewport
    pub fn current_page_index(&self, viewport_size: Vec2, pages: &[PageDimensions]) -> usize {
        if pages.is_empty() {
            return 0;
        }
        let (layouts, _, _) = self.compute_layout(pages);
        let center_y = self.scroll_offset.y + viewport_size.y * 0.5;

        for layout in &layouts {
            if center_y >= layout.y_top && center_y <= layout.y_bottom() + PAGE_GAP_PX {
                return layout.page_index;
            }
        }
        layouts.last().map(|l| l.page_index).unwrap_or(0)
    }

    /// Calcule le rectangle écran (`egui::Rect`) où une page doit être dessinée dans `canvas_rect`
    pub fn page_screen_rect(&self, layout: &PageLayoutRect, canvas_rect: Rect) -> Rect {
        let center_x = canvas_rect.center().x - self.scroll_offset.x;
        let x_left = center_x - layout.width * 0.5;
        let y_top = canvas_rect.min.y + layout.y_top - self.scroll_offset.y;

        Rect::from_min_size(Pos2::new(x_left, y_top), Vec2::new(layout.width, layout.height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pages() -> Vec<PageDimensions> {
        vec![
            PageDimensions {
                width_pt: 595.0,
                height_pt: 842.0,
            };
            5
        ]
    }

    #[test]
    fn test_trackpad_pinch_zoom_preserves_cursor_anchor() {
        let pages = sample_pages();
        let viewport_size = Vec2::new(1280.0, 800.0);
        let mut vp = ViewportState::default();
        vp.scroll_offset = Vec2::new(0.0, 300.0);

        let cursor_anchor = Vec2::new(640.0, 400.0);
        // Point vertical dans le document sous le curseur avant le zoom (en coordonnées normalisées / zoom)
        let doc_y_before = (vp.scroll_offset.y + cursor_anchor.y) / vp.zoom;

        vp.apply_zoom_around_anchor(1.5, cursor_anchor, viewport_size, &pages);
        assert!((vp.zoom - 1.5).abs() < 1e-4);

        let doc_y_after = (vp.scroll_offset.y + cursor_anchor.y) / vp.zoom;
        assert!(
            (doc_y_before - doc_y_after).abs() < 1e-2,
            "Le point sous les doigts du trackpad doit rester fixe pendant un pinch-to-zoom ({} vs {})",
            doc_y_before,
            doc_y_after
        );
    }

    #[test]
    fn test_trackpad_2d_scroll_vertical_and_horizontal() {
        let pages = sample_pages();
        let viewport_size = Vec2::new(800.0, 600.0);
        let mut vp = ViewportState::default();
        // On zoome à 250% pour que la page soit plus large que l'écran et permette le scroll horizontal
        vp.zoom = 2.5;
        vp.clamp_scroll(viewport_size, &pages);

        // Simulation d'un glissement 2 doigts sur le trackpad vers le haut/gauche (scroll vers le bas et la droite)
        vp.apply_pan_delta(Vec2::new(-120.0, -250.0), viewport_size, &pages);

        assert!(vp.scroll_offset.x > 100.0, "Le pan horizontal trackpad doit fonctionner quand on est zoomé");
        assert!((vp.scroll_offset.y - 250.0).abs() < 1e-2, "Le scroll vertical trackpad doit être fluide au pixel près");
    }

    #[test]
    fn test_jump_to_page_and_current_page_detection() {
        let pages = sample_pages();
        let viewport_size = Vec2::new(1200.0, 800.0);
        let mut vp = ViewportState::default();

        assert_eq!(vp.current_page_index(viewport_size, &pages), 0);
        vp.jump_to_page(3, viewport_size, &pages);
        assert_eq!(vp.current_page_index(viewport_size, &pages), 3);
    }
}
