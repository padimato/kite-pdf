use crate::pdf_engine::PageDimensions;
use egui::{Pos2, Rect};
use std::ops::Range;

/// Rectangle en points PDF mesuré depuis le coin SUPÉRIEUR gauche de la page (0..width_pt, 0..height_pt)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfTopDownRect {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub angle_deg: f32,
}

impl PdfTopDownRect {
    pub fn width(&self) -> f32 {
        (self.x_max - self.x_min).max(0.0)
    }

    pub fn height(&self) -> f32 {
        (self.y_max - self.y_min).max(0.0)
    }

    pub fn center(&self) -> (f32, f32) {
        ((self.x_min + self.x_max) * 0.5, (self.y_min + self.y_max) * 0.5)
    }

    pub fn contains_pt(&self, x: f32, y: f32, margin: f32) -> bool {
        x >= self.x_min - margin
            && x <= self.x_max + margin
            && y >= self.y_min - margin
            && y <= self.y_max + margin
    }

    #[allow(dead_code)]
    pub fn intersects(&self, other: &PdfTopDownRect) -> bool {
        self.x_min <= other.x_max
            && self.x_max >= other.x_min
            && self.y_min <= other.y_max
            && self.y_max >= other.y_min
    }

    pub fn union(&self, other: &PdfTopDownRect) -> PdfTopDownRect {
        PdfTopDownRect {
            x_min: self.x_min.min(other.x_min),
            y_min: self.y_min.min(other.y_min),
            x_max: self.x_max.max(other.x_max),
            y_max: self.y_max.max(other.y_max),
            angle_deg: self.angle_deg,
        }
    }

    pub fn translate(&self, dx: f32, dy: f32) -> PdfTopDownRect {
        PdfTopDownRect {
            x_min: self.x_min + dx,
            y_min: self.y_min + dy,
            x_max: self.x_max + dx,
            y_max: self.y_max + dy,
            angle_deg: self.angle_deg,
        }
    }

    /// Convertit ce rectangle (en points PDF top-down) en rectangle écran (`egui::Rect`)
    pub fn to_screen_rect(&self, page_screen_rect: Rect, page_dim: PageDimensions) -> Rect {
        let sx = page_screen_rect.width() / page_dim.width_pt.max(1.0);
        let sy = page_screen_rect.height() / page_dim.height_pt.max(1.0);

        Rect::from_min_max(
            Pos2::new(
                page_screen_rect.min.x + self.x_min * sx,
                page_screen_rect.min.y + self.y_min * sy,
            ),
            Pos2::new(
                page_screen_rect.min.x + self.x_max * sx,
                page_screen_rect.min.y + self.y_max * sy,
            ),
        )
    }

    /// Convertit un rectangle écran (`egui::Rect`) en coordonnées points PDF top-down
    #[allow(dead_code)]
    pub fn from_screen_rect(
        screen_rect: Rect,
        page_screen_rect: Rect,
        page_dim: PageDimensions,
    ) -> Self {
        let sx = page_dim.width_pt / page_screen_rect.width().max(1.0);
        let sy = page_dim.height_pt / page_screen_rect.height().max(1.0);

        Self {
            x_min: ((screen_rect.min.x - page_screen_rect.min.x) * sx)
                .clamp(0.0, page_dim.width_pt),
            y_min: ((screen_rect.min.y - page_screen_rect.min.y) * sy)
                .clamp(0.0, page_dim.height_pt),
            x_max: ((screen_rect.max.x - page_screen_rect.min.x) * sx)
                .clamp(0.0, page_dim.width_pt),
            y_max: ((screen_rect.max.y - page_screen_rect.min.y) * sy)
                .clamp(0.0, page_dim.height_pt),
            angle_deg: 0.0,
        }
    }
}

/// Caractère individuel extrait d'une page PDF (natif ou OCR)
#[derive(Debug, Clone)]
pub struct PositionedChar {
    pub ch: char,
    pub rect: PdfTopDownRect,
    pub is_ocr: bool,
}

/// Couche de texte complète d'une page PDF (stockée en mémoire légère même lorsque l'onglet est hiberné)
#[derive(Debug, Clone)]
pub struct PageTextLayer {
    pub page_index: usize,
    pub chars: Vec<PositionedChar>,
    /// Texte normalisé (un `char` par entrée dans `chars`) pour recherche rapide par indice de caractère
    pub normalized_chars: Vec<char>,
}

impl PageTextLayer {
    pub fn new(page_index: usize, chars: Vec<PositionedChar>) -> Self {
        let normalized_chars = chars.iter().map(|c| normalize_char(c.ch)).collect();
        Self {
            page_index,
            chars,
            normalized_chars,
        }
    }

    /// Trouve l'index du caractère situé sous (ou à proximité immédiate de) `(x_pt, y_pt)`.
    /// Utilisé pour afficher le curseur texte `I` (`CursorIcon::Text`) et pour la sélection au fil des phrases.
    pub fn char_index_near_point(&self, x_pt: f32, y_pt: f32, max_dist_pt: f32) -> Option<usize> {
        let mut best_idx = None;
        let mut best_dist_sq = max_dist_pt * max_dist_pt;

        for (i, pc) in self.chars.iter().enumerate() {
            if pc.rect.width() < 0.1 && pc.rect.height() < 0.1 {
                continue;
            }
            if pc.rect.contains_pt(x_pt, y_pt, 2.0) {
                return Some(i);
            }
            let (cx, cy) = pc.rect.center();
            let dx = cx - x_pt;
            let dy = (cy - y_pt) * 1.6; // Pondération verticale pour privilégier la ligne courante
            let d2 = dx * dx + dy * dy;
            if d2 < best_dist_sq {
                best_dist_sq = d2;
                best_idx = Some(i);
            }
        }
        best_idx
    }

    /// Sélectionne un flux continu de caractères au fil des phrases (du caractère `idx_a` au caractère `idx_b`),
    /// exactement comme un curseur texte `I` dans Adobe Acrobat ou Word.
    pub fn select_char_stream(&self, idx_a: usize, idx_b: usize) -> (String, Vec<PdfTopDownRect>) {
        if self.chars.is_empty() {
            return (String::new(), Vec::new());
        }
        let start = idx_a.min(idx_b).min(self.chars.len() - 1);
        let end = idx_a.max(idx_b).min(self.chars.len() - 1);

        let mut text = String::with_capacity(end - start + 1);
        for i in start..=end {
            let c = self.chars[i].ch;
            if c == '\r' {
                continue;
            }
            text.push(c);
        }

        let boxes = self.merge_highlight_boxes(start..(end + 1));
        (text.trim().to_string(), boxes)
    }

    /// Sélectionne le mot complet sous le point `(x_pt, y_pt)` lors d'un double-clic au Trackpad / Souris.
    pub fn select_word_at_point(&self, x_pt: f32, y_pt: f32) -> Option<(usize, usize, String, Vec<PdfTopDownRect>)> {
        let pivot = self.char_index_near_point(x_pt, y_pt, 14.0)?;
        let is_word_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-' || c == '+';
        if !is_word_char(self.chars[pivot].ch) {
            return None;
        }

        let mut start = pivot;
        while start > 0 && is_word_char(self.chars[start - 1].ch) {
            start -= 1;
        }
        let mut end = pivot;
        while end + 1 < self.chars.len() && is_word_char(self.chars[end + 1].ch) {
            end += 1;
        }

        let (txt, boxes) = self.select_char_stream(start, end);
        Some((start, end, txt, boxes))
    }

    /// Fusionne une plage de caractères consécutifs en boîtes de surbrillance par ligne/orientation
    pub fn merge_highlight_boxes(&self, range: Range<usize>) -> Vec<PdfTopDownRect> {
        let mut boxes: Vec<PdfTopDownRect> = Vec::new();
        for idx in range {
            let Some(pc) = self.chars.get(idx) else {
                continue;
            };
            if pc.ch.is_whitespace() || pc.rect.width() < 0.2 {
                continue;
            }
            if let Some(last) = boxes.last_mut() {
                let same_angle = (last.angle_deg - pc.rect.angle_deg).abs() < 8.0;
                let same_line_y = (last.y_min - pc.rect.y_min).abs() < 10.0;
                let close_x = (pc.rect.x_min - last.x_max).abs() < 24.0 && pc.rect.x_min >= last.x_min - 4.0;
                if same_angle && (same_line_y || last.angle_deg.abs() > 5.0) && close_x {
                    *last = last.union(&pc.rect);
                    continue;
                }
            }
            boxes.push(pc.rect);
        }
        boxes
    }

    /// Construit un court extrait de contexte autour de la plage `range`
    pub fn build_snippet(&self, range: &Range<usize>) -> String {
        let start = range.start.saturating_sub(18);
        let end = (range.end + 22).min(self.chars.len());
        let mut s = String::with_capacity(end.saturating_sub(start) + 6);
        if start > 0 {
            s.push_str("...");
        }
        for i in start..end {
            let c = self.chars[i].ch;
            if c == '\r' || c == '\n' {
                s.push(' ');
            } else {
                s.push(c);
            }
        }
        if end < self.chars.len() {
            s.push_str("...");
        }
        s.trim().to_string()
    }

    /// Sélectionne les caractères dont le centre tombe dans le rectangle `selection_rect` (`Alt + Glisser` pour tableaux/graphiques)
    pub fn select_in_rect(&self, selection_rect: &PdfTopDownRect) -> (String, Vec<PdfTopDownRect>) {
        let mut selected_indices = Vec::new();
        for (i, pc) in self.chars.iter().enumerate() {
            let (cx, cy) = pc.rect.center();
            if cx >= selection_rect.x_min
                && cx <= selection_rect.x_max
                && cy >= selection_rect.y_min
                && cy <= selection_rect.y_max
            {
                selected_indices.push(i);
            }
        }

        if selected_indices.is_empty() {
            return (String::new(), Vec::new());
        }

        let mut text = String::with_capacity(selected_indices.len());
        let mut boxes: Vec<PdfTopDownRect> = Vec::new();
        let mut prev_idx: Option<usize> = None;

        for idx in selected_indices {
            if let Some(p) = prev_idx {
                if idx > p + 1 {
                    text.push(' ');
                }
            }
            let pc = &self.chars[idx];
            text.push(pc.ch);
            if !pc.ch.is_whitespace() && pc.rect.width() > 0.2 {
                if let Some(last) = boxes.last_mut() {
                    let same_line = (last.y_min - pc.rect.y_min).abs() < 10.0
                        && (pc.rect.x_min - last.x_max).abs() < 18.0;
                    if same_line {
                        *last = last.union(&pc.rect);
                    } else {
                        boxes.push(pc.rect);
                    }
                } else {
                    boxes.push(pc.rect);
                }
            }
            prev_idx = Some(idx);
        }

        (text.trim().to_string(), boxes)
    }
}

/// Normalise un caractère en minuscule sans accent pour une recherche tolérante
pub fn normalize_char(c: char) -> char {
    match c.to_ascii_lowercase() {
        'à' | 'â' | 'ä' | 'á' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'î' | 'ï' | 'í' => 'i',
        'ô' | 'ö' | 'ó' => 'o',
        'ù' | 'û' | 'ü' | 'ú' => 'u',
        'ç' => 'c',
        other => other,
    }
}

/// Occurrence trouvée par la recherche `Ctrl+F`
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub page_index: usize,
    pub snippet: String,
    pub highlight_boxes: Vec<PdfTopDownRect>,
    pub angle_deg: f32,
    #[allow(dead_code)]
    pub is_ocr: bool,
}

/// Mode de sélection de texte : Au fil des phrases (Curseur `I` par défaut) ou Bloc rectangulaire (`Alt + Glisser`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionGestureMode {
    #[default]
    StreamIBeam,
    RectangularBlock,
}

/// État de la sélection de texte à la souris / Trackpad sur une page
#[derive(Debug, Clone, Default)]
pub struct TextSelectionState {
    pub page_index: Option<usize>,
    pub mode: SelectionGestureMode,
    pub anchor_char_index: Option<usize>,
    pub current_char_index: Option<usize>,
    pub drag_start_pt: Option<(f32, f32)>,
    pub drag_current_pt: Option<(f32, f32)>,
    pub selected_text: String,
    pub highlight_boxes: Vec<PdfTopDownRect>,
    pub copied_feedback_frames: u32,
}

impl TextSelectionState {
    pub fn clear(&mut self) {
        self.page_index = None;
        self.anchor_char_index = None;
        self.current_char_index = None;
        self.drag_start_pt = None;
        self.drag_current_pt = None;
        self.selected_text.clear();
        self.highlight_boxes.clear();
    }

    pub fn has_selection(&self) -> bool {
        !self.selected_text.is_empty() && !self.highlight_boxes.is_empty()
    }

    pub fn active_drag_rect(&self) -> Option<PdfTopDownRect> {
        if self.mode != SelectionGestureMode::RectangularBlock {
            return None;
        }
        let (x0, y0) = self.drag_start_pt?;
        let (x1, y1) = self.drag_current_pt?;
        Some(PdfTopDownRect {
            x_min: x0.min(x1),
            y_min: y0.min(y1),
            x_max: x0.max(x1),
            y_max: y0.max(y1),
            angle_deg: 0.0,
        })
    }
}

/// État du moteur de recherche `Ctrl+F`
#[derive(Debug, Clone, Default)]
pub struct SearchBarState {
    pub is_open: bool,
    pub query: String,
    pub last_executed_query: String,
    pub matches: Vec<SearchMatch>,
    pub current_match_index: usize,
    pub focus_requested: bool,
}

impl SearchBarState {
    pub fn open(&mut self) {
        self.is_open = true;
        self.focus_requested = true;
    }

    pub fn close(&mut self) {
        self.is_open = false;
        self.focus_requested = false;
    }

    /// Relance la recherche quand de nouvelles pages arrivent, sans perdre l'occurrence courante.
    pub fn refresh_keeping_current(&mut self, pages_text: &[PageTextLayer]) {
        let anchor = self
            .matches
            .get(self.current_match_index)
            .and_then(|m| m.highlight_boxes.first().map(|b| (m.page_index, b.x_min, b.y_min)));
        self.execute_search(pages_text);
        if let Some((page, x, y)) = anchor {
            self.current_match_index = self
                .matches
                .iter()
                .position(|m| {
                    m.page_index == page
                        && m.highlight_boxes
                            .first()
                            .is_some_and(|b| (b.x_min - x).abs() < 0.5 && (b.y_min - y).abs() < 0.5)
                })
                .unwrap_or(0);
        }
    }

    pub fn execute_search(&mut self, pages_text: &[PageTextLayer]) {
        self.last_executed_query = self.query.clone();
        self.matches.clear();
        self.current_match_index = 0;

        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            return;
        }

        let needle: Vec<char> = trimmed.chars().map(normalize_char).collect();
        let n_len = needle.len();
        if n_len == 0 {
            return;
        }

        for layer in pages_text {
            if layer.normalized_chars.len() < n_len {
                continue;
            }
            let mut i = 0;
            while i + n_len <= layer.normalized_chars.len() {
                if layer.normalized_chars[i..i + n_len] == needle[..] {
                    let range = i..(i + n_len);
                    let boxes = layer.merge_highlight_boxes(range.clone());
                    let angle_deg = boxes.first().map(|b| b.angle_deg).unwrap_or(0.0);
                    let is_ocr = layer.chars[range.clone()].iter().any(|c| c.is_ocr);
                    let snippet = layer.build_snippet(&range);

                    if !boxes.is_empty() {
                        self.matches.push(SearchMatch {
                            page_index: layer.page_index,
                            snippet,
                            highlight_boxes: boxes,
                            angle_deg,
                            is_ocr,
                        });
                    }
                    i += n_len;
                } else {
                    i += 1;
                }
            }
        }
    }

    pub fn next_match(&mut self) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            return None;
        }
        self.current_match_index = (self.current_match_index + 1) % self.matches.len();
        self.matches.get(self.current_match_index)
    }

    pub fn prev_match(&mut self) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            return None;
        }
        self.current_match_index =
            (self.current_match_index + self.matches.len() - 1) % self.matches.len();
        self.matches.get(self.current_match_index)
    }
}
