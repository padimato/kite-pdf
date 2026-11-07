mod app;
mod pdf_engine;
mod viewport;

use app::{AutomationConfig, SuperPdfApp};
use pdf_engine::ensure_sample_pdf;
use std::path::PathBuf;

fn parse_args() -> AutomationConfig {
    let mut cfg = AutomationConfig {
        initial_pdf: PathBuf::from("test_pdfs/demo_jalon1.pdf"),
        ..Default::default()
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--screenshot" => {
                if let Some(p) = args.get(i + 1) {
                    cfg.screenshot_output = Some(PathBuf::from(p));
                    i += 1;
                }
            }
            "--zoom" => {
                if let Some(z) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                    cfg.initial_zoom = Some(z);
                    i += 1;
                }
            }
            "--page" => {
                if let Some(p) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    cfg.initial_page = Some(p);
                    i += 1;
                }
            }
            "--scroll-x" => {
                if let Some(sx) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                    cfg.initial_scroll_x = Some(sx);
                    i += 1;
                }
            }
            "--scroll-y" => {
                if let Some(sy) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                    cfg.initial_scroll_y = Some(sy);
                    i += 1;
                }
            }
            arg if !arg.starts_with("--") => {
                cfg.initial_pdf = PathBuf::from(arg);
            }
            _ => {}
        }
        i += 1;
    }

    cfg
}

fn main() -> eframe::Result<()> {
    let cfg = parse_args();

    if cfg.initial_pdf == PathBuf::from("test_pdfs/demo_jalon1.pdf") {
        ensure_sample_pdf(&cfg.initial_pdf)
            .expect("Impossible de générer le PDF de démonstration test_pdfs/demo_jalon1.pdf");
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Super Lecteur PDF — Jalon 1 (Rust + PDFium + Trackpad Windows Precision)")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Super Lecteur PDF",
        native_options,
        Box::new(move |cc| Ok(Box::new(SuperPdfApp::new(cc, cfg)))),
    )
}
