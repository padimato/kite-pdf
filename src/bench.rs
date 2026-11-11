//! `--bench <fichier.pdf>` : mesure le temps de chaque étape du lecteur sur un document réel, pour
//! savoir où part le temps avant d'optimiser. À lancer sur une compilation `--release`.

use crate::ocr_engine::{HybridOcrEngine, OcrRecognizer};
use crate::pdf_engine::{PdfEngine, TileRegion};
use crate::viewport::BASE_DPI_SCALE;
use std::path::Path;
use std::time::{Duration, Instant};

/// Échelle d'affichage Windows supposée (150 %, courante sur les portables).
const PIXELS_PER_POINT: f32 = 1.5;
/// Pages mesurées au plus pour chaque étape (réparties sur tout le document).
const SAMPLE_PAGES: usize = 12;
/// Pages passées à l'OCR au plus (étape lente).
const OCR_SAMPLE_PAGES: usize = 3;
const PARALLEL_THREADS: usize = 4;

struct Stats(Vec<Duration>);

impl Stats {
    fn line(&self, label: &str) {
        if self.0.is_empty() {
            println!("  {label:<44} -");
            return;
        }
        let mut sorted = self.0.clone();
        sorted.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let avg = sorted.iter().map(|d| ms(*d)).sum::<f64>() / sorted.len() as f64;
        println!(
            "  {label:<44} moy {:>7.1} ms   max {:>7.1} ms   ({} mesures)",
            avg,
            ms(*sorted.last().unwrap()),
            sorted.len()
        );
    }
}

fn once(label: &str, d: Duration) {
    println!("  {label:<44} {:>11.1} ms", d.as_secs_f64() * 1000.0);
}

fn sample(page_count: usize, max: usize) -> Vec<usize> {
    if page_count <= max {
        return (0..page_count).collect();
    }
    (0..max).map(|i| i * (page_count - 1) / (max - 1)).collect()
}

pub fn run(path: &Path) -> i32 {
    println!("Mesure : {}", path.display());
    if cfg!(debug_assertions) {
        println!("  (compilation debug : chiffres pessimistes, utilisez `cargo run --release -- --bench`)");
    }

    let t = Instant::now();
    let engine = PdfEngine::instance();
    once("Chargement de PDFium", t.elapsed());

    let t = Instant::now();
    let doc = match engine.load_document(path) {
        Ok(doc) => doc,
        Err(e) => {
            println!("Ouverture impossible : {e:#}");
            return 1;
        }
    };
    let page_count = doc.page_dimensions.len();
    once(&format!("Ouverture complète ({page_count} pages)"), t.elapsed());

    let t = Instant::now();
    let light = engine.open_lightweight(path).expect("réouverture");
    once("Ouverture légère (threads d'arrière-plan)", t.elapsed());
    drop(light);

    println!("\nRendu d'une page entière (écran à {:.0} %) :", PIXELS_PER_POINT * 100.0);
    let pages = sample(page_count, SAMPLE_PAGES);
    for zoom in [1.0_f32, 2.0] {
        let mut times = Vec::new();
        for &p in &pages {
            let dim = doc.page_dimensions[p];
            let scale = BASE_DPI_SCALE * zoom * PIXELS_PER_POINT;
            let (w, h) = ((dim.width_pt * scale) as i32, (dim.height_pt * scale) as i32);
            let t = Instant::now();
            if doc.render_page(p, w, h).is_ok() {
                times.push(t.elapsed());
            }
        }
        let dim = doc.page_dimensions[0];
        let scale = BASE_DPI_SCALE * zoom * PIXELS_PER_POINT;
        Stats(times).line(&format!(
            "zoom {:>3.0} % ({}x{} px)",
            zoom * 100.0,
            (dim.width_pt * scale) as i32,
            (dim.height_pt * scale) as i32
        ));
    }

    let mut tiles = Vec::new();
    let scale = BASE_DPI_SCALE * 4.0 * PIXELS_PER_POINT;
    for &p in pages.iter().take(4) {
        for (x, y) in [(0, 0), (512, 512), (1024, 1024)] {
            let region = TileRegion { x, y, width: 512, height: 512 };
            let t = Instant::now();
            if doc.render_page_tile(p, scale, region).is_ok() {
                tiles.push(t.elapsed());
            }
        }
    }
    Stats(tiles).line("tuile 512x512 à zoom 400 %");

    println!("\nTexte et images :");
    let mut text = Vec::new();
    let mut chars = 0;
    for &p in &pages {
        let t = Instant::now();
        chars += doc.extract_single_page_text_layer(p).chars.len();
        text.push(t.elapsed());
    }
    Stats(text).line(&format!("extraction du texte ({chars} car.)"));

    let mut images = Vec::new();
    let mut image_pages = Vec::new();
    for &p in &pages {
        let t = Instant::now();
        let found = doc.extract_page_embedded_images(p);
        images.push(t.elapsed());
        if !found.is_empty() {
            image_pages.push(p);
        }
    }
    Stats(images).line(&format!("extraction des images ({} pages en ont)", image_pages.len()));

    println!("\nOCR :");
    let t = Instant::now();
    match OcrRecognizer::shared() {
        Ok(recognizer) => {
            once("Chargement des modèles", t.elapsed());
            let mut ocr = Vec::new();
            let mut lines = 0;
            // Toutes les images de la page, même celles dont le texte est déjà dans le fichier :
            // c'est le coût d'un premier passage.
            let empty = crate::text_search::PageTextLayer::new(0, Vec::new());
            let mut image_count = 0;
            for &p in image_pages.iter().take(OCR_SAMPLE_PAGES) {
                let images = doc.extract_page_embedded_images(p);
                image_count += images.len();
                let t = Instant::now();
                lines += HybridOcrEngine::recognize_images(&recognizer, images, &empty).len();
                ocr.push(t.elapsed());
            }
            Stats(ocr).line(&format!(
                "reconnaissance par page ({image_count} images, {lines} lignes)"
            ));
        }
        Err(reason) => println!("  indisponible : {reason}"),
    }

    println!("
OCR approfondi (page entière) :");
    let mut full = Vec::new();
    let mut sample_lines: Vec<String> = Vec::new();
    let mut lines = 0;
    let first = std::time::Instant::now();
    for &p in pages.iter().skip(1).take(OCR_SAMPLE_PAGES) {
        let t = Instant::now();
        let native = doc.extract_single_page_text_layer(p);
        match crate::ocr_engine::recognize_full_page(&doc, p, &native) {
            Ok(segments) => {
                full.push(t.elapsed());
                lines += segments.len();
                if sample_lines.is_empty() {
                    sample_lines = segments.iter().take(8).map(|s| format!("p.{} : {}", p + 1, s.text)).collect();
                }
            }
            Err(reason) => {
                println!("  indisponible : {reason}");
                break;
            }
        }
        if full.len() == 1 {
            once("première page (chargement des modèles compris)", first.elapsed());
        }
    }
    Stats(full).line(&format!("par page ({lines} lignes)"));
    for line in sample_lines {
        println!("    {line}");
    }
    let all_dims = doc.page_dimensions.clone();
    let dims = &all_dims;
    drop(doc);

    println!("\nRendu simultané de {} pages à zoom 100 % :", pages.len());
    let render_all = |threads: usize| -> Duration {
        let chunks: Vec<Vec<usize>> = (0..threads)
            .map(|i| pages.iter().copied().skip(i).step_by(threads).collect())
            .collect();
        let start = Instant::now();
        std::thread::scope(|s| {
            for chunk in chunks {
                s.spawn(move || {
                    let doc = engine.open_lightweight(path).expect("ouverture");
                    for p in chunk {
                        let scale = BASE_DPI_SCALE * PIXELS_PER_POINT;
                        let _ = doc.render_page(
                            p,
                            (dims[p].width_pt * scale) as i32,
                            (dims[p].height_pt * scale) as i32,
                        );
                    }
                    let _lock = PdfEngine::lock_guard();
                    drop(doc);
                });
            }
        });
        start.elapsed()
    };
    let sequential = render_all(1);
    let parallel = render_all(PARALLEL_THREADS);
    once("1 thread", sequential);
    once(&format!("{PARALLEL_THREADS} threads"), parallel);
    println!(
        "  gain du parallélisme : x{:.2} (x1 = PDFium ne rend qu'une page à la fois)",
        sequential.as_secs_f64() / parallel.as_secs_f64().max(1e-9)
    );
    0
}
