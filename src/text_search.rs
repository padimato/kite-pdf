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
            x_min: ((screen_rect.min.x - page_screen_rect.min.x) * sx).clamp(0.0, page_dim.width_pt),
            y_min: ((screen_rect.min.y - page_screen_rect.min.y) * sy)
                .clamp(0.0, page_dim.height_pt),
            x_max: ((screen_rect.max.x - page_screen_rect.min.x) * sx).clamp(0.0, page_dim.width_pt),
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
        let normalized_chars = chars
            .iter()
            .map(|c| normalize_char(c.ch))
            .collect();
        Self {
            page_index,
            chars,
            normalized_chars,
        }
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
                let same_line_y = (last.y_min - pc.rect.y_min).abs() < 12.0;
                let close_x = (pc.rect.x_min - last.x_max).abs() < 24.0;
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

    /// Sélectionne les caractères dont le centre tombe dans le rectangle `selection_rect` (en points PDF top-down)
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

/// État de la sélection de texte à la souris / Trackpad sur une page
#[derive(Debug, Clone, Default)]
pub struct TextSelectionState {
    pub page_index: Option<usize>,
    pub drag_start_pt: Option<(f32, f32)>,
    pub drag_current_pt: Option<(f32, f32)>,
    pub selected_text: String,
    pub highlight_boxes: Vec<PdfTopDownRect>,
    pub copied_feedback_frames: u32,
}

impl TextSelectionState {
    pub fn clear(&mut self) {
        self.page_index = None;
        self.drag_start_pt = None;
        self.drag_current_pt = None;
        self.selected_text.clear();
        self.highlight_boxes.clear();
    }

    pub fn has_selection(&self) -> bool {
        !self.selected_text.is_empty() && !self.highlight_boxes.is_empty()
    }

    pub fn active_drag_rect(&self) -> Option<PdfTopDownRect> {
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

    /// Exécute la recherche sur l'ensemble des couches de texte des pages du document
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
