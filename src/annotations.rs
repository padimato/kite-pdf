use crate::ocr_engine::{append_invisible_ocr_layer_to_doc, encode_win_ansi};
use crate::pdf_engine::PageDimensions;
use crate::text_search::{PageTextLayer, PdfTopDownRect};
use anyhow::{Context, Result};
use egui::Color32;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::path::Path;

/// Styles d'extrémité d'une ligne (`/LE`, ISO 32000-1 Table 176)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    None,
    OpenArrow,
    ClosedArrow,
    Circle,
    Square,
    Diamond,
}

impl LineEnding {
    pub const ALL: [LineEnding; 6] = [
        LineEnding::None,
        LineEnding::OpenArrow,
        LineEnding::ClosedArrow,
        LineEnding::Circle,
        LineEnding::Square,
        LineEnding::Diamond,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::None => "Aucun",
            LineEnding::OpenArrow => "Flèche",
            LineEnding::ClosedArrow => "Pleine",
            LineEnding::Circle => "Cercle",
            LineEnding::Square => "Carré",
            LineEnding::Diamond => "Losange",
        }
    }

    fn pdf_name(self) -> &'static [u8] {
        match self {
            LineEnding::None => b"None",
            LineEnding::OpenArrow => b"OpenArrow",
            LineEnding::ClosedArrow => b"ClosedArrow",
            LineEnding::Circle => b"Circle",
            LineEnding::Square => b"Square",
            LineEnding::Diamond => b"Diamond",
        }
    }

    fn from_pdf_name(name: &str) -> LineEnding {
        match name {
            "OpenArrow" | "ROpenArrow" => LineEnding::OpenArrow,
            "ClosedArrow" | "RClosedArrow" => LineEnding::ClosedArrow,
            "Circle" => LineEnding::Circle,
            "Square" => LineEnding::Square,
            "Diamond" => LineEnding::Diamond,
            _ => LineEnding::None,
        }
    }
}

const ARROW_HEAD_LENGTH: f32 = 11.0;
const ARROW_HEAD_HALF_WIDTH: f32 = 5.5;
const ENDING_RADIUS: f32 = 4.5;
pub const ARROW_LINE_WIDTH: f32 = 2.2;

/// Contour d'une extrémité de ligne, en points PDF top-down.
pub struct EndingOutline {
    pub points: Vec<(f32, f32)>,
    pub filled: bool,
}

/// Géométrie d'une extrémité posée sur `tip`, la ligne arrivant selon la direction unitaire `dir`.
pub fn line_ending_outline(
    ending: LineEnding,
    tip: (f32, f32),
    dir: (f32, f32),
) -> Option<EndingOutline> {
    let perp = (-dir.1, dir.0);
    let at = |along: f32, across: f32| {
        (
            tip.0 + dir.0 * along + perp.0 * across,
            tip.1 + dir.1 * along + perp.1 * across,
        )
    };
    let base = -ARROW_HEAD_LENGTH;
    match ending {
        LineEnding::None => None,
        LineEnding::OpenArrow => Some(EndingOutline {
            points: vec![
                at(base, ARROW_HEAD_HALF_WIDTH),
                tip,
                at(base, -ARROW_HEAD_HALF_WIDTH),
            ],
            filled: false,
        }),
        LineEnding::ClosedArrow => Some(EndingOutline {
            points: vec![
                tip,
                at(base, ARROW_HEAD_HALF_WIDTH),
                at(base, -ARROW_HEAD_HALF_WIDTH),
            ],
            filled: true,
        }),
        LineEnding::Circle => Some(EndingOutline {
            points: (0..18)
                .map(|i| {
                    let a = i as f32 / 18.0 * std::f32::consts::TAU;
                    at(a.cos() * ENDING_RADIUS, a.sin() * ENDING_RADIUS)
                })
                .collect(),
            filled: true,
        }),
        LineEnding::Square => Some(EndingOutline {
            points: vec![
                at(ENDING_RADIUS, ENDING_RADIUS),
                at(ENDING_RADIUS, -ENDING_RADIUS),
                at(-ENDING_RADIUS, -ENDING_RADIUS),
                at(-ENDING_RADIUS, ENDING_RADIUS),
            ],
            filled: true,
        }),
        LineEnding::Diamond => Some(EndingOutline {
            points: vec![
                at(ENDING_RADIUS * 1.4, 0.0),
                at(0.0, ENDING_RADIUS * 1.4),
                at(-ENDING_RADIUS * 1.4, 0.0),
                at(0.0, -ENDING_RADIUS * 1.4),
            ],
            filled: true,
        }),
    }
}

/// Direction unitaire de `from` vers `to` (ou axe X si les points sont confondus).
pub fn unit_direction(from: (f32, f32), to: (f32, f32)) -> (f32, f32) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len = dx.hypot(dy);
    if len < 1e-3 {
        (1.0, 0.0)
    } else {
        (dx / len, dy / len)
    }
}

/// Point où le trait s'arrête : il ne doit pas dépasser de la pointe d'une flèche pleine.
pub fn line_stop_point(tip: (f32, f32), dir: (f32, f32), ending: LineEnding) -> (f32, f32) {
    if ending == LineEnding::ClosedArrow {
        (
            tip.0 - dir.0 * ARROW_HEAD_LENGTH * 0.9,
            tip.1 - dir.1 * ARROW_HEAD_LENGTH * 0.9,
        )
    } else {
        tip
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle,
    ArrowLine {
        start_head: LineEnding,
        end_head: LineEnding,
    },
}

impl ShapeKind {
    pub const DEFAULT_ARROW: ShapeKind = ShapeKind::ArrowLine {
        start_head: LineEnding::None,
        end_head: LineEnding::ClosedArrow,
    };
}

/// Partie d'une annotation saisie à la souris
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotHandle {
    Body,
    LineStart,
    LineEnd,
    /// 0 : haut-gauche, 1 : haut-droit, 2 : bas-gauche, 3 : bas-droit
    Corner(u8),
}

const LINE_HIT_TOLERANCE_PT: f32 = 6.0;
const LINE_RECT_MARGIN_PT: f32 = 12.0;
const INK_RECT_MARGIN_PT: f32 = 4.0;
const MIN_ANNOT_WIDTH_PT: f32 = 24.0;
const MIN_ANNOT_HEIGHT_PT: f32 = 16.0;
const MIN_INK_SIZE_PT: f32 = 8.0;

pub const FREETEXT_DEFAULT_FILL: (u8, u8, u8) = (255, 255, 245);
const FREETEXT_BORDER_WIDTH_PT: f32 = 1.5;
const NOTE_BUBBLE_WIDTH_PT: f32 = 190.0;
const NOTE_BUBBLE_HEIGHT_PT: f32 = 70.0;
const NOTE_BUBBLE_GAP_PT: f32 = 12.0;

/// Position par défaut d'une bulle : sous le texte associé, ou au-dessus en bas de page.
fn default_note_bubble(quads: &[PdfTopDownRect], page_dim: PageDimensions) -> PdfTopDownRect {
    let anchor = quads
        .iter()
        .skip(1)
        .fold(quads.first().copied().unwrap_or(PdfTopDownRect {
            x_min: 40.0,
            y_min: 40.0,
            x_max: 40.0,
            y_max: 40.0,
            angle_deg: 0.0,
        }), |acc, q| acc.union(q));
    let x_min = anchor
        .x_min
        .min(page_dim.width_pt - NOTE_BUBBLE_WIDTH_PT - 8.0)
        .max(8.0);
    let below = anchor.y_max + NOTE_BUBBLE_GAP_PT;
    let y_min = if below + NOTE_BUBBLE_HEIGHT_PT <= page_dim.height_pt - 8.0 {
        below
    } else {
        (anchor.y_min - NOTE_BUBBLE_GAP_PT - NOTE_BUBBLE_HEIGHT_PT).max(8.0)
    };
    PdfTopDownRect {
        x_min,
        y_min,
        x_max: x_min + NOTE_BUBBLE_WIDTH_PT,
        y_max: y_min + NOTE_BUBBLE_HEIGHT_PT,
        angle_deg: 0.0,
    }
}

fn distance_to_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let len_sq = abx * abx + aby * aby;
    let t = if len_sq < 1e-6 {
        0.0
    } else {
        (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / len_sq).clamp(0.0, 1.0)
    };
    (p.0 - (a.0 + abx * t)).hypot(p.1 - (a.1 + aby * t))
}

/// Variantes d'annotations conformes à la norme ISO 32000-1 / 32000-2 (§12.5)
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotationKind {
    /// `/Subtype /Highlight` avec `/QuadPoints`
    Highlight { quads: Vec<PdfTopDownRect> },
    /// `/Subtype /FreeText` avec `/DA` (DefaultAppearance), `/Contents` et `/AP`
    /// `fill_rgb` et `border_rgb` à `None` : bloc transparent, sans contour, fondu dans la page.
    FreeText {
        text: String,
        font_size: f32,
        fill_rgb: Option<(u8, u8, u8)>,
        border_rgb: Option<(u8, u8, u8)>,
    },
    /// `/Subtype /Ink` avec `/InkList` (tracés vectoriels libres)
    Ink {
        strokes: Vec<Vec<(f32, f32)>>,
        thickness: f32,
    },
    /// `/Subtype /Square` ou `/Subtype /Line` avec `/LE [/None /ClosedArrow]`
    Shape {
        shape_kind: ShapeKind,
        start_pt: (f32, f32),
        end_pt: (f32, f32),
    },
    /// Note. Avec `anchor_quads`, elle est liée à un mot ou une phrase et enregistrée comme les
    /// commentaires d'Acrobat (`/Highlight` + `/Contents` + `/Popup`) ; sinon `/Subtype /Text`.
    /// `PdfAnnotation::rect` est alors la position de la bulle, pas celle du texte.
    StickyNote {
        comment: String,
        is_popup_open: bool,
        anchor_quads: Vec<PdfTopDownRect>,
    },
    /// `/Subtype /Line` + `/Measure` (Règle de mesure de cote en pt/mm) ou `/Subtype /Stamp`
    Measure {
        start_pt: (f32, f32),
        end_pt: (f32, f32),
        stamp_tag: Option<String>,
    },
}

/// Annotation PDF unifiée (en coordonnées points PDF top-down pour l'affichage et convertie en bottom-up ISO 32000 à l'enregistrement)
#[derive(Debug, Clone, PartialEq)]
pub struct PdfAnnotation {
    pub id: u64,
    pub page_index: usize,
    pub rect: PdfTopDownRect,
    pub color_rgb: (u8, u8, u8),
    pub kind: AnnotationKind,
}

impl PdfAnnotation {
    pub fn egui_color(&self, alpha: u8) -> Color32 {
        Color32::from_rgba_unmultiplied(
            self.color_rgb.0,
            self.color_rgb.1,
            self.color_rgb.2,
            alpha,
        )
    }

    pub fn type_badge(&self) -> &'static str {
        match &self.kind {
            AnnotationKind::Highlight { .. } => "Surlignage (/Highlight)",
            AnnotationKind::FreeText { .. } => "Bloc Texte (/FreeText)",
            AnnotationKind::Ink { .. } => "Dessin Libre (/Ink)",
            AnnotationKind::Shape {
                shape_kind: ShapeKind::Rectangle,
                ..
            } => "Rectangle (/Square)",
            AnnotationKind::Shape {
                shape_kind: ShapeKind::ArrowLine { .. },
                ..
            } => "Flèche (/Line)",
            AnnotationKind::StickyNote { anchor_quads, .. } if !anchor_quads.is_empty() => {
                "Note sur le texte"
            }
            AnnotationKind::StickyNote { .. } => "Note (/Text)",
            AnnotationKind::Measure { .. } => "Mesure & Cote (/Measure)",
        }
    }

    /// Extrémités d'une annotation linéaire (flèche ou mesure)
    pub fn line_endpoints(&self) -> Option<((f32, f32), (f32, f32))> {
        match &self.kind {
            AnnotationKind::Shape {
                shape_kind: ShapeKind::ArrowLine { .. },
                start_pt,
                end_pt,
            }
            | AnnotationKind::Measure {
                start_pt, end_pt, ..
            } => Some((*start_pt, *end_pt)),
            _ => None,
        }
    }

    /// Vérifie si un clic en coordonnées points PDF `(x_pt, y_pt)` touche cette annotation.
    /// Lignes et dessins ne réagissent que près du trait : ils ne masquent pas le texte qu'ils entourent.
    pub fn hit_test(&self, x_pt: f32, y_pt: f32) -> bool {
        let p = (x_pt, y_pt);
        if let Some((a, b)) = self.line_endpoints() {
            return distance_to_segment(p, a, b) <= LINE_HIT_TOLERANCE_PT;
        }
        if let AnnotationKind::Ink { strokes, .. } = &self.kind {
            return strokes.iter().any(|s| {
                s.windows(2)
                    .any(|w| distance_to_segment(p, w[0], w[1]) <= LINE_HIT_TOLERANCE_PT)
            });
        }
        if let AnnotationKind::StickyNote { anchor_quads, .. } = &self.kind {
            // La bulle d'une note liée n'existe à l'écran que lorsqu'elle est ouverte :
            // au repos, seul le texte associé réagit.
            if !anchor_quads.is_empty() {
                return anchor_quads.iter().any(|q| q.contains_pt(x_pt, y_pt, 1.0));
            }
        }
        self.rect.contains_pt(x_pt, y_pt, 5.0)
    }

    /// Crée une note liée au texte sélectionné, avec sa bulle placée à côté.
    pub fn new_text_note(
        page_index: usize,
        anchor_quads: Vec<PdfTopDownRect>,
        color_rgb: (u8, u8, u8),
        page_dim: PageDimensions,
    ) -> Self {
        Self {
            id: 0,
            page_index,
            rect: default_note_bubble(&anchor_quads, page_dim),
            color_rgb,
            kind: AnnotationKind::StickyNote {
                comment: String::new(),
                is_popup_open: true,
                anchor_quads,
            },
        }
    }

    /// Poignée située sous le point donné (`tolerance` en points PDF).
    pub fn handle_at(&self, x_pt: f32, y_pt: f32, tolerance: f32) -> Option<AnnotHandle> {
        let near = |q: (f32, f32)| (q.0 - x_pt).hypot(q.1 - y_pt) <= tolerance;
        if let Some((a, b)) = self.line_endpoints() {
            return if near(a) {
                Some(AnnotHandle::LineStart)
            } else if near(b) {
                Some(AnnotHandle::LineEnd)
            } else {
                None
            };
        }
        if matches!(self.kind, AnnotationKind::Highlight { .. }) {
            return None;
        }
        let r = &self.rect;
        [
            (r.x_min, r.y_min),
            (r.x_max, r.y_min),
            (r.x_min, r.y_max),
            (r.x_max, r.y_max),
        ]
        .into_iter()
        .position(near)
        .map(|i| AnnotHandle::Corner(i as u8))
    }

    /// Recalcule le rectangle d'une flèche, d'une mesure ou d'un dessin à partir de sa géométrie.
    pub fn refresh_rect_from_geometry(&mut self) {
        let (points, margin): (Vec<(f32, f32)>, f32) = if let Some((a, b)) = self.line_endpoints() {
            (vec![a, b], LINE_RECT_MARGIN_PT)
        } else if let AnnotationKind::Ink { strokes, .. } = &self.kind {
            (strokes.iter().flatten().copied().collect(), INK_RECT_MARGIN_PT)
        } else {
            return;
        };
        if points.is_empty() {
            return;
        }
        let fold = |f: fn(f32, f32) -> f32, init: f32, get: fn(&(f32, f32)) -> f32| {
            points.iter().map(get).fold(init, f)
        };
        self.rect = PdfTopDownRect {
            x_min: fold(f32::min, f32::INFINITY, |p| p.0) - margin,
            y_min: fold(f32::min, f32::INFINITY, |p| p.1) - margin,
            x_max: fold(f32::max, f32::NEG_INFINITY, |p| p.0) + margin,
            y_max: fold(f32::max, f32::NEG_INFINITY, |p| p.1) + margin,
            angle_deg: 0.0,
        };
    }

    /// Applique le glissement d'une poignée : l'état est toujours recalculé depuis `original`,
    /// ce qui évite toute dérive pendant le geste.
    pub fn apply_handle_drag(
        &mut self,
        original: &PdfAnnotation,
        handle: AnnotHandle,
        grab_pt: (f32, f32),
        cursor: (f32, f32),
    ) {
        *self = original.clone();
        match handle {
            AnnotHandle::Body => self.translate(cursor.0 - grab_pt.0, cursor.1 - grab_pt.1),
            AnnotHandle::LineStart | AnnotHandle::LineEnd => {
                if let AnnotationKind::Shape {
                    start_pt, end_pt, ..
                }
                | AnnotationKind::Measure {
                    start_pt, end_pt, ..
                } = &mut self.kind
                {
                    if handle == AnnotHandle::LineStart {
                        *start_pt = cursor;
                    } else {
                        *end_pt = cursor;
                    }
                }
                self.refresh_rect_from_geometry();
            }
            AnnotHandle::Corner(corner) => self.resize_from_corner(corner, cursor),
        }
    }

    fn resize_from_corner(&mut self, corner: u8, cursor: (f32, f32)) {
        let old = self.rect;
        let dragging_left = corner == 0 || corner == 2;
        let dragging_top = corner == 0 || corner == 1;
        let anchor = (
            if dragging_left { old.x_max } else { old.x_min },
            if dragging_top { old.y_max } else { old.y_min },
        );
        let mut new_w = if dragging_left {
            anchor.0 - cursor.0
        } else {
            cursor.0 - anchor.0
        };
        let mut new_h = if dragging_top {
            anchor.1 - cursor.1
        } else {
            cursor.1 - anchor.1
        };

        if let AnnotationKind::Ink { strokes, thickness } = &mut self.kind {
            // Le dessin garde ses proportions : un seul facteur pour les deux axes.
            let (old_w, old_h) = (old.width().max(1.0), old.height().max(1.0));
            let min_scale = (MIN_INK_SIZE_PT / old_w).max(MIN_INK_SIZE_PT / old_h);
            let scale = (new_w / old_w).max(new_h / old_h).max(min_scale);
            for pt in strokes.iter_mut().flatten() {
                pt.0 = anchor.0 + (pt.0 - anchor.0) * scale;
                pt.1 = anchor.1 + (pt.1 - anchor.1) * scale;
            }
            *thickness = (*thickness * scale).max(0.4);
            new_w = old_w * scale;
            new_h = old_h * scale;
        } else {
            new_w = new_w.max(MIN_ANNOT_WIDTH_PT);
            new_h = new_h.max(MIN_ANNOT_HEIGHT_PT);
        }

        let (x_min, x_max) = if dragging_left {
            (anchor.0 - new_w, anchor.0)
        } else {
            (anchor.0, anchor.0 + new_w)
        };
        let (y_min, y_max) = if dragging_top {
            (anchor.1 - new_h, anchor.1)
        } else {
            (anchor.1, anchor.1 + new_h)
        };
        self.rect = PdfTopDownRect {
            x_min,
            y_min,
            x_max,
            y_max,
            angle_deg: 0.0,
        };
        if let AnnotationKind::Shape {
            shape_kind: ShapeKind::Rectangle,
            start_pt,
            end_pt,
        } = &mut self.kind
        {
            *start_pt = (x_min, y_min);
            *end_pt = (x_max, y_max);
        }
    }

    /// Ajoute un tracé à un dessin existant (signature en plusieurs levers de stylet).
    pub fn append_ink_stroke(&mut self, stroke: Vec<(f32, f32)>) {
        if let AnnotationKind::Ink { strokes, .. } = &mut self.kind {
            strokes.push(stroke);
            self.refresh_rect_from_geometry();
        }
    }

    /// Déplace l'annotation de `(dx_pt, dy_pt)` en mettant à jour toutes ses coordonnées internes
    pub fn translate(&mut self, dx_pt: f32, dy_pt: f32) {
        self.rect = self.rect.translate(dx_pt, dy_pt);
        match &mut self.kind {
            AnnotationKind::Highlight { quads } => {
                for q in quads.iter_mut() {
                    *q = q.translate(dx_pt, dy_pt);
                }
            }
            AnnotationKind::FreeText { .. } | AnnotationKind::StickyNote { .. } => {}
            AnnotationKind::Ink { strokes, .. } => {
                for stroke in strokes.iter_mut() {
                    for pt in stroke.iter_mut() {
                        pt.0 += dx_pt;
                        pt.1 += dy_pt;
                    }
                }
            }
            AnnotationKind::Shape {
                start_pt, end_pt, ..
            }
            | AnnotationKind::Measure {
                start_pt, end_pt, ..
            } => {
                start_pt.0 += dx_pt;
                start_pt.1 += dy_pt;
                end_pt.0 += dx_pt;
                end_pt.1 += dy_pt;
            }
        }
    }

    /// Calcule la distance en points PDF et en millimètres pour un outil de mesure
    pub fn measure_distance_pt_and_mm(start: (f32, f32), end: (f32, f32)) -> (f32, f32) {
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let dist_pt = (dx * dx + dy * dy).sqrt();
        let dist_mm = dist_pt * (25.4 / 72.0);
        (dist_pt, dist_mm)
    }
}

/// Gestionnaire d'annotations par onglet avec historique Annuler/Rétablir (`Ctrl+Z` / `Ctrl+Y`)
#[derive(Debug, Clone, Default)]
pub struct AnnotationManager {
    pub items: Vec<PdfAnnotation>,
    pub selected_id: Option<u64>,
    next_id: u64,
    undo_stack: Vec<Vec<PdfAnnotation>>,
    redo_stack: Vec<Vec<PdfAnnotation>>,
    /// Tracé ou forme en cours de dessin au Trackpad / Souris
    pub active_draft_points: Vec<(f32, f32)>,
    pub active_draft_page: Option<usize>,
    /// Couleur active choisie pour les nouvelles annotations
    pub active_color_rgb: (u8, u8, u8),
    /// Indique si le dernier enregistrement `Ctrl+S` vient de réussir (compteur de frames)
    pub saved_feedback_frames: u32,
}

pub const PALETTE_COLORS: [(&str, (u8, u8, u8)); 5] = [
    ("Jaune Or", (255, 210, 30)),
    ("Bleu Azur", (45, 135, 245)),
    ("Vert Émeraude", (40, 195, 125)),
    ("Rouge Corail", (235, 70, 75)),
    ("Violet Améthyste", (175, 90, 235)),
];

impl AnnotationManager {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected_id: None,
            next_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_draft_points: Vec::new(),
            active_draft_page: None,
            active_color_rgb: PALETTE_COLORS[0].1,
            saved_feedback_frames: 0,
        }
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    pub fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(self.items.clone());
        if self.undo_stack.len() > 40 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    pub fn add_annotation(&mut self, mut annot: PdfAnnotation) -> u64 {
        self.push_undo_snapshot();
        if annot.id == 0 {
            annot.id = self.alloc_id();
        } else {
            self.next_id = self.next_id.max(annot.id + 1);
        }
        let id = annot.id;
        let is_highlight = matches!(annot.kind, AnnotationKind::Highlight { .. });
        self.items.push(annot);
        if !is_highlight {
            self.selected_id = Some(id);
        }
        id
    }

    pub fn delete_selected(&mut self) -> bool {
        let Some(sel_id) = self.selected_id else {
            return false;
        };
        if self.items.iter().any(|a| a.id == sel_id) {
            self.push_undo_snapshot();
            self.items.retain(|a| a.id != sel_id);
            self.selected_id = None;
            true
        } else {
            false
        }
    }

    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.items.clone());
            self.items = prev;
            if let Some(sel) = self.selected_id {
                if !self.items.iter().any(|a| a.id == sel) {
                    self.selected_id = None;
                }
            }
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.items.clone());
            self.items = next;
            true
        } else {
            false
        }
    }

    pub fn selected_mut(&mut self) -> Option<&mut PdfAnnotation> {
        let id = self.selected_id?;
        self.items.iter_mut().find(|a| a.id == id)
    }

    pub fn selected(&self) -> Option<&PdfAnnotation> {
        let id = self.selected_id?;
        self.items.iter().find(|a| a.id == id)
    }

    /// Génère un jeu complet d'annotations de démonstration ISO 32000 sur la page `page_index`
    /// couvrant tous les outils façon Adobe Acrobat : Highlight, FreeText, Ink, Shape/Flèche, StickyNote et Mesure.
    pub fn populate_demo_showcase(&mut self, page_index: usize) {
        // 1. Surlignage de texte (/Highlight + /QuadPoints)
        let q1 = PdfTopDownRect {
            x_min: 52.0,
            y_min: 158.0,
            x_max: 485.0,
            y_max: 173.0,
            angle_deg: 0.0,
        };
        self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: q1,
            color_rgb: (255, 215, 25),
            kind: AnnotationKind::Highlight { quads: vec![q1] },
        });

        // 2. Bloc de texte directement visible et éditable (/FreeText)
        let ft_rect = PdfTopDownRect {
            x_min: 335.0,
            y_min: 262.0,
            x_max: 536.0,
            y_max: 318.0,
            angle_deg: 0.0,
        };
        let ft_id = self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: ft_rect,
            color_rgb: (45, 135, 245),
            kind: AnnotationKind::FreeText {
                text: "Bloc FreeText ISO 32000\nModifiable dans Adobe Acrobat !".to_string(),
                font_size: 11.5,
                fill_rgb: Some(FREETEXT_DEFAULT_FILL),
                border_rgb: Some((45, 135, 245)),
            },
        });

        // 3. Flèche et Rectangle d'annotation sur le graphique (/Square & /Line)
        self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: PdfTopDownRect {
                x_min: 235.0,
                y_min: 310.0,
                x_max: 345.0,
                y_max: 395.0,
                angle_deg: 0.0,
            },
            color_rgb: (235, 70, 75),
            kind: AnnotationKind::Shape {
                shape_kind: ShapeKind::DEFAULT_ARROW,
                start_pt: (340.0, 315.0),
                end_pt: (245.0, 390.0),
            },
        });

        // 4. Dessin libre à main levée (/Ink + /InkList) entourant un point du graphique
        let mut circle_pts = Vec::with_capacity(25);
        for k in 0..=24 {
            let a = (k as f32 / 24.0) * std::f32::consts::TAU;
            circle_pts.push((330.0 + 28.0 * a.cos(), 432.0 + 18.0 * a.sin()));
        }
        self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: PdfTopDownRect {
                x_min: 298.0,
                y_min: 410.0,
                x_max: 362.0,
                y_max: 454.0,
                angle_deg: 0.0,
            },
            color_rgb: (40, 195, 125),
            kind: AnnotationKind::Ink {
                strokes: vec![circle_pts],
                thickness: 2.5,
            },
        });

        // 5. Note / Commentaire en info-bulle (/Text Sticky Note)
        self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: PdfTopDownRect {
                x_min: 62.0,
                y_min: 265.0,
                x_max: 245.0,
                y_max: 315.0,
                angle_deg: 0.0,
            },
            color_rgb: (175, 90, 235),
            kind: AnnotationKind::StickyNote {
                comment: "Note /Text : Verifier le pic GPU a 60 FPS".to_string(),
                is_popup_open: true,
                anchor_quads: Vec::new(),
            },
        });

        // 6. Règle de mesure de cote (/Measure) dans la partie basse du graphique
        self.add_annotation(PdfAnnotation {
            id: 0,
            page_index,
            rect: PdfTopDownRect {
                x_min: 90.0,
                y_min: 493.0,
                x_max: 500.0,
                y_max: 517.0,
                angle_deg: 0.0,
            },
            color_rgb: (45, 135, 245),
            kind: AnnotationKind::Measure {
                start_pt: (90.0, 505.0),
                end_pt: (500.0, 505.0),
                stamp_tag: Some("APPROUVE ISO 32000".to_string()),
            },
        });

        // Sélectionner le bloc FreeText par défaut pour montrer la barre d'édition
        self.selected_id = Some(ft_id);
    }
}

/// Enregistre toutes les annotations dans un fichier PDF en respectant strictement la norme ISO 32000-1 (§12.5).
/// Chaque annotation est écrite comme un dictionnaire `/Annot` standard dans le tableau `/Annots` de la page,
/// accompagnée de son flux d'apparence `/AP` (`Form XObject`), garantissant qu'Adobe Acrobat ou tout autre lecteur
/// puisse afficher ET modifier/déplacer/éditer les blocs de texte, surlignages et dessins.
#[allow(dead_code)]
pub fn save_annotations_to_pdf(
    source_path: &Path,
    dest_path: &Path,
    annotations: &[PdfAnnotation],
    page_dimensions: &[PageDimensions],
) -> Result<()> {
    save_annotations_and_ocr_to_pdf(source_path, dest_path, annotations, &[], page_dimensions)
}

/// Enregistre à la fois les annotations standard ISO 32000 (`/Annots` + `/AP`) ET la couche de texte OCR
/// invisible normalisée (`3 Tr` + matrice orientée `Tm`) directement dans le fichier PDF (`Ctrl+S`).
pub fn save_annotations_and_ocr_to_pdf(
    source_path: &Path,
    dest_path: &Path,
    annotations: &[PdfAnnotation],
    text_layers: &[PageTextLayer],
    page_dimensions: &[PageDimensions],
) -> Result<()> {
    let mut doc = Document::load(source_path).with_context(|| {
        format!(
            "Impossible de charger le PDF source pour sauvegarde : {}",
            source_path.display()
        )
    })?;

    // Police Helvetica standard pour les flux d'apparence /FreeText
    let helv_font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    let pages_map = doc.get_pages(); // BTreeMap<u32 (1-based page num), ObjectId>

    for (&page_num_1based, &page_obj_id) in &pages_map {
        let page_idx = (page_num_1based as usize).saturating_sub(1);
        let page_dim = page_dimensions.get(page_idx).copied().unwrap_or(PageDimensions {
            width_pt: 595.0,
            height_pt: 842.0,
        });
        let page_h = page_dim.height_pt;

        let page_annots: Vec<&PdfAnnotation> = annotations
            .iter()
            .filter(|a| a.page_index == page_idx)
            .collect();

        // Récupérer les annotations non-SuperPDF éventuellement déjà présentes
        let mut annot_refs: Vec<Object> = Vec::new();
        if let Ok(page_dict) = doc.get_object(page_obj_id).and_then(|o| o.as_dict()) {
            if let Ok(existing_annots) = page_dict.get(b"Annots").and_then(|o| o.as_array()) {
                for item in existing_annots {
                    let is_ours = if let Ok(id) = item.as_reference() {
                        doc.get_object(id)
                            .and_then(|o| o.as_dict())
                            .and_then(|d| d.get(b"NM"))
                            .and_then(|n| n.as_str())
                            .map(|s| s.starts_with(b"superpdf_"))
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    if !is_ours {
                        annot_refs.push(item.clone());
                    }
                }
            }
        }

        for annot in page_annots {
            for object_id in build_iso32000_annot_objects(&mut doc, annot, page_h, helv_font_id)? {
                annot_refs.push(Object::Reference(object_id));
            }
        }

        if let Ok(page_obj) = doc.get_object_mut(page_obj_id) {
            if let Ok(page_dict) = page_obj.as_dict_mut() {
                page_dict.set("Annots", Object::Array(annot_refs));
            }
        }
    }

    // Injection de la couche de texte OCR invisible normalisée (`3 Tr` + `Tm`) pour les images embarquées scannées
    if !text_layers.is_empty() {
        let _ = append_invisible_ocr_layer_to_doc(&mut doc, &pages_map, text_layers, page_dimensions);
    }

    // Écriture dans un fichier voisin puis remplacement atomique : une panne en cours d'écriture
    // ne peut pas corrompre l'original, et les lectures PDFium en cours restent valides.
    let temp_path = dest_path.with_extension("superpdf-save.tmp");
    doc.save(&temp_path).with_context(|| {
        format!(
            "Échec d'écriture du fichier PDF annoté : {}",
            temp_path.display()
        )
    })?;
    std::fs::rename(&temp_path, dest_path).with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Impossible de remplacer le fichier PDF : {}",
            dest_path.display()
        )
    })?;
    Ok(())
}

/// Chaîne de texte PDF : UTF-16BE avec BOM dès qu'un caractère sort de l'ASCII (ISO 32000-1 §7.9.2.2).
fn pdf_text_string(text: &str) -> Object {
    if text.is_ascii() {
        return Object::string_literal(text);
    }
    let mut bytes = vec![0xFE, 0xFF];
    bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    Object::String(bytes, lopdf::StringFormat::Hexadecimal)
}

fn decode_pdf_text(bytes: &[u8]) -> String {
    if let Some(utf16) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    // Les fichiers écrits par les versions précédentes contiennent de l'UTF-8 brut.
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect())
}

/// Largeur approximative d'un caractère Helvetica, en fraction de corps.
fn helvetica_char_width(c: char) -> f32 {
    match c {
        'i' | 'j' | 'l' | '\'' | '.' | ',' | ':' | ';' | '|' | '!' | ' ' | 't' | 'f' | 'r' => 0.3,
        'm' | 'w' | 'M' | 'W' => 0.82,
        c if c.is_uppercase() => 0.68,
        _ => 0.55,
    }
}

/// Coupe le texte aux mots pour tenir dans `max_width` : Acrobat affiche ce flux d'apparence tel quel.
fn wrap_helvetica_lines(text: &str, max_width: f32, font_size: f32) -> Vec<String> {
    let width_of = |s: &str| s.chars().map(helvetica_char_width).sum::<f32>() * font_size;
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut current = String::new();
        for word in paragraph.split(' ') {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if width_of(&candidate) > max_width && !current.is_empty() {
                lines.push(std::mem::replace(&mut current, word.to_string()));
            } else {
                current = candidate;
            }
        }
        lines.push(current);
    }
    lines
}

/// Marquage d'un surlignage : teinte chaque ligne en mode multiplicatif pour laisser le texte lisible.
fn add_highlight_markup(
    doc: &mut Document,
    dict: &mut lopdf::Dictionary,
    quads: &[PdfTopDownRect],
    (r, g, b): (f32, f32, f32),
    origin: (f32, f32),
    size: (f32, f32),
    page_h: f32,
) -> Result<()> {
    dict.set("Subtype", Object::Name(b"Highlight".to_vec()));
    dict.set("CA", 1.0);
    let mut quad_points: Vec<Object> = Vec::with_capacity(quads.len() * 8);
    let mut ops = vec![
        Operation::new("q", vec![]),
        Operation::new("gs", vec!["GS_Multiply".into()]),
        Operation::new("rg", vec![r.into(), g.into(), b.into()]),
    ];
    for q in quads {
        let (y_top, y_bottom) = (page_h - q.y_min, page_h - q.y_max);
        // Ordre ISO 32000 QuadPoints : (x1,y_top), (x2,y_top), (x1,y_bot), (x2,y_bot)
        quad_points.extend_from_slice(&[
            q.x_min.into(),
            y_top.into(),
            q.x_max.into(),
            y_top.into(),
            q.x_min.into(),
            y_bottom.into(),
            q.x_max.into(),
            y_bottom.into(),
        ]);
        ops.push(Operation::new(
            "re",
            vec![
                (q.x_min - origin.0).into(),
                (y_bottom - origin.1).into(),
                q.width().into(),
                q.height().into(),
            ],
        ));
    }
    ops.push(Operation::new("f", vec![]));
    ops.push(Operation::new("Q", vec![]));
    dict.set("QuadPoints", Object::Array(quad_points));

    let multiply_state = doc.add_object(dictionary! {
        "Type" => "ExtGState",
        "BM" => "Multiply",
    });
    let resources = dictionary! {
        "ExtGState" => Object::Dictionary(dictionary! { "GS_Multiply" => multiply_state }),
    };
    let ap_id = create_appearance_stream_with_resources(doc, size.0, size.1, ops, resources)?;
    dict.set("AP", dictionary! { "N" => ap_id });
    Ok(())
}

fn build_iso32000_annot_objects(
    doc: &mut Document,
    annot: &PdfAnnotation,
    page_h: f32,
    helv_font_id: ObjectId,
) -> Result<Vec<ObjectId>> {
    // Le rectangle d'une flèche ou d'un dessin doit contenir ses extrémités, sinon `/BBox` les couperait.
    let mut owned_annot = annot.clone();
    owned_annot.refresh_rect_from_geometry();
    // Note liée : `/Rect` couvre le texte associé, la bulle part dans l'annotation `/Popup`.
    let mut note_bubble_rect = None;
    if let AnnotationKind::StickyNote { anchor_quads, .. } = &owned_annot.kind {
        if let Some(first) = anchor_quads.first() {
            let union = anchor_quads.iter().fold(*first, |acc, q| acc.union(q));
            note_bubble_rect = Some(owned_annot.rect);
            owned_annot.rect = union;
        }
    }
    let annot = &owned_annot;

    let llx = annot.rect.x_min;
    let urx = annot.rect.x_max.max(llx + 4.0);
    let ury = (page_h - annot.rect.y_min).max(4.0);
    let lly = (page_h - annot.rect.y_max).min(ury - 4.0);

    let r = annot.color_rgb.0 as f32 / 255.0;
    let g = annot.color_rgb.1 as f32 / 255.0;
    let b = annot.color_rgb.2 as f32 / 255.0;

    let mut dict = lopdf::Dictionary::new();
    dict.set("Type", Object::Name(b"Annot".to_vec()));
    dict.set(
        "Rect",
        vec![llx.into(), lly.into(), urx.into(), ury.into()],
    );
    dict.set("C", vec![r.into(), g.into(), b.into()]);
    dict.set("F", 4); // Bit 3 = Print (ISO 32000-1 Table 165)
    dict.set(
        "NM",
        Object::string_literal(format!("superpdf_{}", annot.id)),
    );

    let width = (urx - llx).max(4.0);
    let height = (ury - lly).max(4.0);

    match &annot.kind {
        AnnotationKind::Highlight { quads } => {
            add_highlight_markup(
                doc,
                &mut dict,
                quads,
                (r, g, b),
                (llx, lly),
                (width, height),
                page_h,
            )?;
        }
        AnnotationKind::FreeText {
            text,
            font_size,
            fill_rgb,
            border_rgb,
        } => {
            let to_unit = |c: (u8, u8, u8)| {
                (
                    c.0 as f32 / 255.0,
                    c.1 as f32 / 255.0,
                    c.2 as f32 / 255.0,
                )
            };
            dict.set("Subtype", Object::Name(b"FreeText".to_vec()));
            dict.set("Contents", pdf_text_string(text));
            // Convention Acrobat : `/C` porte le fond, `/DA` le texte et le contour (`RG`), `/BS` l'épaisseur.
            dict.remove(b"C");
            if let Some(fill) = fill_rgb {
                let (fr, fg, fb) = to_unit(*fill);
                dict.set("C", vec![fr.into(), fg.into(), fb.into()]);
            }
            let border_width = if border_rgb.is_some() {
                FREETEXT_BORDER_WIDTH_PT
            } else {
                0.0
            };
            dict.set("BS", dictionary! { "W" => border_width });
            let border_operator = border_rgb.map_or(String::new(), |c| {
                let (br, bg, bb) = to_unit(c);
                format!(" {br:.3} {bg:.3} {bb:.3} RG")
            });
            dict.set(
                "DA",
                Object::string_literal(format!(
                    "/Helv {:.1} Tf 0.100 0.120 0.180 rg{}",
                    font_size, border_operator
                )),
            );

            let mut ap_ops = vec![Operation::new("q", vec![])];
            if let Some(fill) = fill_rgb {
                let (fr, fg, fb) = to_unit(*fill);
                ap_ops.push(Operation::new("rg", vec![fr.into(), fg.into(), fb.into()]));
            }
            if let Some(border) = border_rgb {
                let (br, bg, bb) = to_unit(*border);
                ap_ops.push(Operation::new("RG", vec![br.into(), bg.into(), bb.into()]));
                ap_ops.push(Operation::new("w", vec![border_width.into()]));
            }
            ap_ops.push(Operation::new(
                "re",
                vec![0.75.into(), 0.75.into(), (width - 1.5).into(), (height - 1.5).into()],
            ));
            ap_ops.push(Operation::new(
                match (fill_rgb.is_some(), border_rgb.is_some()) {
                    (true, true) => "B",
                    (true, false) => "f",
                    (false, true) => "S",
                    (false, false) => "n",
                },
                vec![],
            ));
            ap_ops.push(Operation::new("Q", vec![]));

            let line_height = font_size * 1.2;
            for (line_idx, line) in wrap_helvetica_lines(text, width - 12.0, *font_size)
                .iter()
                .enumerate()
            {
                let y_line = height - 4.0 - font_size - line_idx as f32 * line_height;
                if y_line < 1.0 {
                    break;
                }
                ap_ops.push(Operation::new("BT", vec![]));
                ap_ops.push(Operation::new("rg", vec![0.10.into(), 0.12.into(), 0.18.into()]));
                ap_ops.push(Operation::new("Tf", vec!["Helv".into(), (*font_size).into()]));
                ap_ops.push(Operation::new("Td", vec![6.into(), y_line.into()]));
                ap_ops.push(Operation::new(
                    "Tj",
                    vec![Object::String(
                        encode_win_ansi(line),
                        lopdf::StringFormat::Literal,
                    )],
                ));
                ap_ops.push(Operation::new("ET", vec![]));
            }

            let ap_id = create_appearance_stream(doc, width, height, ap_ops, Some(helv_font_id))?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::Ink { strokes, thickness } => {
            dict.set("Subtype", Object::Name(b"Ink".to_vec()));
            let mut ink_list: Vec<Object> = Vec::with_capacity(strokes.len());
            let mut ap_ops = vec![
                Operation::new("q", vec![]),
                Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                Operation::new("w", vec![(*thickness).into()]),
            ];

            for stroke in strokes {
                let mut stroke_coords: Vec<Object> = Vec::with_capacity(stroke.len() * 2);
                for (idx, &(px, py)) in stroke.iter().enumerate() {
                    let pdf_x = px;
                    let pdf_y = page_h - py;
                    stroke_coords.push(pdf_x.into());
                    stroke_coords.push(pdf_y.into());

                    let rel_x = (pdf_x - llx).clamp(0.0, width);
                    let rel_y = (pdf_y - lly).clamp(0.0, height);
                    if idx == 0 {
                        ap_ops.push(Operation::new("m", vec![rel_x.into(), rel_y.into()]));
                    } else {
                        ap_ops.push(Operation::new("l", vec![rel_x.into(), rel_y.into()]));
                    }
                }
                if stroke.len() >= 2 {
                    ap_ops.push(Operation::new("S", vec![]));
                }
                ink_list.push(Object::Array(stroke_coords));
            }
            ap_ops.push(Operation::new("Q", vec![]));
            dict.set("InkList", Object::Array(ink_list));

            let ap_id = create_appearance_stream(doc, width, height, ap_ops, None)?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::Shape {
            shape_kind,
            start_pt,
            end_pt,
        } => {
            match shape_kind {
                ShapeKind::Rectangle => {
                    dict.set("Subtype", Object::Name(b"Square".to_vec()));
                }
                ShapeKind::ArrowLine {
                    start_head,
                    end_head,
                } => {
                    dict.set("Subtype", Object::Name(b"Line".to_vec()));
                    dict.set(
                        "L",
                        vec![
                            start_pt.0.into(),
                            (page_h - start_pt.1).into(),
                            end_pt.0.into(),
                            (page_h - end_pt.1).into(),
                        ],
                    );
                    dict.set(
                        "LE",
                        vec![
                            Object::Name(start_head.pdf_name().to_vec()),
                            Object::Name(end_head.pdf_name().to_vec()),
                        ],
                    );
                }
            }
            let ap_ops = match shape_kind {
                ShapeKind::Rectangle => vec![
                    Operation::new("q", vec![]),
                    Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                    Operation::new("w", vec![2.0.into()]),
                    Operation::new(
                        "re",
                        vec![1.0.into(), 1.0.into(), (width - 2.0).into(), (height - 2.0).into()],
                    ),
                    Operation::new("S", vec![]),
                    Operation::new("Q", vec![]),
                ],
                ShapeKind::ArrowLine {
                    start_head,
                    end_head,
                } => {
                    let to_ap = |p: (f32, f32)| (p.0 - llx, (page_h - p.1) - lly);
                    let dir_to_end = unit_direction(*start_pt, *end_pt);
                    let dir_to_start = (-dir_to_end.0, -dir_to_end.1);
                    let (sx, sy) = to_ap(line_stop_point(*start_pt, dir_to_start, *start_head));
                    let (ex, ey) = to_ap(line_stop_point(*end_pt, dir_to_end, *end_head));
                    let mut ops = vec![
                        Operation::new("q", vec![]),
                        Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                        Operation::new("rg", vec![r.into(), g.into(), b.into()]),
                        Operation::new("w", vec![ARROW_LINE_WIDTH.into()]),
                        Operation::new("m", vec![sx.into(), sy.into()]),
                        Operation::new("l", vec![ex.into(), ey.into()]),
                        Operation::new("S", vec![]),
                    ];
                    let endings = [
                        (*start_head, *start_pt, dir_to_start),
                        (*end_head, *end_pt, dir_to_end),
                    ];
                    for (ending, tip, dir) in endings {
                        let Some(outline) = line_ending_outline(ending, tip, dir) else {
                            continue;
                        };
                        for (i, point) in outline.points.iter().enumerate() {
                            let (x, y) = to_ap(*point);
                            let op = if i == 0 { "m" } else { "l" };
                            ops.push(Operation::new(op, vec![x.into(), y.into()]));
                        }
                        if outline.filled {
                            ops.push(Operation::new("h", vec![]));
                            ops.push(Operation::new("f", vec![]));
                        } else {
                            ops.push(Operation::new("S", vec![]));
                        }
                    }
                    ops.push(Operation::new("Q", vec![]));
                    ops
                }
            };
            let ap_id = create_appearance_stream(doc, width, height, ap_ops, None)?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::StickyNote {
            comment,
            anchor_quads,
            ..
        } if !anchor_quads.is_empty() => {
            dict.set("Contents", pdf_text_string(comment));
            add_highlight_markup(
                doc,
                &mut dict,
                anchor_quads,
                (r, g, b),
                (llx, lly),
                (width, height),
                page_h,
            )?;
        }
        AnnotationKind::StickyNote { comment, .. } => {
            dict.set("Subtype", Object::Name(b"Text".to_vec()));
            dict.set("Name", Object::Name(b"Comment".to_vec()));
            dict.set("Contents", pdf_text_string(comment));
            dict.set("Open", true);
        }
        AnnotationKind::Measure {
            start_pt,
            end_pt,
            stamp_tag,
        } => {
            let (dist_pt, dist_mm) = PdfAnnotation::measure_distance_pt_and_mm(*start_pt, *end_pt);
            let label = format!(
                "{:.1} mm ({:.0} pt){}",
                dist_mm,
                dist_pt,
                stamp_tag
                    .as_ref()
                    .map(|t| format!(" | {}", t))
                    .unwrap_or_default()
            );
            dict.set("Subtype", Object::Name(b"Line".to_vec()));
            dict.set(
                "L",
                vec![
                    start_pt.0.into(),
                    (page_h - start_pt.1).into(),
                    end_pt.0.into(),
                    (page_h - end_pt.1).into(),
                ],
            );
            dict.set("Contents", Object::string_literal(label));
            dict.set("Cap", true);
        }
    }

    let annot_id = doc.add_object(Object::Dictionary(dict));
    let mut objects = vec![annot_id];
    if let Some(bubble) = note_bubble_rect {
        let popup_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Popup",
            "Parent" => annot_id,
            "Open" => false,
            "NM" => Object::string_literal(format!("superpdf_popup_{}", annot.id)),
            "Rect" => vec![
                bubble.x_min.into(),
                (page_h - bubble.y_max).into(),
                bubble.x_max.into(),
                (page_h - bubble.y_min).into(),
            ],
        });
        if let Ok(annot_dict) = doc.get_object_mut(annot_id).and_then(|o| o.as_dict_mut()) {
            annot_dict.set("Popup", popup_id);
        }
        objects.push(popup_id);
    }
    Ok(objects)
}

fn create_appearance_stream(
    doc: &mut Document,
    width: f32,
    height: f32,
    ops: Vec<Operation>,
    helv_font_id: Option<ObjectId>,
) -> Result<ObjectId> {
    let mut resources = lopdf::Dictionary::new();
    if let Some(fid) = helv_font_id {
        resources.set("Font", dictionary! { "Helv" => fid });
    }
    create_appearance_stream_with_resources(doc, width, height, ops, resources)
}

fn create_appearance_stream_with_resources(
    doc: &mut Document,
    width: f32,
    height: f32,
    ops: Vec<Operation>,
    resources: lopdf::Dictionary,
) -> Result<ObjectId> {
    let content = Content { operations: ops };
    let stream_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "FormType" => 1,
        "BBox" => vec![0.into(), 0.into(), width.into(), height.into()],
        "Resources" => Object::Dictionary(resources),
    };
    Ok(doc.add_object(Stream::new(stream_dict, content.encode()?)))
}

/// Charge les annotations standard ISO 32000 présentes dans le tableau `/Annots` d'un fichier PDF
/// (qu'elles aient été créées dans Adobe Acrobat ou dans Super Lecteur PDF).
pub fn load_annotations_from_pdf(
    path: &Path,
    page_dimensions: &[PageDimensions],
) -> Result<Vec<PdfAnnotation>> {
    // Pré-vérification ultra-rapide en mémoire (< 1 ms) : si le fichier ne contient ni marqueur
    // "superpdf_" ni sous-types d'annotation éditables (/Highlight, /FreeText, /Ink), ou s'il dépasse
    // 4 Mo sans marqueur "superpdf_", on évite de parser tout l'arbre d'objets avec lopdf à l'ouverture.
    if let Ok(raw_bytes) = std::fs::read(path) {
        let has_superpdf = raw_bytes.windows(9).any(|w| w == b"superpdf_");
        if !has_superpdf {
            if raw_bytes.len() > 4 * 1024 * 1024 {
                return Ok(Vec::new());
            }
            let has_editable_annot = raw_bytes
                .windows(9)
                .any(|w| w == b"Highlight" || w == b"/FreeText");
            if !has_editable_annot {
                return Ok(Vec::new());
            }
        }
    }

    let doc = Document::load(path)?;
    let pages_map = doc.get_pages();
    let mut loaded = Vec::new();
    let mut next_id = 1_u64;

    for (&page_num_1based, &page_obj_id) in &pages_map {
        let page_idx = (page_num_1based as usize).saturating_sub(1);
        let page_dim = page_dimensions.get(page_idx).copied().unwrap_or(PageDimensions {
            width_pt: 595.0,
            height_pt: 842.0,
        });
        let page_h = page_dim.height_pt;

        let Ok(page_dict) = doc.get_object(page_obj_id).and_then(|o| o.as_dict()) else {
            continue;
        };
        let Ok(annots_arr) = page_dict.get(b"Annots").and_then(|o| o.as_array()) else {
            continue;
        };

        for item in annots_arr {
            let annot_dict = match item {
                Object::Reference(id) => doc.get_object(*id).and_then(|o| o.as_dict()).ok(),
                Object::Dictionary(d) => Some(d),
                _ => None,
            };
            let Some(d) = annot_dict else {
                continue;
            };

            let subtype = d
                .get(b"Subtype")
                .and_then(|o| o.as_name_str())
                .unwrap_or("");

            let mut rect = parse_pdf_rect(d, page_h).unwrap_or(PdfTopDownRect {
                x_min: 50.0,
                y_min: 50.0,
                x_max: 200.0,
                y_max: 90.0,
                angle_deg: 0.0,
            });
            let color_rgb = parse_pdf_color(d).unwrap_or((255, 210, 30));
            let contents = d
                .get(b"Contents")
                .and_then(|o| o.as_str())
                .map(decode_pdf_text)
                .unwrap_or_default();

            let kind = match subtype {
                "Highlight" => {
                    let quads = parse_quad_points(d, page_h).unwrap_or_else(|| vec![rect]);
                    if contents.trim().is_empty() {
                        AnnotationKind::Highlight { quads }
                    } else {
                        // Commentaire attaché au texte : la bulle vient de `/Popup` si elle existe.
                        rect = d
                            .get(b"Popup")
                            .and_then(|o| o.as_reference())
                            .and_then(|id| doc.get_object(id))
                            .and_then(|o| o.as_dict())
                            .ok()
                            .and_then(|popup| parse_pdf_rect(popup, page_h))
                            .unwrap_or_else(|| default_note_bubble(&quads, page_dim));
                        AnnotationKind::StickyNote {
                            comment: contents,
                            is_popup_open: true,
                            anchor_quads: quads,
                        }
                    }
                }
                "FreeText" => {
                    let (fill_rgb, border_rgb) = parse_freetext_style(d, color_rgb);
                    AnnotationKind::FreeText {
                        text: if contents.is_empty() {
                            "Texte".to_string()
                        } else {
                            contents
                        },
                        font_size: 11.5,
                        fill_rgb,
                        border_rgb,
                    }
                }
                "Ink" => {
                    let mut strokes = Vec::new();
                    if let Ok(ink_list) = d.get(b"InkList").and_then(|o| o.as_array()) {
                        for s_obj in ink_list {
                            if let Ok(coords) = s_obj.as_array() {
                                let mut pts = Vec::new();
                                for pair in coords.chunks(2) {
                                    if pair.len() == 2 {
                                        let x = obj_to_f32(&pair[0]).unwrap_or(rect.x_min);
                                        let y = page_h - obj_to_f32(&pair[1]).unwrap_or(0.0);
                                        pts.push((x, y));
                                    }
                                }
                                if !pts.is_empty() {
                                    strokes.push(pts);
                                }
                            }
                        }
                    }
                    AnnotationKind::Ink {
                        strokes,
                        thickness: 2.5,
                    }
                }
                "Square" | "Circle" => AnnotationKind::Shape {
                    shape_kind: ShapeKind::Rectangle,
                    start_pt: (rect.x_min, rect.y_min),
                    end_pt: (rect.x_max, rect.y_max),
                },
                "Line" => {
                    let (start_pt, end_pt) = if let Ok(l_arr) = d.get(b"L").and_then(|o| o.as_array()) {
                        if l_arr.len() == 4 {
                            (
                                (
                                    obj_to_f32(&l_arr[0]).unwrap_or(rect.x_min),
                                    page_h - obj_to_f32(&l_arr[1]).unwrap_or(0.0),
                                ),
                                (
                                    obj_to_f32(&l_arr[2]).unwrap_or(rect.x_max),
                                    page_h - obj_to_f32(&l_arr[3]).unwrap_or(0.0),
                                ),
                            )
                        } else {
                            ((rect.x_min, rect.y_min), (rect.x_max, rect.y_max))
                        }
                    } else {
                        ((rect.x_min, rect.y_min), (rect.x_max, rect.y_max))
                    };
                    if contents.contains("mm") {
                        AnnotationKind::Measure {
                            start_pt,
                            end_pt,
                            stamp_tag: Some("MESURE".to_string()),
                        }
                    } else {
                        let ending_at = |index: usize| -> LineEnding {
                            d.get(b"LE")
                                .and_then(|o| o.as_array())
                                .ok()
                                .and_then(|arr| arr.get(index))
                                .and_then(|o| o.as_name_str().ok())
                                .map_or(LineEnding::None, LineEnding::from_pdf_name)
                        };
                        AnnotationKind::Shape {
                            shape_kind: ShapeKind::ArrowLine {
                                start_head: ending_at(0),
                                end_head: ending_at(1),
                            },
                            start_pt,
                            end_pt,
                        }
                    }
                }
                "Text" | "Stamp" => AnnotationKind::StickyNote {
                    comment: if contents.is_empty() {
                        "Note PDF".to_string()
                    } else {
                        contents
                    },
                    is_popup_open: true,
                    anchor_quads: Vec::new(),
                },
                _ => continue,
            };

            loaded.push(PdfAnnotation {
                id: next_id,
                page_index: page_idx,
                rect,
                color_rgb,
                kind,
            });
            next_id += 1;
        }
    }

    Ok(loaded)
}

fn obj_to_f32(obj: &Object) -> Option<f32> {
    match obj {
        Object::Real(v) => Some(*v),
        Object::Integer(i) => Some(*i as f32),
        _ => None,
    }
}

fn parse_pdf_rect(d: &lopdf::Dictionary, page_h: f32) -> Option<PdfTopDownRect> {
    let arr = d.get(b"Rect").ok()?.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let x1 = obj_to_f32(&arr[0])?;
    let y1 = obj_to_f32(&arr[1])?;
    let x2 = obj_to_f32(&arr[2])?;
    let y2 = obj_to_f32(&arr[3])?;
    let llx = x1.min(x2);
    let urx = x1.max(x2);
    let lly = y1.min(y2);
    let ury = y1.max(y2);

    Some(PdfTopDownRect {
        x_min: llx,
        y_min: (page_h - ury).max(0.0),
        x_max: urx,
        y_max: (page_h - lly).max(0.0),
        angle_deg: 0.0,
    })
}

/// Relit les `/QuadPoints` (un rectangle par groupe de 8 nombres).
fn parse_quad_points(d: &lopdf::Dictionary, page_h: f32) -> Option<Vec<PdfTopDownRect>> {
    let values: Vec<f32> = d
        .get(b"QuadPoints")
        .ok()?
        .as_array()
        .ok()?
        .iter()
        .filter_map(obj_to_f32)
        .collect();
    let quads: Vec<PdfTopDownRect> = values
        .chunks_exact(8)
        .map(|q| {
            let (xs, ys) = ([q[0], q[2], q[4], q[6]], [q[1], q[3], q[5], q[7]]);
            let min_of = |v: [f32; 4]| v.into_iter().fold(f32::INFINITY, f32::min);
            let max_of = |v: [f32; 4]| v.into_iter().fold(f32::NEG_INFINITY, f32::max);
            PdfTopDownRect {
                x_min: min_of(xs),
                y_min: page_h - max_of(ys),
                x_max: max_of(xs),
                y_max: page_h - min_of(ys),
                angle_deg: 0.0,
            }
        })
        .collect();
    (!quads.is_empty()).then_some(quads)
}

type OptionalRgb = Option<(u8, u8, u8)>;

/// Fond et contour d'un `/FreeText`. Nos anciens fichiers, sans `/BS`, avaient un fond crème fixe
/// et stockaient le contour dans `/C`.
fn parse_freetext_style(d: &lopdf::Dictionary, color_rgb: (u8, u8, u8)) -> (OptionalRgb, OptionalRgb) {
    let is_ours = d
        .get(b"NM")
        .and_then(|n| n.as_str())
        .is_ok_and(|s| s.starts_with(b"superpdf_"));
    let border_style = d.get(b"BS").and_then(|o| o.as_dict()).ok();
    if is_ours && border_style.is_none() {
        return (Some(FREETEXT_DEFAULT_FILL), Some(color_rgb));
    }

    let border_width = border_style
        .and_then(|bs| bs.get(b"W").ok())
        .and_then(obj_to_f32)
        .unwrap_or(0.0);
    let border_rgb = (border_width > 0.0).then(|| {
        let default_appearance = d
            .get(b"DA")
            .and_then(|o| o.as_str())
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        let tokens: Vec<&str> = default_appearance.split_whitespace().collect();
        tokens
            .iter()
            .position(|tok| *tok == "RG")
            .filter(|i| *i >= 3)
            .map(|i| {
                let channel = |t: &str| (t.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
                (channel(tokens[i - 3]), channel(tokens[i - 2]), channel(tokens[i - 1]))
            })
            .unwrap_or((0, 0, 0))
    });
    (parse_pdf_color(d), border_rgb)
}

fn parse_pdf_color(d: &lopdf::Dictionary) -> Option<(u8, u8, u8)> {
    let arr = d.get(b"C").ok()?.as_array().ok()?;
    if arr.len() < 3 {
        return None;
    }
    let r = (obj_to_f32(&arr[0])?.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (obj_to_f32(&arr[1])?.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (obj_to_f32(&arr[2])?.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_engine::{ensure_sample_pdf, PdfEngine};
    use std::path::PathBuf;

    #[test]
    fn test_annotations_undo_redo_and_translation() {
        let mut mgr = AnnotationManager::new();
        assert_eq!(mgr.items.len(), 0);

        mgr.populate_demo_showcase(0);
        assert_eq!(mgr.items.len(), 6);

        // Test Undo (Ctrl+Z) puis Redo (Ctrl+Y)
        assert!(mgr.undo());
        assert_eq!(mgr.items.len(), 5);
        assert!(mgr.redo());
        assert_eq!(mgr.items.len(), 6);

        // Test Déplacement d'une annotation sélectionnée
        let sel_id = mgr.selected_id.expect("Une annotation doit être sélectionnée");
        let old_x = mgr.selected().unwrap().rect.x_min;
        mgr.selected_mut().unwrap().translate(25.0, -15.0);
        assert!((mgr.selected().unwrap().rect.x_min - (old_x + 25.0)).abs() < 1e-3);

        // Test Suppression
        assert_eq!(mgr.selected_id, Some(sel_id));
        assert!(mgr.delete_selected());
        assert_eq!(mgr.items.len(), 5);
    }

    fn rect(x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> PdfTopDownRect {
        PdfTopDownRect {
            x_min,
            y_min,
            x_max,
            y_max,
            angle_deg: 0.0,
        }
    }

    #[test]
    fn test_arrow_endpoints_move_independently_and_body_moves_both() {
        let mut arrow = PdfAnnotation {
            id: 1,
            page_index: 0,
            rect: rect(0.0, 0.0, 1.0, 1.0),
            color_rgb: (0, 0, 0),
            kind: AnnotationKind::Shape {
                shape_kind: ShapeKind::DEFAULT_ARROW,
                start_pt: (100.0, 100.0),
                end_pt: (200.0, 100.0),
            },
        };
        assert!(arrow.hit_test(150.0, 103.0));
        assert!(!arrow.hit_test(150.0, 130.0), "seul le trait est cliquable");
        assert_eq!(arrow.handle_at(200.0, 101.0, 5.0), Some(AnnotHandle::LineEnd));

        let original = arrow.clone();
        arrow.apply_handle_drag(&original, AnnotHandle::LineEnd, (200.0, 100.0), (260.0, 40.0));
        assert_eq!(arrow.line_endpoints(), Some(((100.0, 100.0), (260.0, 40.0))));

        let original = arrow.clone();
        arrow.apply_handle_drag(&original, AnnotHandle::Body, (150.0, 100.0), (160.0, 120.0));
        assert_eq!(arrow.line_endpoints(), Some(((110.0, 120.0), (270.0, 60.0))));
    }

    #[test]
    fn test_ink_resize_keeps_aspect_ratio_and_opposite_corner() {
        let mut ink = PdfAnnotation {
            id: 1,
            page_index: 0,
            rect: rect(100.0, 100.0, 300.0, 200.0),
            color_rgb: (0, 0, 0),
            kind: AnnotationKind::Ink {
                strokes: vec![vec![(104.0, 104.0), (296.0, 196.0)]],
                thickness: 2.0,
            },
        };
        let original = ink.clone();
        ink.apply_handle_drag(&original, AnnotHandle::Corner(3), (300.0, 200.0), (200.0, 150.0));
        assert!((ink.rect.width() / ink.rect.height() - 2.0).abs() < 1e-3);
        assert!((ink.rect.x_min - 100.0).abs() < 1e-3 && (ink.rect.y_min - 100.0).abs() < 1e-3);
        assert!((ink.rect.x_max - 200.0).abs() < 1e-3);
    }

    #[test]
    fn test_save_in_place_while_open_keeps_arrow_endings_and_accents() {
        let path = PathBuf::from("test_pdfs/unit_test_save_in_place.pdf");
        let _ = std::fs::remove_file(&path);
        ensure_sample_pdf(&path).expect("Doit créer le PDF");
        let engine = PdfEngine::instance();
        let doc = engine.load_document(&path).expect("Doit charger le PDF");
        // Ce second handle simule le thread de rendu, qui garde le fichier ouvert pendant Ctrl+S.
        let held_open = engine.open_lightweight(&path).expect("Doit rouvrir le PDF");

        let annotations = vec![
            PdfAnnotation {
                id: 1,
                page_index: 0,
                rect: rect(100.0, 100.0, 300.0, 200.0),
                color_rgb: (235, 70, 75),
                kind: AnnotationKind::Shape {
                    shape_kind: ShapeKind::ArrowLine {
                        start_head: LineEnding::Circle,
                        end_head: LineEnding::OpenArrow,
                    },
                    start_pt: (110.0, 110.0),
                    end_pt: (290.0, 190.0),
                },
            },
            PdfAnnotation {
                id: 2,
                page_index: 0,
                rect: rect(100.0, 300.0, 300.0, 360.0),
                color_rgb: (45, 135, 245),
                kind: AnnotationKind::FreeText {
                    text: "Déjà vérifié".to_string(),
                    font_size: 11.5,
                    fill_rgb: None,
                    border_rgb: Some((235, 70, 75)),
                },
            },
        ];
        save_annotations_and_ocr_to_pdf(&path, &path, &annotations, &[], &doc.page_dimensions)
            .expect("Doit remplacer le fichier malgré les handles ouverts");

        {
            let _lock = PdfEngine::lock_guard();
            drop(held_open);
        }
        let reloaded = load_annotations_from_pdf(&path, &doc.page_dimensions).expect("Doit relire");
        assert!(reloaded.iter().any(|a| matches!(
            a.kind,
            AnnotationKind::Shape {
                shape_kind: ShapeKind::ArrowLine {
                    start_head: LineEnding::Circle,
                    end_head: LineEnding::OpenArrow,
                },
                ..
            }
        )));
        assert!(reloaded.iter().any(|a| matches!(
            &a.kind,
            AnnotationKind::FreeText { text, fill_rgb: None, border_rgb: Some((235, 70, 75)), .. }
                if text == "Déjà vérifié"
        )));
    }

    #[test]
    fn test_note_linked_to_text_roundtrips_with_its_bubble() {
        let src = PathBuf::from("test_pdfs/unit_test_note_src.pdf");
        let dst = PathBuf::from("test_pdfs/unit_test_note_saved.pdf");
        let _ = std::fs::remove_file(&dst);
        ensure_sample_pdf(&src).expect("Doit créer le PDF");
        let engine = PdfEngine::instance();
        let doc = engine.load_document(&src).expect("Doit charger le PDF");
        let page_dim = doc.page_dimensions[0];

        let anchors = vec![rect(100.0, 200.0, 180.0, 212.0), rect(100.0, 214.0, 150.0, 226.0)];
        let mut note = PdfAnnotation::new_text_note(0, anchors.clone(), (255, 210, 30), page_dim);
        note.id = 7;
        if let AnnotationKind::StickyNote { comment, .. } = &mut note.kind {
            *comment = "À vérifier sur le schéma".to_string();
        }
        assert!(note.hit_test(120.0, 205.0), "le mot associé est cliquable");
        assert!(!note.hit_test(note.rect.x_max - 5.0, note.rect.y_max - 5.0), "bulle fermée inerte");
        let bubble = note.rect;

        save_annotations_and_ocr_to_pdf(&src, &dst, &[note], &[], &doc.page_dimensions)
            .expect("Doit enregistrer");
        let reloaded = load_annotations_from_pdf(&dst, &doc.page_dimensions).expect("Doit relire");
        assert_eq!(reloaded.len(), 1, "le /Popup ne doit pas créer de seconde annotation");
        assert!((reloaded[0].rect.x_min - bubble.x_min).abs() < 0.1);
        assert!((reloaded[0].rect.y_min - bubble.y_min).abs() < 0.1);
        match &reloaded[0].kind {
            AnnotationKind::StickyNote { comment, anchor_quads, .. } => {
                assert_eq!(comment, "À vérifier sur le schéma");
                assert_eq!(anchor_quads.len(), 2);
                assert!((anchor_quads[1].y_min - 214.0).abs() < 0.1);
            }
            other => panic!("note liée attendue, reçu {other:?}"),
        }
    }

    #[test]
    fn test_pdf_text_string_roundtrips_accents() {
        let Object::String(bytes, _) = pdf_text_string("Déjà vérifié œ") else {
            panic!("chaîne attendue");
        };
        assert_eq!(decode_pdf_text(&bytes), "Déjà vérifié œ");
    }

    #[test]
    fn test_iso32000_pdf_annotation_roundtrip_save_and_reload() {
        let src_path = PathBuf::from("test_pdfs/unit_test_annot_src.pdf");
        let dst_path = PathBuf::from("test_pdfs/unit_test_annot_saved.pdf");
        let _ = std::fs::remove_file(&dst_path);
        ensure_sample_pdf(&src_path).expect("Doit créer le PDF source");

        let engine = PdfEngine::instance();
        let doc = engine.load_document(&src_path).expect("Doit charger le PDF");
        let dims = doc.page_dimensions.clone();

        let mut mgr = AnnotationManager::new();
        mgr.populate_demo_showcase(0);

        // Sauvegarde ISO 32000 dans dst_path
        save_annotations_to_pdf(&src_path, &dst_path, &mgr.items, &dims)
            .expect("Doit enregistrer les annotations au format ISO 32000");

        // Relecture depuis dst_path (comme le ferait un autre lecteur ou une nouvelle session)
        let reloaded = load_annotations_from_pdf(&dst_path, &dims)
            .expect("Doit relire les objets /Annot ISO 32000 depuis le PDF");
        assert_eq!(
            reloaded.len(),
            6,
            "Les 6 annotations ISO 32000 (Highlight, FreeText, Arrow, Ink, StickyNote, Measure) doivent être relues"
        );

        let has_freetext = reloaded.iter().any(|a| match &a.kind {
            AnnotationKind::FreeText { text, .. } => text.contains("Adobe Acrobat"),
            _ => false,
        });
        assert!(
            has_freetext,
            "Le bloc /FreeText doit conserver son texte modifiable après sauvegarde/rechargement PDF"
        );
    }
}
