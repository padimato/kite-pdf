mod app;
mod memory_manager;
mod pdf_engine;
mod tabs;
mod viewport;

use app::{AutomationConfig, InitialSidebarMode, SuperPdfApp};
use pdf_engine::ensure_sample_pdf;
use std::path::PathBuf;
use tabs::ActiveTool;

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
            "--demo-tabs" => {
                if let Some(c) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    cfg.demo_tabs_count = Some(c);
                    i += 1;
                }
            }
            "--active-tab" => {
                if let Some(t) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    cfg.active_tab_index = Some(t);
                    i += 1;
                }
            }
            "--sidebar" => {
                if let Some(mode) = args.get(i + 1) {
                    cfg.sidebar_mode = match mode.as_str() {
                        "autohide-hidden" | "hidden" => InitialSidebarMode::AutoHideHidden,
                        "autohide-hover" | "hover" => InitialSidebarMode::AutoHideHover,
                        _ => InitialSidebarMode::Pinned,
                    };
                    i += 1;
                }
            }
            "--palette" => {
                if let Some(q) = args.get(i + 1) {
                    cfg.open_palette_query = Some(q.clone());
                    i += 1;
                }
            }
            "--tool" => {
                if let Some(t) = args.get(i + 1) {
                    cfg.initial_tool = match t.as_str() {
                        "highlight" => Some(ActiveTool::Highlight),
                        "freetext" => Some(ActiveTool::FreeText),
                        "ink" => Some(ActiveTool::Ink),
                        "shape" => Some(ActiveTool::Shape),
                        "note" => Some(ActiveTool::StickyNote),
                        "measure" => Some(ActiveTool::Measure),
                        "pan" => Some(ActiveTool::Pan),
                        _ => Some(ActiveTool::Select),
                    };
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

    if cfg.demo_tabs_count.is_none() && cfg.initial_pdf == PathBuf::from("test_pdfs/demo_jalon1.pdf")
    {
        ensure_sample_pdf(&cfg.initial_pdf)
            .expect("Impossible de générer le PDF de démonstration test_pdfs/demo_jalon1.pdf");
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Super Lecteur PDF — Haute Performance & Virtualisation RAM (15+ Onglets)")
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
