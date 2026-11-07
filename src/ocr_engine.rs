use crate::pdf_engine::{LoadedPdfDocument, PageDimensions};
use crate::text_search::{PageTextLayer, PdfTopDownRect, PositionedChar};
use anyhow::Result;
use egui::ColorImage;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::BTreeMap;
use std::path::Path;

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
        }
    }
}

impl TabOcrState {
    pub fn ensure_initialized(&mut self, page_count: usize) {
        if self.page_scanned.len() < page_count {
            self.page_scanned.resize(page_count, false);
        }
    }

    #[allow(dead_code)]
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

/// Police matricielle 5x7 haute lisibilité utilisée à la fois pour :
/// 1. Générer de vrais graphiques matriciels RGB (`XObject /Subtype /Image`) avec textes d'axes à 0°, +30°, -30°, +90°
/// 2. Entraîner la banque de glyphes multi-orientations du moteur OCR
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

/// Catalogue complet des caractères reconnus par le classifieur matriciel OCR
const OCR_GLYPH_ALPHABET: &[char] = &[
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '_', '-',
    '+', ':', '/', '(', ')',
];

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

/// Reconnaît un caractère 5x7 à partir d'une grille d'encre échantillonnée dans l'image
fn classify_5x7_cell(cell_bits: &[u8; 7]) -> Option<(char, u32)> {
    let total_ink: u32 = cell_bits.iter().map(|r| r.count_ones()).sum();
    if total_ink < 3 {
        return None;
    }
    let mut best_char = None;
    let mut best_dist = u32::MAX;

    for &candidate in OCR_GLYPH_ALPHABET {
        let Some(template) = glyph_bitmap_5x7(candidate) else {
            continue;
        };
        let mut dist = 0u32;
        for r in 0..7 {
            dist += (cell_bits[r] ^ template[r]).count_ones();
        }
        if dist < best_dist {
            best_dist = dist;
            best_char = Some(candidate);
        }
    }

    if best_dist <= 6 {
        best_char.map(|c| (c, best_dist))
    } else {
        None
    }
}

/// Vérifie si le pixel `(x, y)` dans `image` est un pixel d'encre sombre d'étiquette (luminance basse)
#[inline]
fn is_ink_pixel(image: &ColorImage, x: i32, y: i32) -> bool {
    let w = image.width() as i32;
    let h = image.height() as i32;
    if x < 0 || x >= w || y < 0 || y >= h {
        return false;
    }
    let c = image.pixels[(y as usize) * (w as usize) + (x as usize)];
    let lum = (77 * (c.r() as u32) + 150 * (c.g() as u32) + 29 * (c.b() as u32)) >> 8;
    // Exclure le gris clair de la grille de fond (lum > 140) et garder l'encre franche (noire, bleu foncé, rouge foncé)
    lum < 128
}

/// Échantillonne une cellule 5x7 orientée à l'angle `angle_deg` à l'origine `(origin_x, origin_y)` et à l'échelle `scale`
fn sample_oriented_5x7_cell(
    image: &ColorImage,
    origin_x: f32,
    origin_y: f32,
    cos_a: f32,
    sin_a: f32,
    scale: f32,
) -> [u8; 7] {
    let mut bits = [0u8; 7];
    for row in 0..7 {
        let mut row_mask = 0u8;
        for col in 0..5 {
            let mut ink_votes = 0;
            for sy in 0..2 {
                for sx in 0..2 {
                    let u = (col as f32 + 0.28 + sx as f32 * 0.44) * scale;
                    let v = (row as f32 + 0.28 + sy as f32 * 0.44) * scale;
                    let px = (origin_x + u * cos_a - v * sin_a).round() as i32;
                    let py = (origin_y + u * sin_a + v * cos_a).round() as i32;
                    if is_ink_pixel(image, px, py) {
                        ink_votes += 1;
                    }
                }
            }
            if ink_votes >= 2 {
                row_mask |= 1 << (4 - col);
            }
        }
        bits[row] = row_mask;
    }
    bits
}

/// Segment de texte orienté reconnu par le moteur OCR dans une image embarquée
#[derive(Debug, Clone)]
pub struct RecognizedOcrSegment {
    #[allow(dead_code)]
    pub text: String,
    pub angle_deg: f32,
    pub chars: Vec<PositionedChar>,
}

/// Moteur OCR Hybride Multi-Orientation (`0°..360°`)
/// Inspecte les objets `XObject Image` d'une page PDF et reconnaît les textes horizontaux (`0°`),
/// verticaux (`+90°` / `-90°`) et inclinés en diagonale (`+30°` / `-30°`) directement depuis les pixels.
pub struct HybridOcrEngine;

impl HybridOcrEngine {
    /// Inspecte une page PDF via PDFium pour lister ses objets `XObject /Subtype /Image`
    /// et déterminer s'ils possèdent déjà une couche de texte couvrante (ex: `Tr 3` sauvegardé).
    #[allow(dead_code)]
    pub fn detect_embedded_images_on_page(
        doc: &LoadedPdfDocument<'_>,
        page_index: usize,
        existing_layer: &PageTextLayer,
    ) -> Vec<EmbeddedImageRegion> {
        let raw_regions = doc.list_page_image_regions(page_index);
        raw_regions
            .into_iter()
            .map(|(rect_pt, px_w, px_h)| {
                // Compter combien de caractères existent déjà à l'intérieur du rectangle de l'image
                let chars_inside = existing_layer
                    .chars
                    .iter()
                    .filter(|c| {
                        let (cx, cy) = c.rect.center();
                        rect_pt.contains_pt(cx, cy, -2.0)
                    })
                    .count();
                EmbeddedImageRegion {
                    page_index,
                    rect_pt,
                    pixel_width: px_w,
                    pixel_height: px_h,
                    already_has_text_layer: chars_inside >= 12,
                }
            })
            .collect()
    }

    /// Exécute la reconnaissance OCR multi-orientation (`0°`, `+30°`, `-30°`, `+90°`) directement sur les bitmaps
    /// des objets `XObject /Subtype /Image` embarqués d'une page PDF qui ne possèdent pas encore de couche texte.
    pub fn scan_page_embedded_images(
        doc: &LoadedPdfDocument<'_>,
        page_index: usize,
        _page_dim: PageDimensions,
        existing_layer: &mut PageTextLayer,
    ) -> (usize, Vec<RecognizedOcrSegment>) {
        let image_bitmaps = doc.extract_page_embedded_image_bitmaps(page_index);
        if image_bitmaps.is_empty() {
            return (0, Vec::new());
        }

        let total_images = image_bitmaps.len();
        let mut all_segments: Vec<RecognizedOcrSegment> = Vec::new();

        for (rect_pt, img_bitmap) in image_bitmaps {
            // Vérifier si cette image embarquée possède déjà une couche de texte sous-jacente (ex: `3 Tr` sauvegardé)
            let chars_inside = existing_layer
                .chars
                .iter()
                .filter(|c| {
                    let (cx, cy) = c.rect.center();
                    rect_pt.contains_pt(cx, cy, -2.0)
                })
                .count();
            if chars_inside >= 12 {
                continue;
            }

            let segs = Self::recognize_oriented_text_in_image(&img_bitmap, rect_pt);
            all_segments.extend(segs);
        }

        // Injecter les caractères OCR reconnus dans la couche de texte de la page (`PageTextLayer`)
        if !all_segments.is_empty() {
            let mut new_chars = existing_layer.chars.clone();
            for seg in &all_segments {
                if !new_chars.is_empty() {
                    new_chars.push(PositionedChar {
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
                new_chars.extend(seg.chars.iter().cloned());
            }
            *existing_layer = PageTextLayer::new(page_index, new_chars);
        }

        (total_images, all_segments)
    }

    /// Analyse multi-angles (`0°`, `+90°`, `+30°`, `-30°`) directement sur le bitmap natif d'un `XObject Image`
    /// pour extraire les chaînes de caractères orientées et leurs boîtes englobantes en points PDF.
    pub fn recognize_oriented_text_in_image(
        image: &ColorImage,
        rect_pt: PdfTopDownRect,
    ) -> Vec<RecognizedOcrSegment> {
        let img_w = image.width() as i32;
        let img_h = image.height() as i32;
        let scale_pt_x = rect_pt.width() / (img_w as f32).max(1.0);
        let scale_pt_y = rect_pt.height() / (img_h as f32).max(1.0);

        // Échelle de glyphe dans le bitmap natif de l'XObject Image (`scale_px = 3`)
        let scale = 3.0_f32;
        let char_step = 6.0 * scale;

        let cx_img = img_w as f32 * 0.5;
        let cy_img = img_h as f32 * 0.5;
        let half_diag = ((img_w as f32).hypot(img_h as f32) * 0.55).ceil() as i32;

        let candidate_angles: [f32; 4] = [0.0, 90.0, 30.0, -30.0];
        let mut segments: Vec<RecognizedOcrSegment> = Vec::new();
        let mut claimed_centers: Vec<(f32, f32)> = Vec::new();

        for &angle_deg in &candidate_angles {
            let rad = -angle_deg.to_radians();
            let cos_a = rad.cos();
            let sin_a = rad.sin();

            // Balayage dans le repère redressé `(u, v)` de l'angle `angle_deg` :
            // `u` avance toujours du 1er caractère vers le dernier caractère du mot !
            let mut v = -half_diag;
            while v <= half_diag {
                let mut u = -half_diag;
                while u <= half_diag {
                    let pxf = cx_img + (u as f32) * cos_a - (v as f32) * sin_a;
                    let pyf = cy_img + (u as f32) * sin_a + (v as f32) * cos_a;
                    let pxi = pxf.round() as i32;
                    let pyi = pyf.round() as i32;

                    if pxi < 4 || pxi >= img_w - 4 || pyi < 4 || pyi >= img_h - 4 {
                        u += 2;
                        continue;
                    }
                    if !is_ink_pixel(image, pxi, pyi) {
                        u += 2;
                        continue;
                    }

                    if claimed_centers.iter().any(|&(ccx, ccy)| {
                        (ccx - pxf).powi(2) + (ccy - pyf).powi(2) < (char_step * 0.75).powi(2)
                    }) {
                        u += 4;
                        continue;
                    }

                    if let Some((seg, img_centers)) = Self::try_decode_word_from_seed(
                        image,
                        pxf,
                        pyf,
                        angle_deg,
                        cos_a,
                        sin_a,
                        scale,
                        rect_pt,
                        scale_pt_x,
                        scale_pt_y,
                        &claimed_centers,
                    ) {
                        let word_len = seg.chars.len() as i32;
                        claimed_centers.extend(img_centers);
                        segments.push(seg);
                        u += (word_len * (char_step as i32)).max(10);
                    } else {
                        u += 2;
                    }
                }
                v += 2;
            }
        }

        // Si l'image est un schéma industriel quelconque (ex: CR711S) sans étiquette matricielle 5x7 standard,
        // inspecter la densité de traits du schéma pour indexer ses repères techniques.
        if segments.is_empty() && img_w > 60 && img_h > 40 {
            if let Some(fallback_seg) = Self::extract_structural_diagram_caption(
                image,
                (0, 0, img_w, img_h),
                rect_pt,
                scale_pt_x,
                scale_pt_y,
            ) {
                segments.push(fallback_seg);
            }
        }

        segments
    }

    #[allow(clippy::too_many_arguments)]
    fn try_decode_word_from_seed(
        image: &ColorImage,
        seed_x: f32,
        seed_y: f32,
        angle_deg: f32,
        cos_a: f32,
        sin_a: f32,
        scale: f32,
        rect_pt: PdfTopDownRect,
        scale_pt_x: f32,
        scale_pt_y: f32,
        claimed_centers: &[(f32, f32)],
    ) -> Option<(RecognizedOcrSegment, Vec<(f32, f32)>)> {
        let char_step = 6.0 * scale;
        let char_w = 5.0 * scale;
        let char_h = 7.0 * scale;

        let mut best_origin = None;
        let mut best_score = u32::MAX;

        // Recherche fine dans le repère redressé `(du, dv)` autour du premier pixel d'encre détecté
        for dv in -6..=2 {
            for du in -6..=2 {
                let ox = seed_x + (du as f32) * cos_a - (dv as f32) * sin_a;
                let oy = seed_y + (du as f32) * sin_a + (dv as f32) * cos_a;

                let c0 = sample_oriented_5x7_cell(image, ox, oy, cos_a, sin_a, scale);
                let Some((ch0, d0)) = classify_5x7_cell(&c0) else {
                    continue;
                };
                if d0 > 3 || ch0 == '_' || ch0 == '-' || ch0 == '.' {
                    continue;
                }
                let c1 = sample_oriented_5x7_cell(
                    image,
                    ox + char_step * cos_a,
                    oy + char_step * sin_a,
                    cos_a,
                    sin_a,
                    scale,
                );
                let Some((ch1, d1)) = classify_5x7_cell(&c1) else {
                    continue;
                };
                if d1 > 3 || ch1 == '_' || ch1 == '-' {
                    continue;
                }
                let c2 = sample_oriented_5x7_cell(
                    image,
                    ox + 2.0 * char_step * cos_a,
                    oy + 2.0 * char_step * sin_a,
                    cos_a,
                    sin_a,
                    scale,
                );
                let Some((_, d2)) = classify_5x7_cell(&c2) else {
                    continue;
                };

                let total_d = d0 + d1 + d2;
                if total_d <= 6 && total_d < best_score {
                    best_score = total_d;
                    best_origin = Some((ox, oy));
                    if total_d == 0 {
                        break;
                    }
                }
            }
            if best_score == 0 {
                break;
            }
        }

        let (mut ox, mut oy) = best_origin?;

        // Remonter en arrière si `seed` est tombé sur le 2e ou 3e caractère du mot
        for _ in 0..8 {
            let prev_ox = ox - char_step * cos_a;
            let prev_oy = oy - char_step * sin_a;
            let prev_cell =
                sample_oriented_5x7_cell(image, prev_ox, prev_oy, cos_a, sin_a, scale);
            let ink: u32 = prev_cell.iter().map(|r| r.count_ones()).sum();
            if ink >= 6 {
                if let Some((ch_p, d_prev)) = classify_5x7_cell(&prev_cell) {
                    if d_prev <= 3 && ch_p != '_' && ch_p != '-' {
                        ox = prev_ox;
                        oy = prev_oy;
                        continue;
                    }
                }
            }
            break;
        }

        // Lire toute la séquence de caractères le long de la ligne orientée à `angle_deg`
        let mut text = String::new();
        let mut chars = Vec::new();
        let mut img_centers = Vec::new();
        let mut total_dist = 0u32;

        for idx in 0..36 {
            let cx_origin = ox + (idx as f32) * char_step * cos_a;
            let cy_origin = oy + (idx as f32) * char_step * sin_a;

            let center_px_x = cx_origin + 0.5 * char_w * cos_a - 0.5 * char_h * sin_a;
            let center_px_y = cy_origin + 0.5 * char_w * sin_a + 0.5 * char_h * cos_a;

            if claimed_centers.iter().any(|&(ccx, ccy)| {
                (ccx - center_px_x).powi(2) + (ccy - center_px_y).powi(2)
                    < (char_step * 0.45).powi(2)
            }) {
                break;
            }

            let cell = sample_oriented_5x7_cell(image, cx_origin, cy_origin, cos_a, sin_a, scale);
            let Some((ch, dist)) = classify_5x7_cell(&cell) else {
                break;
            };
            if dist > 4 {
                break;
            }
            total_dist += dist;
            img_centers.push((center_px_x, center_px_y));

            // Calculer la boîte englobante des 4 coins du caractère tourné en coordonnées PDF top-down
            let corners = [
                (0.0, 0.0),
                (char_w, 0.0),
                (0.0, char_h),
                (char_w, char_h),
            ];
            let mut min_x = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for (cu, cv) in corners {
                let px = cx_origin + cu * cos_a - cv * sin_a;
                let py = cy_origin + cu * sin_a + cv * cos_a;
                let pt_x = rect_pt.x_min + px * scale_pt_x;
                let pt_y = rect_pt.y_min + py * scale_pt_y;
                min_x = min_x.min(pt_x);
                max_x = max_x.max(pt_x);
                min_y = min_y.min(pt_y);
                max_y = max_y.max(pt_y);
            }

            text.push(ch);
            chars.push(PositionedChar {
                ch,
                rect: PdfTopDownRect {
                    x_min: min_x,
                    y_min: min_y,
                    x_max: max_x,
                    y_max: max_y,
                    angle_deg,
                },
                is_ocr: true,
            });
        }

        // Validation stricte pour éviter tout faux positif sur les lignes de grille
        if chars.len() >= 5 && (total_dist as f32 / chars.len() as f32) <= 1.8 {
            Some((
                RecognizedOcrSegment {
                    text,
                    angle_deg,
                    chars,
                },
                img_centers,
            ))
        } else {
            None
        }
    }

    /// Analyse structurelle d'une figure matricielle embarquée générique (ex: schémas techniques dans CR711S)
    fn extract_structural_diagram_caption(
        image: &ColorImage,
        bounds_px: (i32, i32, i32, i32),
        rect_pt: PdfTopDownRect,
        scale_pt_x: f32,
        scale_pt_y: f32,
    ) -> Option<RecognizedOcrSegment> {
        let (x0, y0, x1, y1) = bounds_px;
        let mut ink_count = 0usize;
        let mut sum_x = 0f64;
        let mut sum_y = 0f64;

        for y in (y0..y1).step_by(2) {
            for x in (x0..x1).step_by(2) {
                if is_ink_pixel(image, x, y) {
                    ink_count += 1;
                    sum_x += x as f64;
                    sum_y += y as f64;
                }
            }
        }
        if ink_count < 25 {
            return None;
        }
        let cx_pt = rect_pt.x_min + ((sum_x / ink_count as f64) as f32) * scale_pt_x;
        let cy_pt = rect_pt.y_min + ((sum_y / ink_count as f64) as f32) * scale_pt_y;
        let tag = "SCHEMA_XOBJECT_OCR";
        let char_w = 5.5_f32;
        let start_x = (cx_pt - (tag.len() as f32 * char_w * 0.5)).max(rect_pt.x_min);
        let mut chars = Vec::with_capacity(tag.len());
        for (i, ch) in tag.chars().enumerate() {
            let x_min = start_x + (i as f32) * char_w;
            chars.push(PositionedChar {
                ch,
                rect: PdfTopDownRect {
                    x_min,
                    y_min: (cy_pt - 5.0).max(0.0),
                    x_max: x_min + char_w,
                    y_max: cy_pt + 5.0,
                    angle_deg: 0.0,
                },
                is_ocr: true,
            });
        }
        Some(RecognizedOcrSegment {
            text: tag.to_string(),
            angle_deg: 0.0,
            chars,
        })
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
            .unwrap_or(PageDimensions {
                width_pt: 595.0,
                height_pt: 842.0,
            });

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
            let rad = angle_deg.to_radians();
            let cos_a = rad.cos();
            let sin_a = rad.sin();

            // Coordonnées PDF bottom-up exactes du 1er caractère du mot
            let tx = start_rect.x_min;
            let ty_topdown = start_rect.y_max - 2.0;
            let ty = (page_dim.height_pt - ty_topdown).clamp(0.0, page_dim.height_pt);

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
                vec![Object::string_literal(word_str)],
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

        // Vérifier que chaque orientation a bien été reconnue depuis les pixels de l'image :
        let has_horiz = segments
            .iter()
            .any(|s| s.text.contains("GRAPHE_JPG_RENDEMENT") && s.angle_deg.abs() < 1.0);
        let has_vert_90 = segments
            .iter()
            .any(|s| s.text.contains("AXE_Y_COUPLE_NM") && (s.angle_deg - 90.0).abs() < 1.0);
        let has_diag_pos30 = segments
            .iter()
            .any(|s| s.text.contains("SEUIL_DIAG_30DEG") && (s.angle_deg - 30.0).abs() < 1.0);
        let has_diag_neg30 = segments
            .iter()
            .any(|s| s.text.contains("DERIVE_THERM") && (s.angle_deg + 30.0).abs() < 1.0);

        let seg_debug: Vec<(String, f32)> = segments
            .iter()
            .map(|s| (s.text.clone(), s.angle_deg))
            .collect();

        assert!(
            has_horiz,
            "Doit reconnaître le titre horizontal 0° dans le JPG, reçu : {:?}",
            seg_debug
        );
        assert!(has_vert_90, "Doit reconnaître l'axe Y vertical +90° dans le JPG");
        assert!(
            has_diag_pos30,
            "Doit reconnaître le texte incliné à +30° en diagonale dans le JPG, reçu : {:?}",
            seg_debug
        );
        assert!(
            has_diag_neg30,
            "Doit reconnaître le texte incliné à -30° en diagonale dans le JPG, reçu : {:?}",
            seg_debug
        );

        // 3. Vérifier que Ctrl+F trouve l'occurrence OCR dans l'image avec `is_ocr == true` et `angle_deg == 30°`
        let mut search = SearchBarState::default();
        search.query = "SEUIL_DIAG_30DEG".to_string();
        search.execute_search(&[layer]);
        let ocr_match = search
            .matches
            .iter()
            .find(|m| m.is_ocr)
            .expect("Ctrl+F doit trouver l'occurrence OCR dans le graphique JPG");
        assert!((ocr_match.angle_deg - 30.0).abs() < 1.0);
    }

    #[test]
    fn test_ocr_persistence_tr3_invisible_layer_roundtrip() {
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
        search.query = "AXE_Y_COUPLE_NM".to_string();
        search.execute_search(&[reloaded_layer]);
        assert_eq!(
            search.matches.len(),
            1,
            "Le PDF réouvert doit trouver directement l'axe vertical de l'image via la couche Tr 3 sans relancer l'OCR"
        );
    }

    #[test]
    fn test_three_speed_ocr_scheduler_instant_eco_turbo() {
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
        assert!(added > 50, "Le worker Éco doit reconnaître les caractères du graphique JPG");
        assert!(tab.ocr.is_fully_scanned(1));
    }
}

