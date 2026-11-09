use crate::pdf_engine::{LoadedPdfDocument, PageDimensions};
use crate::text_search::{normalize_char, PageTextLayer, PdfTopDownRect, PositionedChar};
use anyhow::Result;
use image::RgbImage;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use oar_ocr::core::config::OrtSessionConfig;
use oar_ocr::prelude::{OAROCRBuilder, TextRegion, OAROCR};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Mode de fonctionnement de l'ordonnanceur OCR Hybride à 3 vitesses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrSpeedMode {
    /// Phase 1 : Ouverture instantanée (0 ms de blocage, lecture de la couche existante `Tr 3`)
    #[default]
    InstantOpen,
    /// Phase 2 : Travailleur de fond basse priorité (scanne les images de la page visible au repos)
    EcoBackground,
    /// Phase 3 : Mode Turbo déclenché par `Ctrl+F` (scan haute vitesse de toutes les images embarquées)
    TurboSearch,
}

impl OcrSpeedMode {
    pub fn badge_label(&self) -> &'static str {
        match self {
            OcrSpeedMode::InstantOpen => "Vitesse 1 : Instantané (0 ms)",
            OcrSpeedMode::EcoBackground => "Vitesse 2 : Éco Tâche de fond",
            OcrSpeedMode::TurboSearch => "Vitesse 3 : TURBO Ctrl+F",
        }
    }
}

/// Métadonnées d'une image embarquée (`XObject /Subtype /Image`) détectée dans une page PDF
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EmbeddedImageRegion {
    pub page_index: usize,
    pub rect_pt: PdfTopDownRect,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub already_has_text_layer: bool,
}

/// État de l'ordonnanceur OCR pour un onglet PDF
#[derive(Debug, Clone)]
pub struct TabOcrState {
    pub mode: OcrSpeedMode,
    /// Indique pour chaque page si ses images embarquées ont déjà été inspectées/OCRisées
    pub page_scanned: Vec<bool>,
    /// Nombre total d'images embarquées (`XObject Image`) détectées dans le document
    pub detected_images_count: usize,
    /// Nombre total de caractères OCR reconnus dans les images (toutes orientations)
    pub recognized_ocr_chars_count: usize,
    /// Nombre de segments orientés (0°, ±30°, ±90°) extraits par l'OCR
    pub recognized_segments_count: usize,
    /// Indique si de nouveaux caractères OCR ont été reconnus mais pas encore sauvegardés (`Ctrl+S`)
    pub has_unsaved_ocr: bool,
    /// Message d'état court pour l'interface utilisateur
    pub status_summary: String,
    /// Page en cours de traitement par le thread OCR
    pub in_flight_page: Option<usize>,
    /// Raison de l'indisponibilité de l'OCR (modèles absents...) : plus aucune page n'est soumise
    pub unavailable_reason: Option<String>,
}

impl Default for TabOcrState {
    fn default() -> Self {
        Self {
            mode: OcrSpeedMode::InstantOpen,
            page_scanned: Vec::new(),
            detected_images_count: 0,
            recognized_ocr_chars_count: 0,
            recognized_segments_count: 0,
            has_unsaved_ocr: false,
            status_summary: "OCR Prêt (0 ms)".to_string(),
            in_flight_page: None,
            unavailable_reason: None,
        }
    }
}

impl TabOcrState {
    pub fn ensure_initialized(&mut self, page_count: usize) {
        if self.page_scanned.len() < page_count {
            self.page_scanned.resize(page_count, false);
        }
    }

    pub fn scanned_pages_count(&self) -> usize {
        self.page_scanned.iter().filter(|&&s| s).count()
    }

    #[allow(dead_code)]
    pub fn is_fully_scanned(&self, page_count: usize) -> bool {
        page_count > 0
            && self.page_scanned.len() >= page_count
            && self.page_scanned.iter().take(page_count).all(|&s| s)
    }
}

/// Police matricielle 5x7 utilisée pour
/// générer les graphiques matriciels RGB (`XObject /Subtype /Image`) du PDF de test, avec textes à 0°, +30°, -30°, +90°.
pub fn glyph_bitmap_5x7(ch: char) -> Option<[u8; 7]> {
    let c = ch.to_ascii_uppercase();
    let rows: [u8; 7] = match c {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10011, 0b10101, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b10001, 0b01110],
        '6' => [0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
        '_' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b00100],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        ' ' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
        _ => return None,
    };
    Some(rows)
}

/// Paramètres géométriques de tracé et de reconnaissance pour un texte orienté dans une image
#[derive(Debug, Clone)]
pub struct OrientedImageLabelSpec {
    pub anchor_x_px: f32,
    pub anchor_y_px: f32,
    pub angle_deg: f32,
    pub scale_px: usize,
    pub text: String,
    pub color_rgb: [u8; 3],
}

/// Dessine une étiquette de texte orientée (`0°`, `+30°`, `-30°`, `-90°`/`+90°`) directement en pixels
/// dans un buffer RGB (`width * height * 3`), avec un fond de contraste clair pour une lisibilité parfaite
/// sur un graphique scientifique / industriel.
pub fn draw_oriented_bitmap_label(
    rgb: &mut [u8],
    width: usize,
    height: usize,
    spec: &OrientedImageLabelSpec,
) {
    let scale = spec.scale_px.max(2) as f32;
    let char_h = 7.0 * scale;
    let char_step = 6.0 * scale;
    let total_w = (spec.text.chars().count() as f32) * char_step;

    // Dans l'espace écran/image (Y vers le bas), un angle PDF positif (+30° vers le haut-droite)
    // correspond à un angle image -angle_deg.
    let rad = -spec.angle_deg.to_radians();
    let cos_a = rad.cos();
    let sin_a = rad.sin();

    // 1. Cartouche blanc cassé sous l'étiquette orientée pour isoler le texte de la grille du graphe
    let pad = 3.0;
    let mut u = -pad;
    while u <= total_w + pad {
        let mut v = -pad;
        while v <= char_h + pad {
            let px = (spec.anchor_x_px + u * cos_a - v * sin_a).round() as i32;
            let py = (spec.anchor_y_px + u * sin_a + v * cos_a).round() as i32;
            if px >= 0 && px < width as i32 && py >= 0 && py < height as i32 {
                let idx = (py as usize * width + px as usize) * 3;
                rgb[idx] = 252;
                rgb[idx + 1] = 253;
                rgb[idx + 2] = 255;
            }
            v += 0.5;
        }
        u += 0.5;
    }

    // 2. Tracé des pixels d'encre de chaque glyphe 5x7 à l'angle `angle_deg`
    for (c_idx, ch) in spec.text.chars().enumerate() {
        let Some(rows) = glyph_bitmap_5x7(ch) else {
            continue;
        };
        let u_base = (c_idx as f32) * char_step;
        for (row_idx, row_bits) in rows.iter().enumerate() {
            for col_idx in 0..5 {
                if (row_bits >> (4 - col_idx)) & 1 == 1 {
                    let u0 = u_base + (col_idx as f32) * scale;
                    let v0 = (row_idx as f32) * scale;
                    let mut du = 0.0;
                    while du < scale {
                        let mut dv = 0.0;
                        while dv < scale {
                            let u_curr = u0 + du;
                            let v_curr = v0 + dv;
                            let px =
                                (spec.anchor_x_px + u_curr * cos_a - v_curr * sin_a).round() as i32;
                            let py =
                                (spec.anchor_y_px + u_curr * sin_a + v_curr * cos_a).round() as i32;
                            if px >= 0 && px < width as i32 && py >= 0 && py < height as i32 {
                                let idx = (py as usize * width + px as usize) * 3;
                                rgb[idx] = spec.color_rgb[0];
                                rgb[idx + 1] = spec.color_rgb[1];
                                rgb[idx + 2] = spec.color_rgb[2];
                            }
                            dv += 0.45;
                        }
                        du += 0.45;
                    }
                }
            }
        }
    }
}

/// Segment de texte orienté reconnu par le moteur OCR dans une image embarquée
#[derive(Debug, Clone)]
pub struct RecognizedOcrSegment {
    #[allow(dead_code)]
    pub text: String,
    pub angle_deg: f32,
    pub chars: Vec<PositionedChar>,
}

/// Seuil de confiance minimal pour conserver une ligne reconnue (évite le bruit sur les schémas).
const MIN_OCR_CONFIDENCE: f32 = 0.6;

const DET_MODEL_FILE: &str = "pp-ocrv5_mobile_det.onnx";
const REC_MODEL_FILE: &str = "latin_pp-ocrv5_mobile_rec.onnx";
const REC_DICT_FILE: &str = "ppocrv5_latin_dict.txt";
const LINE_ORI_MODEL_FILE: &str = "pp-lcnet_x0_25_textline_ori.onnx";
const ONNX_RUNTIME_DLL: &str = "onnxruntime.dll";

/// Pipeline PaddleOCR (PP-OCRv5 Latin) : détection orientée DBNet + classifieur 0°/180° + reconnaissance CTC.
pub struct OcrRecognizer {
    pipeline: Mutex<OAROCR>,
}

static OCR_RECOGNIZER: OnceLock<Result<OcrRecognizer, String>> = OnceLock::new();

/// Cherche `assets/models` dans le dossier courant puis autour de l'exécutable.
fn locate_model_dir() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("assets/models")];
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(4) {
            candidates.push(ancestor.join("assets/models"));
        }
    }
    candidates
        .into_iter()
        .find(|dir| {
            [DET_MODEL_FILE, REC_MODEL_FILE, REC_DICT_FILE, LINE_ORI_MODEL_FILE, ONNX_RUNTIME_DLL]
                .iter()
                .all(|file| dir.join(file).is_file())
        })
}

impl OcrRecognizer {
    /// Retourne le moteur partagé (chargé une seule fois), ou la raison pour laquelle il est indisponible.
    /// Le chargement prend plusieurs centaines de ms : ne jamais appeler depuis le thread de l'interface.
    pub fn shared() -> std::result::Result<&'static OcrRecognizer, &'static str> {
        OCR_RECOGNIZER
            .get_or_init(Self::load)
            .as_ref()
            .map_err(|e| e.as_str())
    }

    fn load() -> std::result::Result<OcrRecognizer, String> {
        let dir = locate_model_dir()
            .ok_or_else(|| "modèles absents de assets/models".to_string())?;
        // Chemin explicite : Windows 11 fournit une ancienne onnxruntime.dll dans System32.
        let runtime = std::fs::canonicalize(dir.join(ONNX_RUNTIME_DLL))
            .map_err(|e| format!("onnxruntime.dll illisible : {e}"))?;
        ort::init_from(runtime.to_string_lossy().as_ref())
            .map_err(|e| format!("onnxruntime.dll inutilisable : {e}"))?
            .commit();
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let session = OrtSessionConfig::new().with_intra_threads(cores.saturating_sub(1).clamp(1, 4));
        let pipeline = OAROCRBuilder::new(
            dir.join(DET_MODEL_FILE),
            dir.join(REC_MODEL_FILE),
            dir.join(REC_DICT_FILE),
        )
        .with_text_line_orientation_classification(dir.join(LINE_ORI_MODEL_FILE))
        .ort_session(session)
        .build()
        .map_err(|e| format!("chargement OCR impossible : {e}"))?;
        Ok(OcrRecognizer {
            pipeline: Mutex::new(pipeline),
        })
    }

    /// Reconnaît le texte d'une image placée dans la page à l'emplacement `rect_pt`.
    pub fn recognize_image(
        &self,
        image: RgbImage,
        rect_pt: PdfTopDownRect,
    ) -> Vec<RecognizedOcrSegment> {
        let (w, h) = (image.width() as f32, image.height() as f32);
        let scale = (rect_pt.width() / w.max(1.0), rect_pt.height() / h.max(1.0));
        let results = {
            let pipeline = self.pipeline.lock().unwrap_or_else(|e| e.into_inner());
            match pipeline.predict(vec![image]) {
                Ok(results) => results,
                Err(_) => return Vec::new(),
            }
        };
        results
            .iter()
            .flat_map(|page| page.text_regions.iter())
            .filter_map(|region| segment_from_region(region, rect_pt, scale))
            .collect()
    }
}

/// Avance relative d'un caractère, pour répartir les boîtes le long de la ligne détectée.
fn char_advance_weight(c: char) -> f32 {
    match c {
        'i' | 'j' | 'l' | 'I' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' | ' ' => 0.5,
        'f' | 't' | 'r' | '(' | ')' | '/' | '-' => 0.7,
        'm' | 'w' | 'M' | 'W' => 1.45,
        _ => 1.0,
    }
}

/// Convertit une ligne reconnue en caractères positionnés, en suivant le sens de lecture réel.
fn segment_from_region(
    region: &TextRegion,
    rect_pt: PdfTopDownRect,
    (scale_x, scale_y): (f32, f32),
) -> Option<RecognizedOcrSegment> {
    let text = region.text.as_deref()?.trim();
    if text.is_empty() || region.confidence.unwrap_or(0.0) < MIN_OCR_CONFIDENCE {
        return None;
    }

    let polygon = region.dt_poly.as_ref().unwrap_or(&region.bounding_box);
    let corners: [(f32, f32); 4] = if polygon.points.len() == 4 {
        std::array::from_fn(|i| (polygon.points[i].x, polygon.points[i].y))
    } else {
        let xs = polygon.points.iter().map(|p| p.x);
        let ys = polygon.points.iter().map(|p| p.y);
        let (x0, x1) = xs.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
        let (y0, y1) = ys.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    };
    let [p0, p1, p2, p3] = corners;
    let edge = |a: (f32, f32), b: (f32, f32)| (b.0 - a.0).hypot(b.1 - a.1);

    // Le pipeline tourne les découpes hautes de 90° dans le sens anti-horaire avant de les lire,
    // puis le classifieur signale les lignes à l'envers : on en déduit origine et axes de lecture.
    let is_tall = edge(p1, p2) >= edge(p0, p1) * 1.5;
    let is_flipped = region
        .orientation_angle
        .is_some_and(|a| (a - 180.0).abs() < 1.0);
    let (origin, reading_end, height_end) = match (is_tall, is_flipped) {
        (false, false) => (p0, p1, p3),
        (false, true) => (p2, p3, p1),
        (true, false) => (p1, p2, p0),
        (true, true) => (p3, p0, p2),
    };
    let u = (reading_end.0 - origin.0, reading_end.1 - origin.1);
    let v = (height_end.0 - origin.0, height_end.1 - origin.1);

    let raw_angle = (-u.1 * scale_y).atan2(u.0 * scale_x).to_degrees();
    let angle_deg = [0.0_f32, 90.0, -90.0, 180.0, -180.0]
        .into_iter()
        .find(|snap| (raw_angle - snap).abs() < 3.0)
        .unwrap_or(raw_angle);

    let total_weight: f32 = text.chars().map(char_advance_weight).sum();
    let mut advanced = 0.0_f32;
    let chars = text
        .chars()
        .map(|ch| {
            let t0 = advanced / total_weight;
            advanced += char_advance_weight(ch);
            let t1 = advanced / total_weight;
            let mut x_range = (f32::MAX, f32::MIN);
            let mut y_range = (f32::MAX, f32::MIN);
            for (t, s) in [(t0, 0.0), (t1, 0.0), (t0, 1.0), (t1, 1.0)] {
                let px = origin.0 + u.0 * t + v.0 * s;
                let py = origin.1 + u.1 * t + v.1 * s;
                let (x, y) = (rect_pt.x_min + px * scale_x, rect_pt.y_min + py * scale_y);
                x_range = (x_range.0.min(x), x_range.1.max(x));
                y_range = (y_range.0.min(y), y_range.1.max(y));
            }
            PositionedChar {
                ch,
                rect: PdfTopDownRect {
                    x_min: x_range.0,
                    y_min: y_range.0,
                    x_max: x_range.1,
                    y_max: y_range.1,
                    angle_deg,
                },
                is_ocr: true,
            }
        })
        .collect();

    Some(RecognizedOcrSegment {
        text: text.to_string(),
        angle_deg,
        chars,
    })
}

/// Moteur OCR hybride : ne traite que les images embarquées dépourvues de couche texte.
pub struct HybridOcrEngine;

/// Nombre de caractères présents sur une image à partir duquel on la considère déjà indexée.
const IMAGE_ALREADY_INDEXED_CHARS: usize = 12;

fn chars_inside_rect(layer: &PageTextLayer, rect: &PdfTopDownRect) -> usize {
    layer
        .chars
        .iter()
        .filter(|c| {
            let (cx, cy) = c.rect.center();
            rect.contains_pt(cx, cy, -2.0)
        })
        .count()
}

impl HybridOcrEngine {
    /// Liste les images embarquées d'une page et indique si elles possèdent déjà une couche texte.
    #[allow(dead_code)]
    pub fn detect_embedded_images_on_page(
        doc: &LoadedPdfDocument<'_>,
        page_index: usize,
        existing_layer: &PageTextLayer,
    ) -> Vec<EmbeddedImageRegion> {
        doc.list_page_image_regions(page_index)
            .into_iter()
            .map(|(rect_pt, pixel_width, pixel_height)| EmbeddedImageRegion {
                page_index,
                rect_pt,
                pixel_width,
                pixel_height,
                already_has_text_layer: chars_inside_rect(existing_layer, &rect_pt)
                    >= IMAGE_ALREADY_INDEXED_CHARS,
            })
            .collect()
    }

    /// Extrait les images à analyser. Retourne le nombre total d'images et celles sans couche texte.
    pub fn collect_images_to_scan(
        doc: &LoadedPdfDocument<'_>,
        page_index: usize,
        existing_layer: &PageTextLayer,
    ) -> (usize, Vec<(PdfTopDownRect, RgbImage)>) {
        let images = doc.extract_page_embedded_images(page_index);
        let total = images.len();
        let to_scan = images
            .into_iter()
            .filter(|(rect, _)| chars_inside_rect(existing_layer, rect) < IMAGE_ALREADY_INDEXED_CHARS)
            .collect();
        (total, to_scan)
    }

    /// Lance la reconnaissance sur des images déjà extraites (aucun appel PDFium, donc sans verrou global).
    /// Les lignes déjà présentes dans la couche native de l'image sont écartées pour éviter les doublons.
    pub fn recognize_images(
        recognizer: &OcrRecognizer,
        images: Vec<(PdfTopDownRect, RgbImage)>,
        existing_layer: &PageTextLayer,
    ) -> Vec<RecognizedOcrSegment> {
        let compact = |s: &str| -> String {
            s.chars()
                .filter(|c| !c.is_whitespace())
                .map(normalize_char)
                .collect()
        };
        let mut segments = Vec::new();
        for (rect, image) in images {
            let existing_in_image: String = existing_layer
                .chars
                .iter()
                .filter(|c| {
                    let (cx, cy) = c.rect.center();
                    rect.contains_pt(cx, cy, 0.0) && !c.ch.is_whitespace()
                })
                .map(|c| normalize_char(c.ch))
                .collect();
            segments.extend(
                recognizer
                    .recognize_image(image, rect)
                    .into_iter()
                    .filter(|seg| !existing_in_image.contains(&compact(&seg.text))),
            );
        }
        segments
    }

    /// Ajoute les segments reconnus à la couche de texte de la page.
    pub fn merge_segments_into_layer(layer: &mut PageTextLayer, segments: &[RecognizedOcrSegment]) {
        if segments.is_empty() {
            return;
        }
        let mut chars = std::mem::take(&mut layer.chars);
        for seg in segments {
            if !chars.is_empty() {
                chars.push(PositionedChar {
                    ch: ' ',
                    rect: PdfTopDownRect {
                        x_min: 0.0,
                        y_min: 0.0,
                        x_max: 0.0,
                        y_max: 0.0,
                        angle_deg: seg.angle_deg,
                    },
                    is_ocr: true,
                });
            }
            chars.extend(seg.chars.iter().cloned());
        }
        *layer = PageTextLayer::new(layer.page_index, chars);
    }

    /// Version synchrone (tests et options CLI). L'application interactive passe par `worker.rs`.
    pub fn scan_page_embedded_images(
        doc: &LoadedPdfDocument<'_>,
        page_index: usize,
        _page_dim: PageDimensions,
        existing_layer: &mut PageTextLayer,
    ) -> (usize, Vec<RecognizedOcrSegment>) {
        let (total, images) = Self::collect_images_to_scan(doc, page_index, existing_layer);
        let Ok(recognizer) = OcrRecognizer::shared() else {
            return (total, Vec::new());
        };
        let segments = Self::recognize_images(recognizer, images, existing_layer);
        Self::merge_segments_into_layer(existing_layer, &segments);
        (total, segments)
    }
}

/// Génère un fichier PDF Hybride "Word + Graphique JPG Multi-Orientations" (`test_pdfs/hybrid_word_jpg_ocr.pdf`) :
/// - Contient du **texte natif Word** normal (paragraphes d'introduction et de conclusion).
/// - Contient au centre un **vrai objet `XObject /Subtype /Image` (bitmap RGB 460x260 px)** représentant un graphique
///   scientifique dont les légendes d'axes sont dessinées **exclusivement en pixels dans l'image** selon 4 orientations :
///   1. Horizontal (`0°`) : `GRAPHE_JPG_RENDEMENT_MOTEUR` et `REGIME_ROTATION_4500RPM`
///   2. Vertical (`+90°` sur l'axe Y) : `AXE_Y_COUPLE_NM`
///   3. Diagonal montant (`+30°` le long de la courbe) : `SEUIL_DIAG_30DEG`
///   4. Diagonal descendant (`-30°`) : `DERIVE_THERM_85C`
///
/// Avant le passage de notre OCR Hybride, un lecteur PDF classique (ou `page.text()`) est **incapable** de trouver ces mots via `Ctrl+F` !
pub fn ensure_hybrid_word_jpg_pdf(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 1. Construction de l'image matricielle RGB du graphique (552 x 312 pixels, affichée en 460x260 pt)
    let img_w = 552usize;
    let img_h = 312usize;
    let mut rgb = vec![247u8; img_w * img_h * 3];

    // Bordure grise et grille claire du graphique
    for y in 0..img_h {
        for x in 0..img_w {
            let idx = (y * img_w + x) * 3;
            let on_border = x < 2 || x >= img_w - 2 || y < 2 || y >= img_h - 2;
            let on_grid = (x > 55 && x < img_w - 15 && y > 40 && y < img_h - 40)
                && (x % 55 == 0 || y % 45 == 0);
            let on_axes = (x == 55 && y >= 35 && y <= img_h - 40)
                || (y == img_h - 40 && x >= 55 && x <= img_w - 15);
            if on_border {
                rgb[idx] = 155;
                rgb[idx + 1] = 168;
                rgb[idx + 2] = 190;
            } else if on_axes {
                rgb[idx] = 145;
                rgb[idx + 1] = 155;
                rgb[idx + 2] = 175;
            } else if on_grid {
                rgb[idx] = 224;
                rgb[idx + 1] = 230;
                rgb[idx + 2] = 240;
            }
        }
    }

    // Courbe colorée claire dans le fond du graphique (luminance > 135 pour ne pas interférer avec l'encre noire)
    for x in 60..520 {
        let t = (x - 60) as f32 / 460.0;
        let curve_y = (250.0 - t * 155.0 - (t * 6.28).sin() * 16.0).round() as i32;
        for dy in -1..=1 {
            let py = curve_y + dy;
            if py >= 40 && py < (img_h as i32 - 42) {
                let idx = (py as usize * img_w + x) * 3;
                rgb[idx] = 95;
                rgb[idx + 1] = 175;
                rgb[idx + 2] = 245;
            }
        }
    }

    // Tracé des 5 étiquettes techniques UNIQUEMENT EN PIXELS dans l'image RGB (`scale_px = 3`, 0 chevauchement) :
    let labels = [
        // 1. Titre horizontal (0°) en haut du graphe JPG
        OrientedImageLabelSpec {
            anchor_x_px: 48.0,
            anchor_y_px: 14.0,
            angle_deg: 0.0,
            scale_px: 3,
            text: "GRAPHE_JPG_RENDEMENT_MOTEUR".to_string(),
            color_rgb: [15, 25, 45],
        },
        // 2. Titre vertical (+90°) le long de l'axe Y
        OrientedImageLabelSpec {
            anchor_x_px: 16.0,
            anchor_y_px: 290.0,
            angle_deg: 90.0,
            scale_px: 3,
            text: "AXE_Y_COUPLE_NM".to_string(),
            color_rgb: [20, 45, 95],
        },
        // 3. Étiquette en diagonale montante (+30°) dans la moitié gauche du graphe
        OrientedImageLabelSpec {
            anchor_x_px: 78.0,
            anchor_y_px: 205.0,
            angle_deg: 30.0,
            scale_px: 3,
            text: "SEUIL_DIAG_30DEG".to_string(),
            color_rgb: [135, 20, 20],
        },
        // 4. Étiquette en diagonale descendante (-30°) dans la moitié droite du graphe
        OrientedImageLabelSpec {
            anchor_x_px: 304.0,
            anchor_y_px: 124.0,
            angle_deg: -30.0,
            scale_px: 3,
            text: "DERIVE_THERM_85".to_string(),
            color_rgb: [25, 80, 45],
        },
        // 5. Titre de l'axe X horizontal (0°) en bas du graphe JPG
        OrientedImageLabelSpec {
            anchor_x_px: 68.0,
            anchor_y_px: 278.0,
            angle_deg: 0.0,
            scale_px: 3,
            text: "REGIME_ROTATION_4500RPM".to_string(),
            color_rgb: [15, 25, 45],
        },
    ];

    for spec in &labels {
        draw_oriented_bitmap_label(&mut rgb, img_w, img_h, spec);
    }

    // 2. Construction du document PDF Hybride avec `lopdf`
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();

    let font_helvetica = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_bold = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });

    // Objet `XObject /Subtype /Image` contenant le bitmap RGB du graphique
    let image_stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => img_w as i32,
            "Height" => img_h as i32,
            "ColorSpace" => Object::Name(b"DeviceRGB".to_vec()),
            "BitsPerComponent" => 8,
        },
        rgb,
    );
    let image_xobj_id = doc.add_object(image_stream);

    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_helvetica,
            "F2" => font_bold,
        },
        "XObject" => dictionary! {
            "ImGraph1" => image_xobj_id,
        },
    });

    let mut ops: Vec<Operation> = Vec::new();

    // Bandeau supérieur style rapport Word exporté en PDF
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new(
        "rg",
        vec![0.11.into(), 0.26.into(), 0.48.into()],
    ));
    ops.push(Operation::new(
        "re",
        vec![36.into(), 735.into(), 523.into(), 72.into()],
    ));
    ops.push(Operation::new("f", vec![]));
    ops.push(Operation::new("Q", vec![]));

    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new("rg", vec![1.0.into(), 1.0.into(), 1.0.into()]));
    ops.push(Operation::new("Tf", vec!["F2".into(), 16.into()]));
    ops.push(Operation::new("Td", vec![50.into(), 776.into()]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::string_literal(
            "RAPPORT TECHNIQUE WORD + GRAPHIQUE JPG EMBARQUE (JALON 6)",
        )],
    ));
    ops.push(Operation::new("ET", vec![]));

    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new("rg", vec![0.85.into(), 0.93.into(), 1.0.into()]));
    ops.push(Operation::new("Tf", vec!["F1".into(), 10.5.into()]));
    ops.push(Operation::new("Td", vec![50.into(), 752.into()]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::string_literal(
            "Demonstration Smart Hybrid OCR : Texte Natif Word + Image XObject Multi-Angles (0, +30, -30, +90 deg)",
        )],
    ));
    ops.push(Operation::new("ET", vec![]));

    // Paragraphes de texte natif (comme dans un document Microsoft Word exporté en PDF)
    let native_paragraphs = [
        "1. Contexte du document hybride (Texte Natif Word + Image Matricielle JPG) :",
        "Ce rapport contient du texte vectoriel natif exporte par Word, ainsi qu'une figure",
        "inserree sous forme d'image matricielle (XObject /Subtype /Image de 460x260 pixels).",
        "Dans un lecteur PDF classique, les legendes d'axes du graphique JPG ci-dessous sont",
        "totalement introuvables via Ctrl+F car elles ne sont que des pixels dans l'image :",
    ];
    for (i, line) in native_paragraphs.iter().enumerate() {
        ops.push(Operation::new("BT", vec![]));
        if i == 0 {
            ops.push(Operation::new(
                "rg",
                vec![0.11.into(), 0.26.into(), 0.48.into()],
            ));
            ops.push(Operation::new("Tf", vec!["F2".into(), 12.into()]));
        } else {
            ops.push(Operation::new(
                "rg",
                vec![0.15.into(), 0.18.into(), 0.22.into()],
            ));
            ops.push(Operation::new("Tf", vec!["F1".into(), 10.5.into()]));
        }
        let y = 706 - (i as i32 * 18);
        ops.push(Operation::new("Td", vec![48.into(), y.into()]));
        ops.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
        ops.push(Operation::new("ET", vec![]));
    }

    // Affichage de l'objet `XObject Image` ("ImGraph1") au centre de la page :
    // Position PDF bottom-up : x = 67 pt, y = 330 pt, largeur = 460 pt, hauteur = 260 pt
    // (Soit en coordonnées top-down : x = 67..527 pt, y = 252..512 pt)
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new(
        "cm",
        vec![
            460.into(),
            0.into(),
            0.into(),
            260.into(),
            67.into(),
            330.into(),
        ],
    ));
    ops.push(Operation::new(
        "Do",
        vec![Object::Name(b"ImGraph1".to_vec())],
    ));
    ops.push(Operation::new("Q", vec![]));

    // Paragraphe natif sous l'image expliquant comment tester avec Ctrl+F
    // (les identifiants complets ne figurent QUE sous forme de pixels dans l'image XObject ci-dessus !)
    let bottom_lines = [
        "2. Validation du moteur Smart OCR a 3 Vitesses (Jalon 6) :",
        "  * Texte incline a +30 deg dans l'image JPG : cherchez SEUIL_DIAG...",
        "  * Axe vertical a +90 deg ou pente a -30 deg : cherchez AXE_Y... ou DERIVE_THERM...",
        "  * Titres horizontaux 0 deg dans l'image JPG : cherchez GRAPHE_JPG... ou REGIME_ROTATION...",
        "  * Appuyez sur Ctrl+S : la couche texte invisible ISO 32000 (3 Tr + matrice Tm) est",
        "    ecrite dans le PDF pour que les prochaines ouvertures soient instantanees (0 ms) !",
    ];
    for (i, line) in bottom_lines.iter().enumerate() {
        ops.push(Operation::new("BT", vec![]));
        if i == 0 {
            ops.push(Operation::new(
                "rg",
                vec![0.11.into(), 0.26.into(), 0.48.into()],
            ));
            ops.push(Operation::new("Tf", vec!["F2".into(), 12.into()]));
        } else {
            ops.push(Operation::new(
                "rg",
                vec![0.18.into(), 0.22.into(), 0.28.into()],
            ));
            ops.push(Operation::new("Tf", vec!["F1".into(), 10.0.into()]));
        }
        let y = 295 - (i as i32 * 18);
        ops.push(Operation::new("Td", vec![48.into(), y.into()]));
        ops.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
        ops.push(Operation::new("ET", vec![]));
    }

    let content = Content { operations: ops };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Resources" => resources_id,
        "Contents" => content_id,
    });

    let pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => vec![page_id.into()],
        "Count" => 1,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();
    doc.save(path)?;
    Ok(())
}

/// Injecte dans le fichier PDF la couche de texte OCR invisible normalisée ISO 32000 (`3 Tr` + matrice orientée `Tm`)
/// afin qu'aux ouvertures suivantes (dans Super PDF ou dans Adobe Acrobat), tous les mots des graphiques JPG
/// soient immédiatement cherchables en `0 ms` sans jamais relancer l'OCR !
pub fn append_invisible_ocr_layer_to_doc(
    doc: &mut Document,
    pages_map: &BTreeMap<u32, ObjectId>,
    text_layers: &[PageTextLayer],
    page_dimensions: &[PageDimensions],
) -> Result<usize> {
    let mut injected_words = 0usize;

    let ocr_font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier-Bold",
        "Encoding" => "WinAnsiEncoding",
    });

    for layer in text_layers {
        let ocr_chars: Vec<&PositionedChar> = layer
            .chars
            .iter()
            .filter(|c| c.is_ocr && !c.ch.is_whitespace() && c.rect.width() > 0.2)
            .collect();

        if ocr_chars.is_empty() {
            continue;
        }

        let page_num = (layer.page_index + 1) as u32;
        let Some(&page_id) = pages_map.get(&page_num) else {
            continue;
        };
        let page_dim = page_dimensions
            .get(layer.page_index)
            .copied()
            .unwrap_or(PageDimensions::simple(595.0, 842.0));

        // Regrouper les caractères OCR consécutifs de même orientation en mots (avec le rect du 1er caractère et le rect union)
        let mut words: Vec<(String, PdfTopDownRect, PdfTopDownRect, f32)> = Vec::new();
        let mut current_word = String::new();
        let mut union_rect: Option<PdfTopDownRect> = None;
        let mut first_rect: Option<PdfTopDownRect> = None;
        let mut current_angle = 0.0_f32;

        for pc in &layer.chars {
            if !pc.is_ocr {
                continue;
            }
            if pc.ch.is_whitespace() || pc.rect.width() <= 0.2 {
                if !current_word.is_empty() {
                    if let (Some(u_r), Some(f_r)) = (union_rect.take(), first_rect.take()) {
                        words.push((std::mem::take(&mut current_word), u_r, f_r, current_angle));
                    }
                }
                continue;
            }
            if let Some(u_r) = &mut union_rect {
                if (current_angle - pc.rect.angle_deg).abs() > 5.0 {
                    let finished_u = *u_r;
                    let finished_f = first_rect.unwrap_or(finished_u);
                    words.push((
                        std::mem::take(&mut current_word),
                        finished_u,
                        finished_f,
                        current_angle,
                    ));
                    current_word.push(pc.ch);
                    union_rect = Some(pc.rect);
                    first_rect = Some(pc.rect);
                    current_angle = pc.rect.angle_deg;
                } else {
                    current_word.push(pc.ch);
                    *u_r = u_r.union(&pc.rect);
                }
            } else {
                current_word.push(pc.ch);
                union_rect = Some(pc.rect);
                first_rect = Some(pc.rect);
                current_angle = pc.rect.angle_deg;
            }
        }
        if !current_word.is_empty() {
            if let (Some(u_r), Some(f_r)) = (union_rect, first_rect) {
                words.push((current_word, u_r, f_r, current_angle));
            }
        }

        if words.is_empty() {
            continue;
        }

        // S'assurer que la police `/F_OCR` est déclarée dans les `/Resources /Font` de la page
        ensure_page_has_ocr_font(doc, page_id, ocr_font_id);

        let mut ops: Vec<Operation> = Vec::new();
        ops.push(Operation::new("q", vec![]));
        ops.push(Operation::new("BT", vec![]));
        // Mode de rendu de texte PDF 3 = Invisible (ni remplissage ni contour, 100 % sélectionnable et cherchable)
        ops.push(Operation::new("Tr", vec![3.into()]));

        for (word_str, word_rect, start_rect, angle_deg) in words {
            let rad = (angle_deg + page_dim.rotation_deg as f32).to_radians();
            let cos_a = rad.cos();
            let sin_a = rad.sin();

            // Coordonnées PDF bottom-up exactes du 1er caractère du mot
            let (tx, ty) = page_dim.view_to_pdf(start_rect.x_min, start_rect.y_max - 2.0);

            // Calibrer la taille de police `Courier-Bold` (chasse 0.60 em) pour couvrir toute l'étendue géométrique du mot
            let span_pt = word_rect.width().hypot(word_rect.height());
            let n_chars = (word_str.chars().count() as f32).max(1.0);
            let font_size = (span_pt / (n_chars * 0.65)).clamp(11.0, 22.0);

            ops.push(Operation::new(
                "Tf",
                vec!["F_OCR".into(), font_size.into()],
            ));
            ops.push(Operation::new(
                "Tm",
                vec![
                    cos_a.into(),
                    sin_a.into(),
                    (-sin_a).into(),
                    cos_a.into(),
                    tx.into(),
                    ty.into(),
                ],
            ));
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(
                    encode_win_ansi(&word_str),
                    lopdf::StringFormat::Literal,
                )],
            ));
            injected_words += 1;
        }

        ops.push(Operation::new("ET", vec![]));
        ops.push(Operation::new("Q", vec![]));

        let ocr_content = Content { operations: ops };
        if let Ok(encoded) = ocr_content.encode() {
            let stream_id = doc.add_object(Stream::new(dictionary! {}, encoded));
            append_content_stream_to_page(doc, page_id, stream_id);
        }
    }

    Ok(injected_words)
}

/// Encode une chaîne en WinAnsi pour les flux de contenu (caractères hors table remplacés par `?`).
pub fn encode_win_ansi(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c {
            '\u{20}'..='\u{7E}' | '\u{A0}'..='\u{FF}' => c as u8,
            '€' => 0x80,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '–' => 0x96,
            '—' => 0x97,
            'œ' => 0x9C,
            'Œ' => 0x8C,
            _ => b'?',
        })
        .collect()
}

fn ensure_page_has_ocr_font(doc: &mut Document, page_id: ObjectId, ocr_font_id: ObjectId) {
    let res_obj = doc
        .get_object(page_id)
        .and_then(|o| o.as_dict())
        .and_then(|d| d.get(b"Resources"))
        .cloned();

    match res_obj {
        Ok(Object::Reference(res_ref)) => {
            if let Ok(res_dict) = doc.get_object_mut(res_ref).and_then(|o| o.as_dict_mut()) {
                if let Ok(Object::Reference(font_ref)) = res_dict.get(b"Font").cloned() {
                    if let Ok(f_dict) = doc.get_object_mut(font_ref).and_then(|o| o.as_dict_mut()) {
                        f_dict.set("F_OCR", Object::Reference(ocr_font_id));
                    }
                } else if let Ok(font_dict) = res_dict.get_mut(b"Font").and_then(|o| o.as_dict_mut())
                {
                    font_dict.set("F_OCR", Object::Reference(ocr_font_id));
                } else {
                    res_dict.set(
                        "Font",
                        Object::Dictionary(dictionary! { "F_OCR" => ocr_font_id }),
                    );
                }
            }
        }
        Ok(Object::Dictionary(mut res_dict)) => {
            if let Ok(font_dict) = res_dict.get_mut(b"Font").and_then(|o| o.as_dict_mut()) {
                font_dict.set("F_OCR", Object::Reference(ocr_font_id));
            } else {
                res_dict.set(
                    "Font",
                    Object::Dictionary(dictionary! { "F_OCR" => ocr_font_id }),
                );
            }
            if let Ok(page_dict) = doc.get_object_mut(page_id).and_then(|o| o.as_dict_mut()) {
                page_dict.set("Resources", Object::Dictionary(res_dict));
            }
        }
        _ => {}
    }
}

fn append_content_stream_to_page(doc: &mut Document, page_id: ObjectId, new_stream_id: ObjectId) {
    if let Ok(page_dict) = doc.get_object_mut(page_id).and_then(|o| o.as_dict_mut()) {
        match page_dict.get(b"Contents").cloned() {
            Ok(Object::Reference(existing_ref)) => {
                page_dict.set(
                    "Contents",
                    Object::Array(vec![
                        Object::Reference(existing_ref),
                        Object::Reference(new_stream_id),
                    ]),
                );
            }
            Ok(Object::Array(mut arr)) => {
                arr.push(Object::Reference(new_stream_id));
                page_dict.set("Contents", Object::Array(arr));
            }
            _ => {
                page_dict.set("Contents", Object::Reference(new_stream_id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::save_annotations_and_ocr_to_pdf;
    use crate::pdf_engine::PdfEngine;
    use crate::tabs::DocumentTab;
    use crate::text_search::SearchBarState;
    use egui::Color32;
    use std::path::PathBuf;

    #[test]
    fn test_hybrid_word_jpg_pdf_native_vs_ocr_multi_orientation() {
        if OcrRecognizer::shared().is_err() {
            eprintln!("Modèles OCR absents : test ignoré");
            return;
        }
        let pdf_path = PathBuf::from("test_pdfs/unit_test_hybrid_word_jpg.pdf");
        let _ = std::fs::remove_file(&pdf_path);
        ensure_hybrid_word_jpg_pdf(&pdf_path).expect("Doit générer le PDF hybride Word + JPG");

        let engine = PdfEngine::instance();
        let doc = engine
            .load_document(&pdf_path)
            .expect("Doit charger le PDF hybride");
        assert_eq!(doc.page_count(), 1);

        // 1. Avant l'OCR : le texte natif Word est présent, mais les légendes d'axes dans l'image JPG
        // sont exclusivement des pixels et sont donc 100 % introuvables via Ctrl+F !
        let mut layer = doc.extract_single_page_text_layer(0);
        let mut search_before = SearchBarState::default();
        search_before.query = "SEUIL_DIAG_30DEG".to_string();
        search_before.execute_search(std::slice::from_ref(&layer));
        assert_eq!(
            search_before.matches.len(),
            0,
            "Avant l'OCR, le texte diagonal dans l'image JPG ne doit pas exister dans la couche native"
        );

        let regions_before = HybridOcrEngine::detect_embedded_images_on_page(&doc, 0, &layer);
        assert_eq!(
            regions_before.len(),
            1,
            "Doit détecter exactement 1 objet XObject Image embarqué sur la page"
        );
        assert!(
            !regions_before[0].already_has_text_layer,
            "Avant l'OCR, l'image JPG embarquée ne possède aucune couche texte sous-jacente"
        );

        // 2. Exécution du moteur OCR Hybride Multi-Orientation sur l'image embarquée
        let page_dim = doc.page_dimensions[0];
        let (img_count, segments) =
            HybridOcrEngine::scan_page_embedded_images(&doc, 0, page_dim, &mut layer);
        assert_eq!(img_count, 1);
        assert!(
            segments.len() >= 4,
            "Doit reconnaître les étiquettes orientées (0°, +90°, +30°, -30°) dans l'image JPG, reçu : {:?}",
            segments.iter().map(|s| (&s.text, s.angle_deg)).collect::<Vec<_>>()
        );

        let seg_debug: Vec<(String, f32)> = segments
            .iter()
            .map(|s| (s.text.clone(), s.angle_deg))
            .collect();
        let found_near = |needle: &str, angle: f32| {
            segments.iter().any(|s| {
                s.text.to_uppercase().contains(needle) && (s.angle_deg - angle).abs() < 12.0
            })
        };
        assert!(found_near("RENDEMENT", 0.0), "Titre 0° attendu : {seg_debug:?}");
        assert!(found_near("COUPLE", 90.0), "Axe +90° attendu : {seg_debug:?}");
        assert!(found_near("SEUIL", 30.0), "Texte +30° attendu : {seg_debug:?}");
        assert!(found_near("THERM", -30.0), "Texte -30° attendu : {seg_debug:?}");

        // 3. Vérifier que Ctrl+F trouve l'occurrence OCR dans l'image, avec son orientation réelle
        let mut search = SearchBarState::default();
        search.query = "SEUIL".to_string();
        search.execute_search(&[layer]);
        let ocr_match = search
            .matches
            .iter()
            .find(|m| m.is_ocr)
            .expect("Ctrl+F doit trouver l'occurrence OCR dans le graphique JPG");
        assert!((ocr_match.angle_deg - 30.0).abs() < 12.0);
    }

    #[test]
    fn test_ocr_persistence_tr3_invisible_layer_roundtrip() {
        if OcrRecognizer::shared().is_err() {
            eprintln!("Modèles OCR absents : test ignoré");
            return;
        }
        let src_path = PathBuf::from("test_pdfs/unit_test_ocr_tr3_src.pdf");
        let saved_path = PathBuf::from("test_pdfs/unit_test_ocr_tr3_saved.pdf");
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&saved_path);

        ensure_hybrid_word_jpg_pdf(&src_path).expect("Doit générer le PDF source");
        let engine = PdfEngine::instance();
        let doc = engine.load_document(&src_path).expect("Doit charger le PDF");
        let page_dim = doc.page_dimensions[0];
        let mut layer = doc.extract_single_page_text_layer(0);

        let (_, segments) =
            HybridOcrEngine::scan_page_embedded_images(&doc, 0, page_dim, &mut layer);
        assert!(!segments.is_empty());

        // Sauvegarde Ctrl+S avec injection de la couche texte invisible `3 Tr`
        save_annotations_and_ocr_to_pdf(&src_path, &saved_path, &[], &[layer], &[page_dim])
            .expect("Doit sauvegarder la couche OCR Tr 3 dans le PDF");

        // Réouverture du PDF sauvegardé SANS exécuter l'OCR :
        // PDFium (comme Adobe Acrobat) doit extraire directement la couche texte `3 Tr` en 0 ms !
        let doc_reloaded = engine
            .load_document(&saved_path)
            .expect("Doit recharger le PDF enrichi Tr 3");
        let reloaded_layer = doc_reloaded.extract_single_page_text_layer(0);

        let regions_after =
            HybridOcrEngine::detect_embedded_images_on_page(&doc_reloaded, 0, &reloaded_layer);
        assert_eq!(regions_after.len(), 1);
        assert!(
            regions_after[0].already_has_text_layer,
            "Après sauvegarde Ctrl+S (Tr 3), l'image embarquée possède déjà sa couche texte et ne déclenche plus de recalcul OCR !"
        );

        let mut search = SearchBarState::default();
        search.query = "COUPLE".to_string();
        search.execute_search(&[reloaded_layer]);
        assert_eq!(
            search.matches.len(),
            1,
            "Le PDF réouvert doit trouver directement l'axe vertical de l'image via la couche Tr 3 sans relancer l'OCR"
        );
    }

    #[test]
    fn test_three_speed_ocr_scheduler_instant_eco_turbo() {
        if OcrRecognizer::shared().is_err() {
            eprintln!("Modèles OCR absents : test ignoré");
            return;
        }
        let pdf_path = PathBuf::from("test_pdfs/unit_test_ocr_scheduler.pdf");
        let _ = std::fs::remove_file(&pdf_path);
        ensure_hybrid_word_jpg_pdf(&pdf_path).expect("Doit générer le PDF");

        let engine = PdfEngine::instance();
        let doc = engine.load_document(&pdf_path).expect("Doit charger le PDF");
        let mut tab = DocumentTab::new_active(1, doc, Color32::WHITE);

        // Vitesse 1 : Ouverture instantanée (0 ms, aucune page scannée de manière bloquante)
        assert_eq!(tab.ocr.mode, OcrSpeedMode::InstantOpen);
        assert_eq!(tab.ocr.scanned_pages_count(), 0);

        // Vitesse 2 : Mode Éco Tâche de fond (scanne la page visible au repos)
        let added = tab.step_eco_ocr_background(0, engine);
        assert_eq!(tab.ocr.mode, OcrSpeedMode::EcoBackground);
        assert!(added > 20, "Le worker Éco doit reconnaître les caractères du graphique JPG");
        assert!(tab.ocr.is_fully_scanned(1));
    }
}

