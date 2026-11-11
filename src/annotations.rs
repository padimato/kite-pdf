use crate::ocr_engine::{format_ocr_scanned_pages, OCR_DEEP_PAGES_INFO_KEY, OCR_SCANNED_PAGES_INFO_KEY};
use crate::ocr_engine::{append_invisible_ocr_layer_to_doc, encode_win_ansi};
use crate::pdf_engine::PageDimensions;
use crate::text_search::{PageTextLayer, PdfTopDownRect};
use anyhow::{Context, Result};
use egui::Color32;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::{HashMap, HashSet};
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
    Ellipse,
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
pub const FREETEXT_TEXT_RGB: (u8, u8, u8) = (26, 31, 46);
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
        text_rgb: (u8, u8, u8),
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

/// Réponse dans le fil de discussion d'un commentaire (`/IRT` + `/RT /R`)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommentReply {
    pub author: String,
    pub date: String,
    pub text: String,
    /// Objet d'origine dans le fichier, et texte tel que relu (pour ne réécrire que le nécessaire)
    pub source: Option<ObjectId>,
    pub loaded_text: Option<String>,
}

/// État de révision d'un commentaire (`/StateModel /Review`)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReviewState {
    pub state: String,
    pub author: String,
    pub date: String,
}

/// États proposés par Acrobat : valeur ISO 32000 et libellé.
pub const REVIEW_STATES: [(&str, &str); 5] = [
    ("None", "Aucun"),
    ("Accepted", "Accepté"),
    ("Cancelled", "Annulé"),
    ("Completed", "Terminé"),
    ("Rejected", "Refusé"),
];

/// Informations de révision communes à toutes les annotations, et lien avec le fichier d'origine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnnotationMeta {
    pub author: String,
    pub created: String,
    pub modified: String,
    pub replies: Vec<CommentReply>,
    pub review_state: Option<ReviewState>,
    /// Le commentaire provient d'un simple surlignage : vidé, il redevient un surlignage
    pub was_highlight: bool,
    source: Option<ObjectId>,
    popup_source: Option<ObjectId>,
    state_sources: Vec<ObjectId>,
    loaded_state: Option<ReviewState>,
    loaded_snapshot: Option<String>,
}

/// Nom d'auteur par défaut : le compte Windows, comme le fait Acrobat.
pub fn default_author() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "Utilisateur".to_string())
}

/// Date courante au format PDF (`D:AAAAMMJJHHmmSS+01'00'`).
pub fn pdf_date_now() -> String {
    let now = chrono::Local::now();
    let offset = now.format("%z").to_string();
    let (hours, minutes) = offset.split_at(3);
    format!("D:{}{hours}'{minutes}'", now.format("%Y%m%d%H%M%S"))
}

/// Date PDF lisible : l'heure si c'est aujourd'hui, sinon la date complète.
pub fn format_pdf_date(date: &str) -> String {
    let digits: String = date
        .strip_prefix("D:")
        .unwrap_or(date)
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.len() < 12 {
        return String::new();
    }
    let (year, month, day, hour, minute) = (
        &digits[0..4],
        &digits[4..6],
        &digits[6..8],
        &digits[8..10],
        &digits[10..12],
    );
    if chrono::Local::now().format("%Y%m%d").to_string() == digits[0..8] {
        format!("{hour}:{minute}")
    } else {
        format!("{day}/{month}/{year} {hour}:{minute}")
    }
}

/// Annotation PDF unifiée (en coordonnées points PDF top-down pour l'affichage et convertie en bottom-up ISO 32000 à l'enregistrement)
#[derive(Debug, Clone, PartialEq)]
pub struct PdfAnnotation {
    pub id: u64,
    pub page_index: usize,
    pub rect: PdfTopDownRect,
    pub color_rgb: (u8, u8, u8),
    pub kind: AnnotationKind,
    pub meta: AnnotationMeta,
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
                shape_kind: ShapeKind::Ellipse,
                ..
            } => "Ellipse (/Circle)",
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

    /// Surlignage ou note liée : solidaire du texte, jamais déplacé à la souris.
    pub fn is_fixed_to_text(&self) -> bool {
        match &self.kind {
            AnnotationKind::Highlight { .. } => true,
            AnnotationKind::StickyNote { anchor_quads, .. } => !anchor_quads.is_empty(),
            _ => false,
        }
    }

    /// Transforme un surlignage en commentaire : même annotation dans le fichier, avec un texte.
    pub fn convert_highlight_to_note(&mut self, page_dim: PageDimensions) {
        if let AnnotationKind::Highlight { quads } = &self.kind {
            let anchor_quads = quads.clone();
            self.rect = default_note_bubble(&anchor_quads, page_dim);
            self.kind = AnnotationKind::StickyNote {
                comment: String::new(),
                is_popup_open: true,
                anchor_quads,
            };
            self.meta.was_highlight = true;
        }
    }

    /// Commentaire sans texte ni réponse : il n'a pas lieu d'être conservé.
    pub fn is_empty_comment(&self) -> bool {
        matches!(&self.kind, AnnotationKind::StickyNote { comment, .. } if comment.trim().is_empty())
            && self.meta.replies.is_empty()
    }

    /// Redonne à un commentaire vide son état de simple surlignage.
    pub fn revert_empty_comment_to_highlight(&mut self) {
        if let AnnotationKind::StickyNote { anchor_quads, .. } = &self.kind {
            let quads = anchor_quads.clone();
            if let Some(first) = quads.first() {
                self.rect = quads.iter().fold(*first, |acc, q| acc.union(q));
            }
            self.kind = AnnotationKind::Highlight { quads };
            self.meta.was_highlight = false;
        }
    }

    fn content_fingerprint(&self) -> String {
        format!("{:?}|{:?}|{:?}", self.rect, self.color_rgb, self.kind)
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
            meta: AnnotationMeta::default(),
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
        if self.is_fixed_to_text() {
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
            shape_kind: ShapeKind::Rectangle | ShapeKind::Ellipse,
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

    /// Annotations relues du fichier : elles ne font pas partie de l'historique d'annulation.
    /// Ajoute les annotations relues du fichier en tâche de fond, devant celles que l'utilisateur
    /// a pu créer entre-temps (nouveaux identifiants pour éviter tout doublon).
    pub fn merge_loaded(&mut self, loaded: Vec<PdfAnnotation>) {
        if self.items.is_empty() {
            self.load_items(loaded);
            return;
        }
        let renumbered: Vec<PdfAnnotation> = loaded
            .into_iter()
            .map(|mut item| {
                item.id = self.alloc_id();
                item
            })
            .collect();
        self.items.splice(0..0, renumbered);
    }

    pub fn load_items(&mut self, items: Vec<PdfAnnotation>) {
        self.next_id = items.iter().map(|a| a.id + 1).max().unwrap_or(1).max(self.next_id);
        self.items = items;
        self.selected_id = None;
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
        if annot.meta.author.is_empty() {
            annot.meta.author = default_author();
        }
        if annot.meta.created.is_empty() {
            annot.meta.created = pdf_date_now();
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

    #[cfg(test)]
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
            meta: AnnotationMeta::default(),
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
            meta: AnnotationMeta::default(),
            id: 0,
            page_index,
            rect: ft_rect,
            color_rgb: (45, 135, 245),
            kind: AnnotationKind::FreeText {
                text: "Bloc FreeText ISO 32000\nModifiable dans Adobe Acrobat !".to_string(),
                font_size: 11.5,
                text_rgb: FREETEXT_TEXT_RGB,
                fill_rgb: Some(FREETEXT_DEFAULT_FILL),
                border_rgb: Some((45, 135, 245)),
            },
        });

        // 3. Flèche et Rectangle d'annotation sur le graphique (/Square & /Line)
        self.add_annotation(PdfAnnotation {
            meta: AnnotationMeta::default(),
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
            meta: AnnotationMeta::default(),
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
            meta: AnnotationMeta::default(),
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
            meta: AnnotationMeta::default(),
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
    save_pdf_with_ocr_state(source_path, dest_path, annotations, text_layers, page_dimensions, &[], &[], &[])
}

/// Comme [`save_annotations_and_ocr_to_pdf`], en notant en plus dans le fichier les pages déjà
/// passées à l'OCR (`ocr_scanned_pages`) et à l'OCR approfondi (`deep_ocr_pages`). Une liste vide
/// laisse le marqueur existant tel quel.
pub fn save_pdf_with_ocr_state(
    source_path: &Path,
    dest_path: &Path,
    annotations: &[PdfAnnotation],
    text_layers: &[PageTextLayer],
    page_dimensions: &[PageDimensions],
    ocr_scanned_pages: &[bool],
    deep_ocr_pages: &[bool],
    form_edits: &[(String, FormEdit)],
) -> Result<()> {
    let mut doc = Document::load(source_path).with_context(|| {
        format!(
            "Impossible de charger le PDF source pour sauvegarde : {}",
            source_path.display()
        )
    })?;
    strip_usage_rights(&mut doc);
    // La bibliothèque d'écriture ne sait pas reprotéger le fichier : l'enregistrer retirerait la
    // protection sans prévenir.
    if doc.is_encrypted() {
        anyhow::bail!(
            "Ce PDF est protégé (mot de passe ou restrictions) : l'enregistrement des modifications \
             n'est pas encore pris en charge pour ces documents."
        );
    }

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
        let page_dim = page_dimensions
            .get(page_idx)
            .copied()
            .unwrap_or(PageDimensions::simple(595.0, 842.0));
        let page_annots: Vec<&PdfAnnotation> = annotations
            .iter()
            .filter(|a| a.page_index == page_idx)
            .collect();

        // Objets du fichier que l'utilisateur a conservés : fils de discussion compris.
        let mut kept: HashSet<ObjectId> = HashSet::new();
        for annot in &page_annots {
            kept.extend(annot.meta.source);
            kept.extend(annot.meta.popup_source);
            kept.extend(annot.meta.state_sources.iter().copied());
            kept.extend(annot.meta.replies.iter().filter_map(|r| r.source));
        }

        let mut annot_refs: Vec<Object> = page_annotation_items(&doc, page_obj_id)
            .into_iter()
            .filter(|item| existing_annotation_survives(&doc, item, &kept))
            .collect();

        for annot in page_annots {
            let parent_id = match annot.meta.source {
                Some(source) => {
                    let unchanged = annot.meta.loaded_snapshot.as_deref()
                        == Some(annot.content_fingerprint().as_str());
                    if !unchanged {
                        let rebuilt = build_iso32000_annot_objects(
                            &mut doc,
                            annot,
                            page_dim,
                            page_obj_id,
                            helv_font_id,
                        )?;
                        annot_refs.extend(patch_existing_annotation(
                            &mut doc,
                            source,
                            annot.meta.popup_source,
                            &rebuilt,
                        ));
                    }
                    source
                }
                None => {
                    let rebuilt = build_iso32000_annot_objects(
                        &mut doc,
                        annot,
                        page_dim,
                        page_obj_id,
                        helv_font_id,
                    )?;
                    annot_refs.extend(rebuilt.iter().copied().map(Object::Reference));
                    rebuilt[0]
                }
            };
            annot_refs.extend(
                write_comment_thread(&mut doc, annot, parent_id, page_obj_id)
                    .into_iter()
                    .map(Object::Reference),
            );
        }

        if let Ok(page_dict) = doc.get_object_mut(page_obj_id).and_then(|o| o.as_dict_mut()) {
            page_dict.set("Annots", Object::Array(annot_refs));
        }
    }

    // Injection de la couche de texte OCR invisible normalisée (`3 Tr` + `Tm`) pour les images embarquées scannées
    if !text_layers.is_empty() {
        let _ = append_invisible_ocr_layer_to_doc(&mut doc, &pages_map, text_layers, page_dimensions);
    }
    if ocr_scanned_pages.iter().any(|&s| s) {
        record_ocr_pages(&mut doc, OCR_SCANNED_PAGES_INFO_KEY, ocr_scanned_pages);
    }
    if deep_ocr_pages.iter().any(|&s| s) {
        record_ocr_pages(&mut doc, OCR_DEEP_PAGES_INFO_KEY, deep_ocr_pages);
    }
    apply_form_edits(&mut doc, form_edits);

    // Écriture dans un fichier voisin puis remplacement atomique : une panne en cours d'écriture
    // ne peut pas corrompre l'original, et les lectures PDFium en cours restent valides.
    let temp_path = dest_path.with_extension("kite-save.tmp");
    forget_previous_revisions(&mut doc);
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

/// Retire les « droits d'utilisation étendus » d'Adobe (`/Perms /UR3`) : cette signature ne vaut que
/// pour le fichier d'origine. Une fois le fichier modifié, Acrobat la trouve invalide et verrouille le
/// formulaire. Sans elle, Acrobat (Reader compris) laisse remplir et enregistrer normalement.
fn strip_usage_rights(doc: &mut Document) {
    let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
    let perms = match doc.get_object(catalog_id).and_then(Object::as_dict).and_then(|c| c.get(b"Perms")) {
        Ok(Object::Reference(id)) => Some(*id),
        Ok(Object::Dictionary(_)) => None,
        _ => return,
    };
    let remaining = {
        let dict = match perms {
            Some(id) => doc.get_object_mut(id).and_then(Object::as_dict_mut),
            None => doc
                .get_object_mut(catalog_id)
                .and_then(Object::as_dict_mut)
                .and_then(|c| c.get_mut(b"Perms"))
                .and_then(Object::as_dict_mut),
        };
        let Ok(dict) = dict else { return };
        dict.remove(b"UR3");
        dict.remove(b"UR");
        dict.len()
    };
    if remaining == 0 {
        remove_dict_entry(doc, catalog_id, b"Perms");
    }
}

/// Valeur saisie dans un champ de formulaire, pas encore enregistrée.
#[derive(Debug, Clone, PartialEq)]
pub enum FormEdit {
    /// Champ texte ou liste déroulante
    Text(String),
    Checked(bool),
    /// Bouton radio sélectionné, par sa position dans le groupe
    Radio(u32),
}

/// Écrit les valeurs saisies dans les champs du formulaire. Les apparences des champs modifiés sont
/// retirées et `/NeedAppearances` demande aux lecteurs (PDFium, Acrobat) de les redessiner.
fn apply_form_edits(doc: &mut Document, edits: &[(String, FormEdit)]) {
    if edits.is_empty() {
        return;
    }
    let Some(acroform_id) = acroform_id(doc) else { return };
    let mut fields: Vec<(String, ObjectId)> = Vec::new();
    let roots: Vec<Object> = doc
        .get_object(acroform_id)
        .and_then(Object::as_dict)
        .and_then(|d| d.get(b"Fields"))
        .and_then(|f| match f {
            Object::Reference(id) => doc.get_object(*id).and_then(Object::as_array).cloned(),
            other => other.as_array().cloned(),
        })
        .unwrap_or_default();
    for root in roots {
        if let Object::Reference(id) = root {
            collect_fields(doc, id, String::new(), &mut fields, 0);
        }
    }
    for (name, edit) in edits {
        let Some(&(_, field_id)) = fields.iter().find(|(n, _)| n == name) else { continue };
        let widgets = field_widgets(doc, field_id);
        match edit {
            FormEdit::Text(value) => {
                set_dict_entry(doc, field_id, "V", pdf_text_string(value));
                for widget in &widgets {
                    remove_dict_entry(doc, *widget, b"AP");
                }
                remove_dict_entry(doc, field_id, b"AP");
            }
            FormEdit::Checked(checked) => {
                let on = widgets.first().map(|w| on_state_name(doc, *w)).unwrap_or_else(|| "Yes".into());
                let state = if *checked { on.clone() } else { "Off".to_string() };
                set_dict_entry(doc, field_id, "V", Object::Name(state.clone().into_bytes()));
                for widget in &widgets {
                    let widget_on = on_state_name(doc, *widget);
                    let widget_state = if *checked { widget_on } else { "Off".to_string() };
                    set_dict_entry(doc, *widget, "AS", Object::Name(widget_state.into_bytes()));
                }
            }
            FormEdit::Radio(index) => {
                let Some(&chosen) = widgets.get(*index as usize) else { continue };
                let on = on_state_name(doc, chosen);
                set_dict_entry(doc, field_id, "V", Object::Name(on.clone().into_bytes()));
                for widget in &widgets {
                    let state = if *widget == chosen { on_state_name(doc, *widget) } else { "Off".to_string() };
                    set_dict_entry(doc, *widget, "AS", Object::Name(state.into_bytes()));
                }
            }
        }
    }
    set_dict_entry(doc, acroform_id, "NeedAppearances", Object::Boolean(true));
    // Formulaire « hybride » : Acrobat lirait d'abord sa description XFA, qui garde les anciennes
    // valeurs. Sans elle, il lit les champs remplis ici, comme tous les autres lecteurs.
    remove_dict_entry(doc, acroform_id, b"XFA");
}

/// `/AcroForm` du catalogue, rendu indirect s'il était écrit en place.
fn acroform_id(doc: &mut Document) -> Option<ObjectId> {
    let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference).ok()?;
    let acroform = doc.get_object(catalog_id).and_then(Object::as_dict).ok()?.get(b"AcroForm").ok()?.clone();
    match acroform {
        Object::Reference(id) => Some(id),
        Object::Dictionary(dict) => {
            let id = doc.add_object(dict);
            set_dict_entry(doc, catalog_id, "AcroForm", Object::Reference(id));
            Some(id)
        }
        _ => None,
    }
}

/// Champs terminaux (ceux qui portent une valeur) avec leur nom complet `parent.enfant`.
fn collect_fields(doc: &Document, id: ObjectId, parent: String, out: &mut Vec<(String, ObjectId)>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Ok(dict) = doc.get_object(id).and_then(Object::as_dict) else { return };
    let partial = dict.get(b"T").ok().and_then(|t| match t {
        Object::String(bytes, _) => Some(decode_pdf_text(bytes)),
        _ => None,
    });
    let name = match (&partial, parent.is_empty()) {
        (Some(p), true) => p.clone(),
        (Some(p), false) => format!("{parent}.{p}"),
        (None, _) => parent.clone(),
    };
    let named_kids: Vec<ObjectId> = dict
        .get(b"Kids")
        .and_then(Object::as_array)
        .map(|kids| {
            kids.iter()
                .filter_map(|k| k.as_reference().ok())
                .filter(|k| doc.get_object(*k).and_then(Object::as_dict).is_ok_and(|d| d.has(b"T")))
                .collect()
        })
        .unwrap_or_default();
    if named_kids.is_empty() {
        if partial.is_some() {
            out.push((name, id));
        }
    } else {
        for kid in named_kids {
            collect_fields(doc, kid, name.clone(), out, depth + 1);
        }
    }
}

/// Emplacements (widgets) d'un champ : ses enfants sans nom, ou le champ lui-même.
fn field_widgets(doc: &Document, field_id: ObjectId) -> Vec<ObjectId> {
    let kids: Vec<ObjectId> = doc
        .get_object(field_id)
        .and_then(Object::as_dict)
        .and_then(|d| d.get(b"Kids"))
        .and_then(Object::as_array)
        .map(|kids| kids.iter().filter_map(|k| k.as_reference().ok()).collect())
        .unwrap_or_default();
    if kids.is_empty() {
        vec![field_id]
    } else {
        kids
    }
}

/// Nom de l'état « coché » d'un widget : la clé de `/AP /N` autre que `Off`.
fn on_state_name(doc: &Document, widget: ObjectId) -> String {
    let normal = doc
        .get_object(widget)
        .and_then(Object::as_dict)
        .and_then(|d| d.get(b"AP"))
        .and_then(|ap| match ap {
            Object::Reference(id) => doc.get_object(*id),
            other => Ok(other),
        })
        .and_then(Object::as_dict)
        .and_then(|ap| ap.get(b"N"))
        .and_then(|n| match n {
            Object::Reference(id) => doc.get_object(*id),
            other => Ok(other),
        })
        .and_then(Object::as_dict);
    normal
        .ok()
        .and_then(|n| n.iter().map(|(k, _)| String::from_utf8_lossy(k).into_owned()).find(|k| k != "Off"))
        .unwrap_or_else(|| "Yes".to_string())
}

fn set_dict_entry(doc: &mut Document, id: ObjectId, key: &str, value: Object) {
    if let Ok(dict) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
        dict.set(key, value);
    }
}

fn remove_dict_entry(doc: &mut Document, id: ObjectId, key: &[u8]) {
    if let Ok(dict) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
        dict.remove(key);
    }
}

/// Note dans le dictionnaire `/Info` une liste de pages traitées par l'OCR (clé privée, ignorée des
/// autres lecteurs).
fn record_ocr_pages(doc: &mut Document, key: &str, scanned: &[bool]) {
    let value = Object::string_literal(format_ocr_scanned_pages(scanned));
    let info_ref = match doc.trailer.get(b"Info") {
        Ok(Object::Reference(id)) => Some(*id),
        _ => None,
    };
    if let Some(id) = info_ref {
        if let Ok(info) = doc.get_object_mut(id).and_then(Object::as_dict_mut) {
            info.set(key, value);
            return;
        }
    }
    if let Ok(Object::Dictionary(info)) = doc.trailer.get_mut(b"Info") {
        info.set(key, value);
        return;
    }
    let mut info = lopdf::Dictionary::new();
    info.set(key, value);
    let id = doc.add_object(info);
    doc.trailer.set("Info", Object::Reference(id));
}

/// Le fichier est réécrit d'un seul tenant : garder `/Prev` ferait pointer sa table de références
/// vers un décalage périmé, et Acrobat devrait réparer le PDF à chaque ouverture.
fn forget_previous_revisions(doc: &mut Document) {
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");
    renew_document_id(doc);
}

/// Renouvelle le second identifiant de `/ID` (ISO 32000-1 §14.4 : il change à chaque enregistrement).
/// Acrobat garde en cache la position du texte d'un document d'après son `/ID` : sans cela, sa
/// recherche surligne la couche OCR à l'emplacement d'une version précédente du fichier.
fn renew_document_id(doc: &mut Document) {
    use std::hash::{BuildHasher, Hasher};
    let mut fresh = Vec::with_capacity(16);
    for _ in 0..2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        fresh.extend_from_slice(&hasher.finish().to_be_bytes());
    }
    let fresh = Object::String(fresh, lopdf::StringFormat::Hexadecimal);
    let permanent = match doc.trailer.get(b"ID").and_then(Object::as_array) {
        Ok(ids) if !ids.is_empty() => ids[0].clone(),
        _ => fresh.clone(),
    };
    doc.trailer.set("ID", Object::Array(vec![permanent, fresh]));
}

/// Chaîne de texte PDF : UTF-16BE avec BOM dès qu'un caractère sort de l'ASCII (ISO 32000-1 §7.9.2.2).
fn pdf_text_string(text: &str) -> Object {
    // Acrobat sépare les lignes par `\r` : un `\n` disparaît dans son panneau Commentaires.
    let text = text.replace("\r\n", "\n").replace('\n', "\r");
    if text.is_ascii() {
        return Object::string_literal(text);
    }
    let mut bytes = vec![0xFE, 0xFF];
    bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    Object::String(bytes, lopdf::StringFormat::Hexadecimal)
}

fn decode_pdf_text(bytes: &[u8]) -> String {
    let text = if let Some(utf16) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        // Les fichiers écrits par les versions précédentes contiennent de l'UTF-8 brut.
        String::from_utf8(bytes.to_vec())
            .unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect())
    };
    text.replace("\r\n", "\n").replace('\r', "\n")
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

/// Liste des annotations d'une page, que `/Annots` soit direct ou indirect.
fn page_annotation_items(doc: &Document, page_id: ObjectId) -> Vec<Object> {
    let Ok(page_dict) = doc.get_object(page_id).and_then(|o| o.as_dict()) else {
        return Vec::new();
    };
    match page_dict.get(b"Annots") {
        Ok(Object::Reference(id)) => doc
            .get_object(*id)
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default(),
        Ok(Object::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

fn annotation_flags(d: &lopdf::Dictionary) -> i64 {
    d.get(b"F").and_then(|o| o.as_i64()).unwrap_or(0)
}

const ANNOTATION_FLAG_HIDDEN: i64 = 2;

/// Annotation reprise par l'application (affichée, éditable, et donc réécrite si elle change).
/// Tout le reste (liens, formulaires, tampons, soulignés...) traverse la sauvegarde sans être touché.
fn is_app_managed(d: &lopdf::Dictionary) -> bool {
    if d.has(b"IRT") {
        return true;
    }
    let subtype = d.get(b"Subtype").and_then(|o| o.as_name_str()).unwrap_or("");
    matches!(
        subtype,
        "Highlight" | "FreeText" | "Ink" | "Square" | "Circle" | "Line" | "Text"
    ) && annotation_flags(d) & ANNOTATION_FLAG_HIDDEN == 0
}

fn existing_annotation_survives(doc: &Document, item: &Object, kept: &HashSet<ObjectId>) -> bool {
    let (id, dict) = match item {
        Object::Reference(id) => match doc.get_object(*id).and_then(|o| o.as_dict()) {
            Ok(d) => (Some(*id), d),
            Err(_) => return true,
        },
        Object::Dictionary(d) => (None, d),
        _ => return true,
    };
    if dict.get(b"Subtype").and_then(|o| o.as_name_str()).is_ok_and(|s| s == "Popup") {
        let Ok(parent) = dict.get(b"Parent").and_then(|o| o.as_reference()) else {
            return true;
        };
        if kept.contains(&parent) {
            return true;
        }
        return !doc
            .get_object(parent)
            .and_then(|o| o.as_dict())
            .is_ok_and(is_app_managed);
    }
    !is_app_managed(dict) || id.is_some_and(|id| kept.contains(&id))
}

/// Reporte une annotation modifiée sur l'objet d'origine. Les clés que l'application ne gère pas
/// (`/RC`, `/Subj`, `/IRT`, identifiants...) sont conservées, et les réponses restent attachées.
/// Retourne les références nouvelles à ajouter à la page.
fn patch_existing_annotation(
    doc: &mut Document,
    source: ObjectId,
    popup_source: Option<ObjectId>,
    rebuilt: &[ObjectId],
) -> Vec<Object> {
    let Some(new_dict) = doc
        .get_object(rebuilt[0])
        .and_then(|o| o.as_dict())
        .ok()
        .cloned()
    else {
        return Vec::new();
    };
    const KEYS_KEPT_FROM_ORIGINAL: [&[u8]; 8] = [
        b"NM",
        b"Popup",
        b"Name",
        b"T",
        b"CreationDate",
        b"F",
        b"P",
        b"Open",
    ];
    let new_popup_rect = rebuilt
        .get(1)
        .and_then(|id| doc.get_object(*id).ok())
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Rect").ok())
        .cloned();
    let mut extra_refs = Vec::new();
    if let Ok(original) = doc.get_object_mut(source).and_then(|o| o.as_dict_mut()) {
        for (key, value) in new_dict.iter() {
            if !KEYS_KEPT_FROM_ORIGINAL.contains(&key.as_slice()) {
                original.set(key.clone(), value.clone());
            }
        }
        if !new_dict.has(b"C") {
            original.remove(b"C");
        }
    }
    doc.objects.remove(&rebuilt[0]);

    if let (Some(&new_popup), Some(rect)) = (rebuilt.get(1), new_popup_rect) {
        match popup_source {
            Some(existing) => {
                if let Ok(popup) = doc.get_object_mut(existing).and_then(|o| o.as_dict_mut()) {
                    popup.set("Rect", rect);
                }
                doc.objects.remove(&new_popup);
            }
            None => {
                if let Ok(popup) = doc.get_object_mut(new_popup).and_then(|o| o.as_dict_mut()) {
                    popup.set("Parent", source);
                }
                if let Ok(original) = doc.get_object_mut(source).and_then(|o| o.as_dict_mut()) {
                    original.set("Popup", new_popup);
                }
                extra_refs.push(Object::Reference(new_popup));
            }
        }
    }
    extra_refs
}

/// Écrit les réponses et l'état de révision nouveaux ou modifiés.
fn write_comment_thread(
    doc: &mut Document,
    annot: &PdfAnnotation,
    parent_id: ObjectId,
    page_id: ObjectId,
) -> Vec<ObjectId> {
    let parent_rect = doc
        .get_object(parent_id)
        .and_then(|o| o.as_dict())
        .and_then(|d| d.get(b"Rect"))
        .cloned()
        .unwrap_or(Object::Array(Vec::new()));
    let color = {
        let (r, g, b) = annot.color_rgb;
        vec![
            Object::Real(r as f32 / 255.0),
            Object::Real(g as f32 / 255.0),
            Object::Real(b as f32 / 255.0),
        ]
    };
    let mut created = Vec::new();

    let thread_entry = |nm: String, author: &str, date: &str, flags: i64, contents: &str| {
        let mut dict = lopdf::Dictionary::new();
        dict.set("Type", Object::Name(b"Annot".to_vec()));
        dict.set("Subtype", Object::Name(b"Text".to_vec()));
        dict.set("Rect", parent_rect.clone());
        dict.set("C", Object::Array(color.clone()));
        dict.set("F", flags);
        dict.set("Name", Object::Name(b"Comment".to_vec()));
        // Pas de `/Open` ici : Acrobat ne reconnaît plus l'état de révision s'il est présent.
        dict.set("P", page_id);
        dict.set("IRT", parent_id);
        dict.set("NM", Object::string_literal(nm));
        dict.set("T", pdf_text_string(author));
        dict.set("M", Object::string_literal(date));
        dict.set("CreationDate", Object::string_literal(date));
        dict.set("Contents", pdf_text_string(contents));
        dict
    };

    for (index, reply) in annot.meta.replies.iter().enumerate() {
        match reply.source {
            Some(source) => {
                if reply.loaded_text.as_deref() != Some(reply.text.as_str()) {
                    if let Ok(existing) = doc.get_object_mut(source).and_then(|o| o.as_dict_mut()) {
                        existing.set("Contents", pdf_text_string(&reply.text));
                        existing.set("M", Object::string_literal(pdf_date_now()));
                        existing.remove(b"RC");
                    }
                }
            }
            None => {
                let mut dict = thread_entry(
                    format!("superpdf_reply_{}_{index}", annot.id),
                    &reply.author,
                    &reply.date,
                    ANNOTATION_FLAGS_REPLY,
                    &reply.text,
                );
                dict.set("RT", Object::Name(b"R".to_vec()));
                created.push(doc.add_object(Object::Dictionary(dict)));
            }
        }
    }

    if annot.meta.review_state != annot.meta.loaded_state {
        if let Some(review) = &annot.meta.review_state {
            let mut dict = thread_entry(
                format!("superpdf_state_{}", annot.id),
                &review.author,
                &review.date,
                ANNOTATION_FLAGS_STATE,
                &format!("{} set by {}", review.state, review.author),
            );
            dict.set("State", Object::string_literal(review.state.as_str()));
            dict.set("StateModel", Object::string_literal("Review"));
            created.push(doc.add_object(Object::Dictionary(dict)));
        }
    }
    created
}

/// Print + NoZoom + NoRotate, comme les réponses écrites par Acrobat.
const ANNOTATION_FLAGS_REPLY: i64 = 28;
/// Idem + Hidden : un état de révision n'a pas d'existence visuelle propre.
const ANNOTATION_FLAGS_STATE: i64 = 30;

/// Marquage d'un surlignage : teinte chaque ligne en mode multiplicatif pour laisser le texte lisible.
fn add_highlight_markup(
    doc: &mut Document,
    dict: &mut lopdf::Dictionary,
    quads: &[PdfTopDownRect],
    (r, g, b): (f32, f32, f32),
    origin: (f32, f32),
    size: (f32, f32),
    page_dim: PageDimensions,
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
        // Ordre ISO 32000 QuadPoints : haut-gauche, haut-droit, bas-gauche, bas-droit du texte affiché.
        let corners = [
            page_dim.view_to_pdf(q.x_min, q.y_min),
            page_dim.view_to_pdf(q.x_max, q.y_min),
            page_dim.view_to_pdf(q.x_min, q.y_max),
            page_dim.view_to_pdf(q.x_max, q.y_max),
        ];
        let mut x_range = (f32::MAX, f32::MIN);
        let mut y_range = (f32::MAX, f32::MIN);
        for (x, y) in corners {
            quad_points.push(x.into());
            quad_points.push(y.into());
            x_range = (x_range.0.min(x), x_range.1.max(x));
            y_range = (y_range.0.min(y), y_range.1.max(y));
        }
        ops.push(Operation::new(
            "re",
            vec![
                (x_range.0 - origin.0).into(),
                (y_range.0 - origin.1).into(),
                (x_range.1 - x_range.0).into(),
                (y_range.1 - y_range.0).into(),
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

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Texte enrichi `/RC` : c'est lui qu'Acrobat recharge lorsqu'on réédite le bloc.
fn freetext_rich_contents(text: &str, font_size: f32, color_hex: &str) -> String {
    // Un seul paragraphe avec `&#13;` : Acrobat colle les `<p>` successifs dans son panneau Commentaires.
    let lines: Vec<String> = text.lines().map(escape_xml).collect();
    let paragraphs = format!(
        "<p dir=\"ltr\"><span style=\"font-family:Helvetica\">{}</span></p>",
        lines.join("&#13;")
    );
    format!(
        "<?xml version=\"1.0\"?><body xmlns=\"http://www.w3.org/1999/xhtml\" \
         xmlns:xfa=\"http://www.xfa.org/schema/xfa-data/1.0/\" xfa:APIVersion=\"Acrobat:11.0.0\" \
         xfa:spec=\"2.0.2\" style=\"font-size:{font_size:.1}pt;text-align:left;color:{color_hex};\
         font-weight:normal;font-style:normal;font-family:Helvetica;font-stretch:normal\">{paragraphs}</body>"
    )
}

fn build_iso32000_annot_objects(
    doc: &mut Document,
    annot: &PdfAnnotation,
    page_dim: PageDimensions,
    page_id: ObjectId,
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

    let pdf_rect_of = |rect: &PdfTopDownRect| -> [f32; 4] {
        let (ax, ay) = page_dim.view_to_pdf(rect.x_min, rect.y_min);
        let (bx, by) = page_dim.view_to_pdf(rect.x_max, rect.y_max);
        [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)]
    };
    let [llx, lly, raw_urx, raw_ury] = pdf_rect_of(&annot.rect);
    let urx = raw_urx.max(llx + 4.0);
    let ury = raw_ury.max(lly + 4.0);
    let to_ap = |p: (f32, f32)| {
        let (x, y) = page_dim.view_to_pdf(p.0, p.1);
        (x - llx, y - lly)
    };

    let r = annot.color_rgb.0 as f32 / 255.0;
    let g = annot.color_rgb.1 as f32 / 255.0;
    let b = annot.color_rgb.2 as f32 / 255.0;
    let now = pdf_date_now();

    let mut dict = lopdf::Dictionary::new();
    dict.set("Type", Object::Name(b"Annot".to_vec()));
    dict.set(
        "Rect",
        vec![llx.into(), lly.into(), urx.into(), ury.into()],
    );
    dict.set("C", vec![r.into(), g.into(), b.into()]);
    dict.set("F", 4); // Bit 3 = Print (ISO 32000-1 Table 165)
    dict.set("P", page_id);
    dict.set(
        "NM",
        Object::string_literal(format!("superpdf_{}", annot.id)),
    );
    let author = if annot.meta.author.is_empty() {
        default_author()
    } else {
        annot.meta.author.clone()
    };
    dict.set("T", pdf_text_string(&author));
    dict.set("M", Object::string_literal(now.as_str()));
    let created = if annot.meta.created.is_empty() {
        now.as_str()
    } else {
        annot.meta.created.as_str()
    };
    dict.set("CreationDate", Object::string_literal(created));

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
                page_dim,
            )?;
        }
        AnnotationKind::FreeText {
            text,
            font_size,
            text_rgb,
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
            // Le contenu est dessiné dans le repère affiché : `/Matrix` le redresse sur une page tournée.
            let (view_w, view_h) = (annot.rect.width().max(4.0), annot.rect.height().max(4.0));
            let (text_r, text_g, text_b) = to_unit(*text_rgb);
            let text_hex = format!("#{:02X}{:02X}{:02X}", text_rgb.0, text_rgb.1, text_rgb.2);

            dict.set("Subtype", Object::Name(b"FreeText".to_vec()));
            dict.set("Contents", pdf_text_string(text));
            // Convention Acrobat : `/C` porte le fond, la dernière couleur `rg` de `/DA` le contour,
            // `/DS` et `/RC` la police et la couleur du texte.
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
            let (da_r, da_g, da_b) = border_rgb.map_or((text_r, text_g, text_b), to_unit);
            dict.set(
                "DA",
                Object::string_literal(format!(
                    "{da_r:.3} {da_g:.3} {da_b:.3} rg /Helv {font_size:.1} Tf"
                )),
            );
            dict.set(
                "DS",
                Object::string_literal(format!(
                    "font: Helvetica {font_size:.1}pt; text-align:left; color:{text_hex}"
                )),
            );
            dict.set(
                "RC",
                pdf_text_string(&freetext_rich_contents(text, *font_size, &text_hex)),
            );
            if page_dim.rotation_deg != 0 {
                dict.set("Rotate", page_dim.rotation_deg as i64);
            }

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
                vec![0.75.into(), 0.75.into(), (view_w - 1.5).into(), (view_h - 1.5).into()],
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
            for (line_idx, line) in wrap_helvetica_lines(text, view_w - 12.0, *font_size)
                .iter()
                .enumerate()
            {
                let y_line = view_h - 4.0 - font_size - line_idx as f32 * line_height;
                if y_line < 1.0 {
                    break;
                }
                ap_ops.push(Operation::new("BT", vec![]));
                ap_ops.push(Operation::new(
                    "rg",
                    vec![text_r.into(), text_g.into(), text_b.into()],
                ));
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

            let ap_id = create_appearance_stream(doc, view_w, view_h, ap_ops, Some(helv_font_id))?;
            let matrix: Option<[i64; 4]> = match page_dim.rotation_deg {
                90 => Some([0, 1, -1, 0]),
                180 => Some([-1, 0, 0, -1]),
                270 => Some([0, -1, 1, 0]),
                _ => None,
            };
            if let Some([a, b, c, d]) = matrix {
                if let Ok(stream) = doc.get_object_mut(ap_id).and_then(|o| o.as_stream_mut()) {
                    stream.dict.set(
                        "Matrix",
                        vec![a.into(), b.into(), c.into(), d.into(), 0.into(), 0.into()],
                    );
                }
            }
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::Ink { strokes, thickness } => {
            dict.set("Subtype", Object::Name(b"Ink".to_vec()));
            dict.set("BS", dictionary! { "W" => *thickness });
            let mut ink_list: Vec<Object> = Vec::with_capacity(strokes.len());
            let mut ap_ops = vec![
                Operation::new("q", vec![]),
                Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                Operation::new("w", vec![(*thickness).into()]),
                Operation::new("J", vec![1.into()]),
                Operation::new("j", vec![1.into()]),
            ];

            for stroke in strokes {
                let mut stroke_coords: Vec<Object> = Vec::with_capacity(stroke.len() * 2);
                for (idx, &point) in stroke.iter().enumerate() {
                    let (pdf_x, pdf_y) = page_dim.view_to_pdf(point.0, point.1);
                    stroke_coords.push(pdf_x.into());
                    stroke_coords.push(pdf_y.into());
                    let (rel_x, rel_y) = to_ap(point);
                    let op = if idx == 0 { "m" } else { "l" };
                    ap_ops.push(Operation::new(op, vec![rel_x.into(), rel_y.into()]));
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
            let ap_ops = match shape_kind {
                ShapeKind::Rectangle => {
                    dict.set("Subtype", Object::Name(b"Square".to_vec()));
                    dict.set("BS", dictionary! { "W" => 2.0 });
                    vec![
                        Operation::new("q", vec![]),
                        Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                        Operation::new("w", vec![2.0.into()]),
                        Operation::new(
                            "re",
                            vec![1.0.into(), 1.0.into(), (width - 2.0).into(), (height - 2.0).into()],
                        ),
                        Operation::new("S", vec![]),
                        Operation::new("Q", vec![]),
                    ]
                }
                ShapeKind::Ellipse => {
                    dict.set("Subtype", Object::Name(b"Circle".to_vec()));
                    dict.set("BS", dictionary! { "W" => 2.0 });
                    const KAPPA: f32 = 0.5523;
                    let (cx, cy) = (width * 0.5, height * 0.5);
                    let (rx, ry) = ((width - 2.0) * 0.5, (height - 2.0) * 0.5);
                    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
                    let curve = |x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32| {
                        Operation::new(
                            "c",
                            vec![x1.into(), y1.into(), x2.into(), y2.into(), x3.into(), y3.into()],
                        )
                    };
                    vec![
                        Operation::new("q", vec![]),
                        Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                        Operation::new("w", vec![2.0.into()]),
                        Operation::new("m", vec![(cx + rx).into(), cy.into()]),
                        curve(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry),
                        curve(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy),
                        curve(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry),
                        curve(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy),
                        Operation::new("h", vec![]),
                        Operation::new("S", vec![]),
                        Operation::new("Q", vec![]),
                    ]
                }
                ShapeKind::ArrowLine {
                    start_head,
                    end_head,
                } => {
                    dict.set("Subtype", Object::Name(b"Line".to_vec()));
                    dict.set("BS", dictionary! { "W" => ARROW_LINE_WIDTH });
                    let (ax, ay) = page_dim.view_to_pdf(start_pt.0, start_pt.1);
                    let (bx, by) = page_dim.view_to_pdf(end_pt.0, end_pt.1);
                    dict.set("L", vec![ax.into(), ay.into(), bx.into(), by.into()]);
                    dict.set(
                        "LE",
                        vec![
                            Object::Name(start_head.pdf_name().to_vec()),
                            Object::Name(end_head.pdf_name().to_vec()),
                        ],
                    );
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
                page_dim,
            )?;
        }
        AnnotationKind::StickyNote { comment, .. } => {
            dict.set("Subtype", Object::Name(b"Text".to_vec()));
            dict.set("Name", Object::Name(b"Comment".to_vec()));
            dict.set("Contents", pdf_text_string(comment));
            dict.set("Open", false);
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
            dict.set("BS", dictionary! { "W" => 1.5 });
            let (ax, ay) = page_dim.view_to_pdf(start_pt.0, start_pt.1);
            let (bx, by) = page_dim.view_to_pdf(end_pt.0, end_pt.1);
            dict.set("L", vec![ax.into(), ay.into(), bx.into(), by.into()]);
            dict.set(
                "LE",
                vec![Object::Name(b"Butt".to_vec()), Object::Name(b"Butt".to_vec())],
            );
            dict.set("IT", Object::Name(b"LineDimension".to_vec()));
            dict.set("Contents", Object::string_literal(label.as_str()));
            dict.set("Cap", true);

            let (sx, sy) = to_ap(*start_pt);
            let (ex, ey) = to_ap(*end_pt);
            let dir = unit_direction((sx, sy), (ex, ey));
            let tick = (-dir.1 * 6.0, dir.0 * 6.0);
            let mut ops = vec![
                Operation::new("q", vec![]),
                Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                Operation::new("w", vec![1.5.into()]),
                Operation::new("m", vec![sx.into(), sy.into()]),
                Operation::new("l", vec![ex.into(), ey.into()]),
            ];
            for (x, y) in [(sx, sy), (ex, ey)] {
                ops.push(Operation::new("m", vec![(x - tick.0).into(), (y - tick.1).into()]));
                ops.push(Operation::new("l", vec![(x + tick.0).into(), (y + tick.1).into()]));
            }
            ops.push(Operation::new("S", vec![]));
            ops.push(Operation::new("BT", vec![]));
            ops.push(Operation::new("rg", vec![r.into(), g.into(), b.into()]));
            ops.push(Operation::new("Tf", vec!["Helv".into(), 8.into()]));
            ops.push(Operation::new(
                "Td",
                vec![((sx + ex) * 0.5 - 40.0).max(1.0).into(), ((sy + ey) * 0.5 + 3.0).into()],
            ));
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(encode_win_ansi(&label), lopdf::StringFormat::Literal)],
            ));
            ops.push(Operation::new("ET", vec![]));
            ops.push(Operation::new("Q", vec![]));
            let ap_id = create_appearance_stream(doc, width, height, ops, Some(helv_font_id))?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
    }

    let annot_id = doc.add_object(Object::Dictionary(dict));
    let mut objects = vec![annot_id];
    if let Some(bubble) = note_bubble_rect {
        let [px0, py0, px1, py1] = pdf_rect_of(&bubble);
        let popup_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Popup",
            "Parent" => annot_id,
            "Open" => false,
            "F" => 28,
            "NM" => Object::string_literal(format!("superpdf_popup_{}", annot.id)),
            "Rect" => vec![px0.into(), py0.into(), px1.into(), py1.into()],
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

#[derive(Default)]
struct LoadedThread {
    replies: Vec<CommentReply>,
    states: Vec<(ReviewState, ObjectId)>,
    other_sources: Vec<ObjectId>,
}

fn dict_text(d: &lopdf::Dictionary, key: &[u8]) -> String {
    d.get(key)
        .and_then(|o| o.as_str())
        .map(decode_pdf_text)
        .unwrap_or_default()
}

fn dict_date(d: &lopdf::Dictionary) -> String {
    let modified = dict_text(d, b"M");
    if modified.is_empty() {
        dict_text(d, b"CreationDate")
    } else {
        modified
    }
}

/// Charge les annotations d'un PDF (Acrobat ou Kite PDF), avec leurs fils de discussion.
pub fn load_annotations_from_pdf(
    path: &Path,
    page_dimensions: &[PageDimensions],
) -> Result<Vec<PdfAnnotation>> {
    let doc = Document::load(path)?;
    let mut loaded = Vec::new();
    let mut next_id = 1_u64;

    for (&page_num_1based, &page_obj_id) in &doc.get_pages() {
        let page_idx = (page_num_1based as usize).saturating_sub(1);
        let page_dim = page_dimensions
            .get(page_idx)
            .copied()
            .unwrap_or(PageDimensions::simple(595.0, 842.0));

        let items = page_annotation_items(&doc, page_obj_id);
        let entries: Vec<(Option<ObjectId>, &lopdf::Dictionary)> = items
            .iter()
            .filter_map(|item| match item {
                Object::Reference(id) => doc
                    .get_object(*id)
                    .and_then(|o| o.as_dict())
                    .ok()
                    .map(|d| (Some(*id), d)),
                Object::Dictionary(d) => Some((None, d)),
                _ => None,
            })
            .collect();
        let by_id: HashMap<ObjectId, &lopdf::Dictionary> = entries
            .iter()
            .filter_map(|(id, d)| id.map(|id| (id, *d)))
            .collect();

        // Regroupe réponses et états sous le commentaire racine (une réponse peut viser une réponse).
        let mut threads: HashMap<ObjectId, LoadedThread> = HashMap::new();
        for (id, d) in &entries {
            let Ok(mut root) = d.get(b"IRT").and_then(|o| o.as_reference()) else {
                continue;
            };
            let Some(id) = *id else { continue };
            for _ in 0..32 {
                match by_id.get(&root).and_then(|p| p.get(b"IRT").ok()) {
                    Some(next) => match next.as_reference() {
                        Ok(next) => root = next,
                        Err(_) => break,
                    },
                    None => break,
                }
            }
            let thread = threads.entry(root).or_default();
            if d.has(b"State") {
                if dict_text(d, b"StateModel") == "Review" {
                    thread.states.push((
                        ReviewState {
                            state: dict_text(d, b"State"),
                            author: dict_text(d, b"T"),
                            date: dict_date(d),
                        },
                        id,
                    ));
                } else {
                    thread.other_sources.push(id);
                }
            } else {
                let text = dict_text(d, b"Contents");
                thread.replies.push(CommentReply {
                    author: dict_text(d, b"T"),
                    date: dict_date(d),
                    loaded_text: Some(text.clone()),
                    text,
                    source: Some(id),
                });
            }
        }

        for (source, d) in &entries {
            if d.has(b"IRT") || !is_app_managed(d) {
                continue;
            }
            let subtype = d.get(b"Subtype").and_then(|o| o.as_name_str()).unwrap_or("");
            let mut rect = parse_pdf_rect(d, page_dim).unwrap_or(PdfTopDownRect {
                x_min: 50.0,
                y_min: 50.0,
                x_max: 200.0,
                y_max: 90.0,
                angle_deg: 0.0,
            });
            let color_rgb = parse_pdf_color(d).unwrap_or((255, 210, 30));
            let contents = dict_text(d, b"Contents");
            let mut thread = source
                .and_then(|id| threads.remove(&id))
                .unwrap_or_default();
            thread.replies.sort_by(|a, b| a.date.cmp(&b.date));
            thread.states.sort_by(|a, b| a.0.date.cmp(&b.0.date));
            let popup_source = d.get(b"Popup").and_then(|o| o.as_reference()).ok();

            let kind = match subtype {
                "Highlight" => {
                    let quads = parse_quad_points(d, page_dim).unwrap_or_else(|| vec![rect]);
                    if contents.trim().is_empty() && thread.replies.is_empty() {
                        AnnotationKind::Highlight { quads }
                    } else {
                        // Commentaire attaché au texte : la bulle vient de `/Popup` si elle existe.
                        rect = popup_source
                            .and_then(|id| doc.get_object(id).ok())
                            .and_then(|o| o.as_dict().ok())
                            .and_then(|popup| parse_pdf_rect(popup, page_dim))
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
                        font_size: parse_freetext_font_size(d),
                        text_rgb: parse_freetext_text_color(d),
                        fill_rgb,
                        border_rgb,
                    }
                }
                "Ink" => {
                    let mut strokes = Vec::new();
                    if let Ok(ink_list) = d.get(b"InkList").and_then(|o| o.as_array()) {
                        for s_obj in ink_list {
                            let coords = match s_obj {
                                Object::Reference(id) => {
                                    doc.get_object(*id).and_then(|o| o.as_array()).ok()
                                }
                                other => other.as_array().ok(),
                            };
                            if let Some(coords) = coords {
                                let pts: Vec<(f32, f32)> = coords
                                    .chunks_exact(2)
                                    .filter_map(|pair| {
                                        Some(page_dim.pdf_to_view(
                                            obj_to_f32(&pair[0])?,
                                            obj_to_f32(&pair[1])?,
                                        ))
                                    })
                                    .collect();
                                if !pts.is_empty() {
                                    strokes.push(pts);
                                }
                            }
                        }
                    }
                    AnnotationKind::Ink {
                        strokes,
                        thickness: parse_border_width(d).unwrap_or(1.0),
                    }
                }
                "Square" | "Circle" => AnnotationKind::Shape {
                    shape_kind: if subtype == "Circle" {
                        ShapeKind::Ellipse
                    } else {
                        ShapeKind::Rectangle
                    },
                    start_pt: (rect.x_min, rect.y_min),
                    end_pt: (rect.x_max, rect.y_max),
                },
                "Line" => {
                    let endpoints: Option<Vec<f32>> = d
                        .get(b"L")
                        .and_then(|o| o.as_array())
                        .ok()
                        .map(|l| l.iter().filter_map(obj_to_f32).collect());
                    let (start_pt, end_pt) = match endpoints.as_deref() {
                        Some(&[x0, y0, x1, y1]) => {
                            (page_dim.pdf_to_view(x0, y0), page_dim.pdf_to_view(x1, y1))
                        }
                        _ => ((rect.x_min, rect.y_min), (rect.x_max, rect.y_max)),
                    };
                    if d.has(b"Measure") || contents.contains("mm (") {
                        AnnotationKind::Measure {
                            start_pt,
                            end_pt,
                            stamp_tag: None,
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
                "Text" => AnnotationKind::StickyNote {
                    comment: contents,
                    is_popup_open: false,
                    anchor_quads: Vec::new(),
                },
                _ => continue,
            };

            let mut state_sources = thread.other_sources;
            state_sources.extend(thread.states.iter().map(|(_, id)| *id));
            let loaded_state = thread.states.last().map(|(state, _)| state.clone());
            let mut annotation = PdfAnnotation {
                id: next_id,
                page_index: page_idx,
                rect,
                color_rgb,
                kind,
                meta: AnnotationMeta {
                    author: dict_text(d, b"T"),
                    created: dict_text(d, b"CreationDate"),
                    modified: dict_text(d, b"M"),
                    replies: thread.replies,
                    review_state: loaded_state.clone(),
                    was_highlight: false,
                    source: *source,
                    popup_source,
                    state_sources,
                    loaded_state,
                    loaded_snapshot: None,
                },
            };
            annotation.meta.loaded_snapshot = Some(annotation.content_fingerprint());
            loaded.push(annotation);
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

fn parse_pdf_rect(d: &lopdf::Dictionary, page_dim: PageDimensions) -> Option<PdfTopDownRect> {
    let arr = d.get(b"Rect").ok()?.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let (ax, ay) = page_dim.pdf_to_view(obj_to_f32(&arr[0])?, obj_to_f32(&arr[1])?);
    let (bx, by) = page_dim.pdf_to_view(obj_to_f32(&arr[2])?, obj_to_f32(&arr[3])?);
    Some(PdfTopDownRect {
        x_min: ax.min(bx),
        y_min: ay.min(by),
        x_max: ax.max(bx),
        y_max: ay.max(by),
        angle_deg: 0.0,
    })
}

fn parse_border_width(d: &lopdf::Dictionary) -> Option<f32> {
    if let Ok(style) = d.get(b"BS").and_then(|o| o.as_dict()) {
        return style.get(b"W").ok().and_then(obj_to_f32).or(Some(1.0));
    }
    d.get(b"Border")
        .and_then(|o| o.as_array())
        .ok()
        .and_then(|border| border.get(2))
        .and_then(obj_to_f32)
}

/// Corps de police : `/DS` (Acrobat) en priorité, puis l'opérateur `Tf` de `/DA`.
fn parse_freetext_font_size(d: &lopdf::Dictionary) -> f32 {
    let style = dict_text(d, b"DS");
    let from_style = style.find("pt").and_then(|end| {
        let start = style[..end]
            .rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map_or(0, |i| i + 1);
        style[start..end].parse::<f32>().ok()
    });
    let appearance = dict_text(d, b"DA");
    let tokens: Vec<&str> = appearance.split_whitespace().collect();
    let from_appearance = tokens
        .iter()
        .position(|token| *token == "Tf")
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| tokens[i].parse::<f32>().ok());
    from_style
        .or(from_appearance)
        .filter(|size| *size > 0.0)
        .unwrap_or(12.0)
}

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

/// Couleur du texte : `/DS` puis `/RC`, sinon la couleur de `/DA`.
fn parse_freetext_text_color(d: &lopdf::Dictionary) -> (u8, u8, u8) {
    for key in [b"DS".as_slice(), b"RC".as_slice()] {
        let value = dict_text(d, key);
        if let Some(start) = value.find("color:") {
            let after = value[start + 6..].trim_start();
            if let Some(color) = after.strip_prefix('#').and_then(parse_hex_color) {
                return color;
            }
        }
    }
    last_color_operator(&dict_text(d, b"DA"), "rg").unwrap_or(FREETEXT_TEXT_RGB)
}

fn last_color_operator(appearance: &str, operator: &str) -> Option<(u8, u8, u8)> {
    let tokens: Vec<&str> = appearance.split_whitespace().collect();
    let index = tokens.iter().rposition(|token| *token == operator)?;
    if index < 3 {
        return None;
    }
    let channel = |t: &str| (t.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    Some((
        channel(tokens[index - 3]),
        channel(tokens[index - 2]),
        channel(tokens[index - 1]),
    ))
}

/// Relit les `/QuadPoints` (un rectangle par groupe de 8 nombres).
fn parse_quad_points(d: &lopdf::Dictionary, page_dim: PageDimensions) -> Option<Vec<PdfTopDownRect>> {
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
            let corners = [
                page_dim.pdf_to_view(q[0], q[1]),
                page_dim.pdf_to_view(q[2], q[3]),
                page_dim.pdf_to_view(q[4], q[5]),
                page_dim.pdf_to_view(q[6], q[7]),
            ];
            let xs = corners.map(|c| c.0);
            let ys = corners.map(|c| c.1);
            let min_of = |v: [f32; 4]| v.into_iter().fold(f32::INFINITY, f32::min);
            let max_of = |v: [f32; 4]| v.into_iter().fold(f32::NEG_INFINITY, f32::max);
            PdfTopDownRect {
                x_min: min_of(xs),
                y_min: min_of(ys),
                x_max: max_of(xs),
                y_max: max_of(ys),
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
        .or_else(|| parse_border_width(d))
        .unwrap_or(0.0);
    let border_rgb = (border_width > 0.0).then(|| {
        let appearance = dict_text(d, b"DA");
        // `RG` : fichiers de nos versions précédentes ; `rg` : convention d'Acrobat.
        last_color_operator(&appearance, "RG")
            .or_else(|| last_color_operator(&appearance, "rg"))
            .unwrap_or((0, 0, 0))
    });
    (parse_pdf_color(d), border_rgb)
}

/// Couleur `/C` : gris (1 valeur), RVB (3) ou CMJN (4). Un tableau vide signifie transparent.
fn parse_pdf_color(d: &lopdf::Dictionary) -> Option<(u8, u8, u8)> {
    let values: Vec<f32> = d
        .get(b"C")
        .ok()?
        .as_array()
        .ok()?
        .iter()
        .filter_map(obj_to_f32)
        .map(|v| v.clamp(0.0, 1.0))
        .collect();
    let to_byte = |v: f32| (v * 255.0).round() as u8;
    match values.as_slice() {
        [gray] => Some((to_byte(*gray), to_byte(*gray), to_byte(*gray))),
        [r, g, b] => Some((to_byte(*r), to_byte(*g), to_byte(*b))),
        [c, m, y, k] => Some((
            to_byte((1.0 - c) * (1.0 - k)),
            to_byte((1.0 - m) * (1.0 - k)),
            to_byte((1.0 - y) * (1.0 - k)),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf_engine::{ensure_sample_pdf, PdfEngine};
    use std::path::PathBuf;

    #[test]
    fn test_full_rewrite_drops_links_to_previous_revisions() {
        let mut doc = Document::with_version("1.7");
        doc.trailer.set("Prev", 12586);
        doc.trailer.set("XRefStm", 900);
        doc.trailer.set("Size", 44);
        forget_previous_revisions(&mut doc);
        assert!(!doc.trailer.has(b"Prev") && !doc.trailer.has(b"XRefStm"));
        assert!(doc.trailer.has(b"Size"));
    }

    #[test]
    fn test_text_strings_use_acrobat_line_endings() {
        for (input, expected) in [("a\nb", "a\rb"), ("a\r\nb", "a\rb"), ("é\nb", "é\rb")] {
            let Object::String(bytes, _) = pdf_text_string(input) else {
                panic!("Doit produire une chaîne PDF");
            };
            let raw = if input.is_ascii() {
                String::from_utf8(bytes.clone()).unwrap()
            } else {
                let units: Vec<u16> = bytes[2..]
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16(&units).unwrap()
            };
            assert_eq!(raw, expected);
            assert_eq!(decode_pdf_text(&bytes), input.replace("\r\n", "\n"));
        }
        assert_eq!(decode_pdf_text(b"a\rb\r\nc"), "a\nb\nc");
    }

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
            meta: AnnotationMeta::default(),
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
            meta: AnnotationMeta::default(),
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
    fn test_form_fields_are_filled_saved_and_read_back() {
        use crate::pdf_engine::FormWidgetKind;
        // Formulaire minimal : un champ texte, une case à cocher, un groupe de deux boutons radio.
        let src = PathBuf::from("test_pdfs/unit_test_form_src.pdf");
        let blank = PathBuf::from("test_pdfs/unit_test_form_blank.pdf");
        let filled = PathBuf::from("test_pdfs/unit_test_form_filled.pdf");
        ensure_sample_pdf(&src).expect("Doit créer le PDF");
        let mut doc = Document::load(&src).expect("Doit charger");
        let page_id = doc.get_pages()[&1];
        let font_id = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let mut appearance = || {
            doc.add_object(Stream::new(
                dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 12.into(), 12.into()] },
                Vec::new(),
            ))
        };
        let (yes, off, a, a_off, b, b_off) = (appearance(), appearance(), appearance(), appearance(), appearance(), appearance());
        let text_id = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("nom"),
            "Rect" => vec![100.into(), 700.into(), 300.into(), 720.into()], "P" => page_id,
            "DA" => Object::string_literal("/Helv 10 Tf 0 g"), "V" => Object::string_literal(""),
        });
        let checkbox_id = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn", "T" => Object::string_literal("accord"),
            "Rect" => vec![100.into(), 660.into(), 112.into(), 672.into()], "P" => page_id,
            "AP" => dictionary! { "N" => dictionary! { "Yes" => yes, "Off" => off } }, "AS" => "Off", "V" => "Off",
        });
        let radio_id = doc.new_object_id();
        let kid = |doc: &mut Document, x: i64, on: &str, on_ap: ObjectId, off_ap: ObjectId| {
            let mut normal = lopdf::Dictionary::new();
            normal.set(on, on_ap);
            normal.set("Off", off_ap);
            doc.add_object(dictionary! {
                "Type" => "Annot", "Subtype" => "Widget", "Parent" => radio_id, "P" => page_id,
                "Rect" => vec![x.into(), 620.into(), (x + 12).into(), 632.into()],
                "AP" => dictionary! { "N" => normal }, "AS" => "Off",
            })
        };
        let kid_a = kid(&mut doc, 100, "A", a, a_off);
        let kid_b = kid(&mut doc, 140, "B", b, b_off);
        doc.objects.insert(radio_id, Object::Dictionary(dictionary! {
            "FT" => "Btn", "Ff" => 49152, "T" => Object::string_literal("choix"),
            "Kids" => vec![kid_a.into(), kid_b.into()], "V" => "Off",
        }));
        let annots = vec![text_id.into(), checkbox_id.into(), kid_a.into(), kid_b.into()];
        doc.get_object_mut(page_id).and_then(Object::as_dict_mut).unwrap().set("Annots", Object::Array(annots));
        let acroform = doc.add_object(dictionary! {
            "Fields" => vec![text_id.into(), checkbox_id.into(), radio_id.into()],
            "DR" => dictionary! { "Font" => dictionary! { "Helv" => font_id } },
            "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
        });
        let catalog = doc.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        doc.get_object_mut(catalog).and_then(Object::as_dict_mut).unwrap().set("AcroForm", acroform);
        // Droits d'utilisation Adobe, comme sur les formulaires officiels (W-9...).
        let usage_rights = doc.add_object(dictionary! { "Type" => "Sig", "Filter" => "Adobe.PPKLite" });
        let perms = doc.add_object(dictionary! { "UR3" => usage_rights });
        doc.get_object_mut(catalog).and_then(Object::as_dict_mut).unwrap().set("Perms", perms);
        doc.save(&blank).expect("Doit enregistrer le formulaire vide");

        let engine = PdfEngine::instance();
        let loaded = engine.load_document(&blank).expect("Doit charger");
        let widgets = loaded.page_form_widgets(0);
        assert_eq!(widgets.len(), 4, "{widgets:?}");
        assert!(widgets.iter().any(|w| w.name == "nom" && matches!(w.kind, FormWidgetKind::Text { .. })));
        let dims = loaded.page_dimensions.clone();
        drop(loaded);

        let edits = vec![
            ("nom".to_string(), FormEdit::Text("Jeanne Dupont".to_string())),
            ("accord".to_string(), FormEdit::Checked(true)),
            ("choix".to_string(), FormEdit::Radio(1)),
        ];
        save_pdf_with_ocr_state(&blank, &filled, &[], &[], &dims, &[], &[], &edits).expect("Doit enregistrer");

        let reloaded = engine.load_document(&filled).expect("Doit recharger");
        let widgets = reloaded.page_form_widgets(0);
        let find = |name: &str, index: Option<u32>| {
            widgets
                .iter()
                .find(|w| w.name == name && index.is_none_or(|i| w.kind == FormWidgetKind::Radio { index: i }))
                .unwrap_or_else(|| panic!("champ {name} absent : {widgets:?}"))
                .clone()
        };
        assert_eq!(find("nom", None).value, "Jeanne Dupont");
        // Les droits d'utilisation, invalides après modification, sont retirés (sinon Acrobat
        // verrouille le formulaire).
        let saved = Document::load(&filled).expect("Doit relire");
        let catalog = saved.trailer.get(b"Root").and_then(Object::as_reference).unwrap();
        assert!(!saved.get_object(catalog).and_then(Object::as_dict).unwrap().has(b"Perms"));
        assert!(find("accord", None).checked);
        assert!(!find("choix", Some(0)).checked);
        assert!(find("choix", Some(1)).checked);
    }

    #[test]
    fn test_ocr_scanned_pages_survive_save_and_reopen() {
        use crate::ocr_engine::{format_ocr_scanned_pages, parse_ocr_scanned_pages};
        assert_eq!(format_ocr_scanned_pages(&[true, true, false, true, true, true]), "6:1-2,4-6");
        assert_eq!(parse_ocr_scanned_pages("6:1-2,4-6", 6), vec![true, true, false, true, true, true]);
        // Nombre de pages différent : le marqueur est ignoré.
        assert_eq!(parse_ocr_scanned_pages("6:1-6", 5), vec![false; 5]);

        let src = PathBuf::from("test_pdfs/unit_test_ocr_marker_src.pdf");
        let dst = PathBuf::from("test_pdfs/unit_test_ocr_marker_saved.pdf");
        ensure_sample_pdf(&src).expect("Doit créer le PDF");
        let engine = PdfEngine::instance();
        let dims = engine.load_document(&src).expect("Doit charger").page_dimensions.clone();
        assert!(engine.load_document(&src).unwrap().ocr_scanned_pages.iter().all(|s| !s));

        let scanned = vec![true, false, true, true, true];
        let deep = vec![false, false, true, false, false];
        save_pdf_with_ocr_state(&src, &dst, &[], &[], &dims, &scanned, &deep, &[]).expect("Doit enregistrer");
        assert_eq!(engine.load_document(&dst).unwrap().ocr_scanned_pages, scanned);
        assert_eq!(engine.load_document(&dst).unwrap().deep_ocr_pages, deep);

        // Un enregistrement sans état OCR garde le marqueur du fichier.
        save_annotations_and_ocr_to_pdf(&dst, &dst, &[], &[], &dims).expect("Doit enregistrer");
        assert_eq!(engine.load_document(&dst).unwrap().ocr_scanned_pages, scanned);
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
                meta: AnnotationMeta::default(),
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
                meta: AnnotationMeta::default(),
                id: 2,
                page_index: 0,
                rect: rect(100.0, 300.0, 300.0, 360.0),
                color_rgb: (45, 135, 245),
                kind: AnnotationKind::FreeText {
                    text: "Déjà vérifié".to_string(),
                    font_size: 11.5,
                    text_rgb: FREETEXT_TEXT_RGB,
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

    /// Reproduit la structure écrite par Acrobat : surlignage commenté, réponse, état, tampon.
    fn write_acrobat_style_pdf(path: &Path) -> (ObjectId, ObjectId) {
        let source = PathBuf::from("test_pdfs/unit_test_sample.pdf");
        ensure_sample_pdf(&source).expect("Doit créer le PDF");
        let mut doc = Document::load(&source).expect("Doit charger le PDF");
        let page_id = *doc.get_pages().get(&1).expect("Page 1");
        let quads: Vec<Object> = [100.0_f32, 700.0, 200.0, 700.0, 100.0, 688.0, 200.0, 688.0]
            .iter()
            .map(|v| Object::Real(*v))
            .collect();
        let rect = || -> Vec<Object> {
            [100.0_f32, 688.0, 200.0, 700.0].iter().map(|v| Object::Real(*v)).collect()
        };
        let highlight = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Highlight", "Rect" => rect(), "QuadPoints" => quads,
            "C" => vec![1.0.into(), 0.8.into(), 0.0.into()], "F" => 4,
            "T" => Object::string_literal("Relecteur"), "Contents" => Object::string_literal("Premier avis"),
            "M" => Object::string_literal("D:20250101100000+01'00'"),
            "Subj" => Object::string_literal("Surlignage"), "NM" => Object::string_literal("acrobat-uuid-1"),
        });
        let popup = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Popup", "Rect" => rect(), "Parent" => highlight,
            "Open" => false, "F" => 28,
        });
        if let Ok(d) = doc.get_object_mut(highlight).and_then(|o| o.as_dict_mut()) {
            d.set("Popup", popup);
        }
        let reply = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Text", "Rect" => rect(), "F" => 28, "IRT" => highlight,
            "RT" => "R", "T" => Object::string_literal("Autre"), "Contents" => Object::string_literal("Une reponse"),
            "M" => Object::string_literal("D:20250101110000+01'00'"),
        });
        let state = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Text", "Rect" => rect(), "F" => 30, "IRT" => highlight,
            "State" => Object::string_literal("Accepted"), "StateModel" => Object::string_literal("Review"),
            "T" => Object::string_literal("Autre"), "M" => Object::string_literal("D:20250101120000+01'00'"),
        });
        let stamp = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Stamp", "Rect" => rect(), "F" => 4,
            "Name" => "Approved", "T" => Object::string_literal("Autre"),
        });
        if let Ok(d) = doc.get_object_mut(page_id).and_then(|o| o.as_dict_mut()) {
            d.set(
                "Annots",
                vec![
                    Object::Reference(highlight),
                    Object::Reference(popup),
                    Object::Reference(reply),
                    Object::Reference(state),
                    Object::Reference(stamp),
                ],
            );
        }
        doc.save(path).expect("Doit écrire le PDF");
        (highlight, stamp)
    }

    fn annotation_count(path: &Path) -> usize {
        let doc = Document::load(path).expect("Doit relire");
        let page_id = *doc.get_pages().get(&1).expect("Page 1");
        page_annotation_items(&doc, page_id).len()
    }

    #[test]
    fn test_acrobat_thread_survives_repeated_saves_without_duplication() {
        let path = PathBuf::from("test_pdfs/unit_test_acrobat_thread.pdf");
        let _ = std::fs::remove_file(&path);
        let (highlight_id, stamp_id) = write_acrobat_style_pdf(&path);
        let dims = [PageDimensions::simple(595.0, 842.0)];

        for _ in 0..3 {
            let loaded = load_annotations_from_pdf(&path, &dims).expect("Doit relire");
            assert_eq!(loaded.len(), 1, "réponse, état et popup ne sont pas des annotations à part");
            save_annotations_and_ocr_to_pdf(&path, &path, &loaded, &[], &dims).expect("Doit enregistrer");
            assert_eq!(annotation_count(&path), 5, "aucune duplication à l'enregistrement");
        }

        let loaded = load_annotations_from_pdf(&path, &dims).expect("Doit relire");
        let thread = &loaded[0];
        assert_eq!(thread.meta.author, "Relecteur");
        assert_eq!(thread.meta.replies.len(), 1);
        assert_eq!(thread.meta.replies[0].author, "Autre");
        assert_eq!(thread.meta.review_state.as_ref().unwrap().state, "Accepted");

        let doc = Document::load(&path).expect("Doit relire");
        let original = doc.get_object(highlight_id).and_then(|o| o.as_dict()).expect("Objet conservé");
        assert_eq!(original.get(b"NM").unwrap().as_str().unwrap(), b"acrobat-uuid-1");
        assert!(original.has(b"Subj"), "les clés inconnues sont préservées");
        assert!(doc.get_object(stamp_id).is_ok(), "le tampon traverse la sauvegarde");
    }

    #[test]
    fn test_reply_state_and_edit_are_written_like_acrobat() {
        let path = PathBuf::from("test_pdfs/unit_test_acrobat_reply.pdf");
        let _ = std::fs::remove_file(&path);
        let (highlight_id, _) = write_acrobat_style_pdf(&path);
        let dims = [PageDimensions::simple(595.0, 842.0)];

        let mut loaded = load_annotations_from_pdf(&path, &dims).expect("Doit relire");
        loaded[0].meta.replies.push(CommentReply {
            author: "Moi".to_string(),
            date: pdf_date_now(),
            text: "Réponse accentuée".to_string(),
            ..Default::default()
        });
        loaded[0].meta.review_state = Some(ReviewState {
            state: "Completed".to_string(),
            author: "Moi".to_string(),
            date: "D:20300101120000+01'00'".to_string(),
        });
        if let AnnotationKind::StickyNote { comment, .. } = &mut loaded[0].kind {
            *comment = "Avis modifié".to_string();
        }
        save_annotations_and_ocr_to_pdf(&path, &path, &loaded, &[], &dims).expect("Doit enregistrer");

        let doc = Document::load(&path).expect("Doit relire");
        let original = doc.get_object(highlight_id).and_then(|o| o.as_dict()).expect("Objet conservé");
        assert_eq!(decode_pdf_text(original.get(b"Contents").unwrap().as_str().unwrap()), "Avis modifié");
        assert_eq!(original.get(b"NM").unwrap().as_str().unwrap(), b"acrobat-uuid-1");
        assert!(
            !doc.objects.values().filter_map(|o| o.as_dict().ok()).any(|d| d.has(b"State") && d.has(b"Open")),
            "Acrobat ignore un état de révision qui porte /Open"
        );

        let reloaded = load_annotations_from_pdf(&path, &dims).expect("Doit relire");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].meta.replies.len(), 2);
        assert_eq!(reloaded[0].meta.replies[1].text, "Réponse accentuée");
        assert_eq!(reloaded[0].meta.review_state.as_ref().unwrap().state, "Completed");
    }

    #[test]
    fn test_deleting_a_comment_removes_its_whole_thread_only() {
        let path = PathBuf::from("test_pdfs/unit_test_acrobat_delete.pdf");
        let _ = std::fs::remove_file(&path);
        write_acrobat_style_pdf(&path);
        let dims = [PageDimensions::simple(595.0, 842.0)];
        save_annotations_and_ocr_to_pdf(&path, &path, &[], &[], &dims).expect("Doit enregistrer");
        assert_eq!(annotation_count(&path), 1, "seul le tampon, non géré, subsiste");
    }

    #[test]
    fn test_freetext_font_size_and_colors_follow_acrobat_keys() {
        let mut d = lopdf::Dictionary::new();
        d.set("DA", Object::string_literal("1 0 0 rg /Helv 18 Tf"));
        d.set("BS", dictionary! { "W" => 1 });
        d.set("C", vec![0.into(), 1.into(), 0.into()]);
        assert_eq!(parse_freetext_font_size(&d), 18.0);
        assert_eq!(parse_freetext_style(&d, (0, 0, 0)), (Some((0, 255, 0)), Some((255, 0, 0))));

        d.set("DS", Object::string_literal("font: Helvetica 9.5pt; color:#0000FF"));
        assert_eq!(parse_freetext_font_size(&d), 9.5);
        assert_eq!(parse_freetext_text_color(&d), (0, 0, 255));
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
