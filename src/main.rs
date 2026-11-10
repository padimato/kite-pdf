mod annotations;
mod app;
mod memory_manager;
mod ocr_engine;
mod pdf_engine;
mod recent_files;
mod tabs;
mod text_search;
mod viewport;
mod worker;

use app::{AutomationConfig, InitialSidebarMode, SuperPdfApp};
use std::path::PathBuf;
use tabs::ActiveTool;
use text_search::PdfTopDownRect;

fn parse_args() -> AutomationConfig {
    let mut cfg = AutomationConfig::default();

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
            "--screenshot-delay-ms" => {
                if let Some(ms) = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                    cfg.screenshot_delay_ms = ms;
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
            "--screenshot-visible" => {
                cfg.screenshot_visible = true;
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
            "--search" => {
                if let Some(sq) = args.get(i + 1) {
                    cfg.initial_search_query = Some(sq.clone());
                    i += 1;
                }
            }
            "--match-index" => {
                if let Some(m) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    cfg.initial_match_index = Some(m);
                    i += 1;
                }
            }
            "--select-rect" => {
                if let Some(r_str) = args.get(i + 1) {
                    let parts: Vec<f32> = r_str
                        .split(',')
                        .filter_map(|p| p.trim().parse::<f32>().ok())
                        .collect();
                    if parts.len() == 4 {
                        cfg.initial_select_rect = Some(PdfTopDownRect {
                            x_min: parts[0],
                            y_min: parts[1],
                            x_max: parts[2],
                            y_max: parts[3],
                            angle_deg: 0.0,
                        });
                    }
                    i += 1;
                }
            }
            "--select-stream" => {
                if let Some(s_str) = args.get(i + 1) {
                    let parts: Vec<usize> = s_str
                        .split(',')
                        .filter_map(|p| p.trim().parse::<usize>().ok())
                        .collect();
                    if parts.len() == 2 {
                        cfg.initial_select_stream = Some((parts[0], parts[1]));
                    }
                    i += 1;
                }
            }
            "--highlight-stream" => {
                if let Some(h_str) = args.get(i + 1) {
                    let parts: Vec<usize> = h_str
                        .split(',')
                        .filter_map(|p| p.trim().parse::<usize>().ok())
                        .collect();
                    if parts.len() == 2 {
                        cfg.initial_highlight_stream = Some((parts[0], parts[1]));
                    }
                    i += 1;
                }
            }
            "--extra-pdf" => {
                if let Some(ep) = args.get(i + 1) {
                    cfg.extra_pdfs.push(PathBuf::from(ep));
                    i += 1;
                }
            }
            "--ocr-turbo" => {
                cfg.ocr_turbo = true;
            }
            "--save-annotated" => {
                if let Some(out_path) = args.get(i + 1) {
                    cfg.save_annotated_output = Some(PathBuf::from(out_path));
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
                cfg.initial_pdf = Some(PathBuf::from(arg));
            }
            _ => {}
        }
        i += 1;
    }

    cfg
}

/// Écrit toute panique dans `super_pdf_crash.log` (dossier temporaire) : une fenêtre qui disparaît laisse ainsi une trace.
fn install_crash_log() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("{info}\n{backtrace}\n");
        let log_path = std::env::temp_dir().join("super_pdf_crash.log");
        let _ = std::fs::write(log_path, report);
        default_hook(info);
    }));
}

fn main() -> eframe::Result<()> {
    install_crash_log();
    let cfg = parse_args();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Super Lecteur PDF")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([640.0, 400.0]);
    if cfg.screenshot_output.is_some() && !cfg.screenshot_visible {
        // Capture automatisée : fenêtre hors écran, sans focus ni bouton dans la barre des tâches,
        // pour ne pas déranger l'utilisateur qui travaille sur la machine.
        viewport = viewport
            .with_active(false)
            .with_taskbar(false)
            .with_position([-20000.0, -20000.0]);
    }

    let native_options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "Super Lecteur PDF",
        native_options,
        Box::new(move |cc| Ok(Box::new(SuperPdfApp::new(cc, cfg)))),
    )
}
