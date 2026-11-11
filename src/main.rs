mod annotations;
mod bench;
mod deps;
mod settings;
mod setup;
mod app;
mod memory_manager;
mod ocr_engine;
mod pdf_engine;
mod print;
mod recent_files;
mod tabs;
mod text_search;
mod viewport;
mod worker;

use app::{AutomationConfig, InitialSidebarMode, SuperPdfApp};
use std::path::PathBuf;
use tabs::ActiveTool;
use text_search::PdfTopDownRect;

/// Variable d'environnement qui active les options de test (`--screenshot`, `--page`...) dans une
/// compilation optimisée. Une compilation de développement les accepte toujours.
const QA_ENV: &str = "SUPERPDF_QA";

fn parse_args() -> AutomationConfig {
    let mut cfg = AutomationConfig::default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // Version distribuée : seul le chemin du PDF à ouvrir est lu (« Ouvrir avec », double-clic).
    if !cfg!(debug_assertions) && std::env::var_os(QA_ENV).is_none() {
        cfg.initial_pdf = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from);
        return cfg;
    }
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
            "--password" => {
                if let Some(pw) = args.get(i + 1) {
                    cfg.initial_password = Some(pw.clone());
                    i += 1;
                }
            }
            "--print-to-pdf" => {
                if let Some(out) = args.get(i + 1) {
                    cfg.print_to_pdf = Some(PathBuf::from(out));
                    i += 1;
                }
            }
            "--form-fill" => {
                if let Some(pair) = args.get(i + 1) {
                    if let Some((name, value)) = pair.split_once('=') {
                        cfg.form_fills.push((name.to_string(), value.to_string()));
                    }
                    i += 1;
                }
            }
            "--sidebar-width" => {
                if let Some(w) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                    cfg.sidebar_width = Some(w);
                    i += 1;
                }
            }
            "--outline" => {
                cfg.outline_open = true;
            }
            "--outline-entry" => {
                if let Some(n) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    cfg.outline_open = true;
                    cfg.outline_entry = Some(n);
                    i += 1;
                }
            }
            "--deep-ocr-dialog" => {
                cfg.deep_ocr_dialog = true;
            }
            "--deep-ocr-all" => {
                cfg.deep_ocr_all = true;
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

/// Écran d'installation tant que PDFium manque, puis l'application elle-même.
enum Root {
    Setup { screen: setup::SetupScreen, cfg: Option<AutomationConfig> },
    Ready(Box<SuperPdfApp>),
}

impl eframe::App for Root {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        match self {
            Root::Setup { screen, cfg } => {
                screen.show(ctx);
                if screen.ready() {
                    let cfg = cfg.take().unwrap_or_default();
                    *self = Root::Ready(Box::new(SuperPdfApp::new(ctx, cfg)));
                    ctx.request_repaint();
                }
            }
            Root::Ready(app) => app.update(ctx, frame),
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Root::Ready(app) = self {
            app.save_session();
        }
    }
}

/// Instant du lancement du processus (mesure du temps jusqu'à la première page affichée).
pub static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() -> eframe::Result<()> {
    PROCESS_START.get_or_init(std::time::Instant::now);
    install_crash_log();

    // `--fetch-deps [dossier]` : installe PDFium et l'OCR (dossier courant par défaut) puis quitte.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(pos) = args.iter().position(|a| a == "--fetch-deps") {
        let root = args
            .get(pos + 1)
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        std::process::exit(deps::fetch_missing_cli(&root));
    }

    if let Some(pos) = args.iter().position(|a| a == "--bench") {
        let Some(pdf) = args.get(pos + 1) else {
            eprintln!("Usage : super_pdf_reader --bench <fichier.pdf>");
            std::process::exit(2);
        };
        std::process::exit(bench::run(std::path::Path::new(pdf)));
    }

    let cfg = parse_args();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Super Lecteur PDF")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([640.0, 400.0]);
    // Fenêtre rouverte à la même place et à la même taille (sauf captures automatisées).
    if settings::persistence_enabled(cfg.screenshot_output.is_some()) {
        let saved = settings::Settings::load();
        if saved.window_is_plausible() {
            if let Some([x, y, w, h]) = saved.window {
                viewport = viewport.with_position([x, y]).with_inner_size([w, h]);
            }
        }
        viewport = viewport.with_maximized(saved.maximized);
    }
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
        Box::new(move |cc| {
            let pdfium_installed = deps::by_file_name("pdfium.dll")
                .and_then(|d| deps::locate(&d))
                .is_some();
            let root = if pdfium_installed {
                Root::Ready(Box::new(SuperPdfApp::new(&cc.egui_ctx, cfg)))
            } else {
                Root::Setup { screen: setup::SetupScreen::new(&cc.egui_ctx), cfg: Some(cfg) }
            };
            Ok(Box::new(root))
        }),
    )
}
