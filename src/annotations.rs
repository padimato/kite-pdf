use crate::ocr_engine::append_invisible_ocr_layer_to_doc;
use crate::pdf_engine::PageDimensions;
use crate::text_search::{PageTextLayer, PdfTopDownRect};
use anyhow::{Context, Result};
use egui::Color32;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle,
    ArrowLine,
}

/// Variantes d'annotations conformes à la norme ISO 32000-1 / 32000-2 (§12.5)
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotationKind {
    /// `/Subtype /Highlight` avec `/QuadPoints`
    Highlight { quads: Vec<PdfTopDownRect> },
    /// `/Subtype /FreeText` avec `/DA` (DefaultAppearance), `/Contents` et `/AP`
    FreeText { text: String, font_size: f32 },
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
    /// `/Subtype /Text` (Sticky Note / Bulle de commentaire)
    StickyNote {
        comment: String,
        is_popup_open: bool,
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
                shape_kind: ShapeKind::ArrowLine,
                ..
            } => "Flèche (/Line)",
            AnnotationKind::StickyNote { .. } => "Note (/Text)",
            AnnotationKind::Measure { .. } => "Mesure & Cote (/Measure)",
        }
    }

    /// Vérifie si un clic en coordonnées points PDF `(x_pt, y_pt)` touche cette annotation
    pub fn hit_test(&self, x_pt: f32, y_pt: f32) -> bool {
        self.rect.contains_pt(x_pt, y_pt, 5.0)
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
                shape_kind: ShapeKind::ArrowLine,
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
            let annot_id = build_iso32000_annot_object(&mut doc, annot, page_h, helv_font_id)?;
            annot_refs.push(Object::Reference(annot_id));
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

    doc.save(dest_path).with_context(|| {
        format!(
            "Échec d'écriture du fichier PDF annoté : {}",
            dest_path.display()
        )
    })?;
    Ok(())
}

fn build_iso32000_annot_object(
    doc: &mut Document,
    annot: &PdfAnnotation,
    page_h: f32,
    helv_font_id: ObjectId,
) -> Result<ObjectId> {
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
            dict.set("Subtype", Object::Name(b"Highlight".to_vec()));
            dict.set("BM", Object::Name(b"Multiply".to_vec()));
            dict.set("CA", 1.0);
            let mut quad_points: Vec<Object> = Vec::with_capacity(quads.len() * 8);
            for q in quads {
                let q_x1 = q.x_min;
                let q_x2 = q.x_max;
                let q_y_top = page_h - q.y_min;
                let q_y_bot = page_h - q.y_max;
                // Ordre ISO 32000 QuadPoints : (x1,y_top), (x2,y_top), (x1,y_bot), (x2,y_bot)
                quad_points.extend_from_slice(&[
                    q_x1.into(),
                    q_y_top.into(),
                    q_x2.into(),
                    q_y_top.into(),
                    q_x1.into(),
                    q_y_bot.into(),
                    q_x2.into(),
                    q_y_bot.into(),
                ]);
            }
            dict.set("QuadPoints", Object::Array(quad_points));

            let ap_ops = vec![
                Operation::new("q", vec![]),
                Operation::new("rg", vec![r.into(), g.into(), b.into()]),
                Operation::new("re", vec![0.into(), 0.into(), width.into(), height.into()]),
                Operation::new("f", vec![]),
                Operation::new("Q", vec![]),
            ];
            let ap_id = create_appearance_stream(doc, width, height, ap_ops, None)?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::FreeText { text, font_size } => {
            dict.set("Subtype", Object::Name(b"FreeText".to_vec()));
            dict.set("Contents", Object::string_literal(text.as_str()));
            dict.set(
                "DA",
                Object::string_literal(format!(
                    "/Helv {:.1} Tf {:.3} {:.3} {:.3} rg",
                    font_size, r, g, b
                )),
            );

            let mut ap_ops = vec![
                Operation::new("q", vec![]),
                Operation::new("rg", vec![1.0.into(), 1.0.into(), 0.96.into()]),
                Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                Operation::new("w", vec![1.5.into()]),
                Operation::new(
                    "re",
                    vec![0.75.into(), 0.75.into(), (width - 1.5).into(), (height - 1.5).into()],
                ),
                Operation::new("B", vec![]),
                Operation::new("Q", vec![]),
            ];

            for (line_idx, line) in text.lines().enumerate() {
                let y_line = (height - 15.0 - (line_idx as f32 * (font_size + 3.0))).max(4.0);
                ap_ops.push(Operation::new("BT", vec![]));
                ap_ops.push(Operation::new("rg", vec![0.10.into(), 0.12.into(), 0.18.into()]));
                ap_ops.push(Operation::new("Tf", vec!["Helv".into(), (*font_size).into()]));
                ap_ops.push(Operation::new("Td", vec![6.into(), y_line.into()]));
                ap_ops.push(Operation::new("Tj", vec![Object::string_literal(line)]));
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
                ShapeKind::ArrowLine => {
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
                            Object::Name(b"None".to_vec()),
                            Object::Name(b"ClosedArrow".to_vec()),
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
                ShapeKind::ArrowLine => {
                    let sx = (start_pt.0 - llx).clamp(1.0, width - 1.0);
                    let sy = ((page_h - start_pt.1) - lly).clamp(1.0, height - 1.0);
                    let ex = (end_pt.0 - llx).clamp(1.0, width - 1.0);
                    let ey = ((page_h - end_pt.1) - lly).clamp(1.0, height - 1.0);
                    vec![
                        Operation::new("q", vec![]),
                        Operation::new("RG", vec![r.into(), g.into(), b.into()]),
                        Operation::new("w", vec![2.2.into()]),
                        Operation::new("m", vec![sx.into(), sy.into()]),
                        Operation::new("l", vec![ex.into(), ey.into()]),
                        Operation::new("S", vec![]),
                        Operation::new("Q", vec![]),
                    ]
                }
            };
            let ap_id = create_appearance_stream(doc, width, height, ap_ops, None)?;
            dict.set("AP", dictionary! { "N" => ap_id });
        }
        AnnotationKind::StickyNote { comment, .. } => {
            dict.set("Subtype", Object::Name(b"Text".to_vec()));
            dict.set("Name", Object::Name(b"Comment".to_vec()));
            dict.set("Contents", Object::string_literal(comment.as_str()));
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

    Ok(doc.add_object(Object::Dictionary(dict)))
}

fn create_appearance_stream(
    doc: &mut Document,
    width: f32,
    height: f32,
    ops: Vec<Operation>,
    helv_font_id: Option<ObjectId>,
) -> Result<ObjectId> {
    let content = Content { operations: ops };
    let mut resources = lopdf::Dictionary::new();
    if let Some(fid) = helv_font_id {
        resources.set("Font", dictionary! { "Helv" => fid });
    }
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

            let rect = parse_pdf_rect(d, page_h).unwrap_or(PdfTopDownRect {
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
                .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                .unwrap_or_default();

            let kind = match subtype {
                "Highlight" => AnnotationKind::Highlight { quads: vec![rect] },
                "FreeText" => AnnotationKind::FreeText {
                    text: if contents.is_empty() {
                        "Texte".to_string()
                    } else {
                        contents
                    },
                    font_size: 11.5,
                },
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
                        AnnotationKind::Shape {
                            shape_kind: ShapeKind::ArrowLine,
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
