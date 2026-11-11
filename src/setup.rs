//! Installation des dépendances depuis l'application : écran de premier lancement (PDFium manquant),
//! téléchargement de l'OCR en tâche de fond et avertissement « version non testée ».

use crate::deps::{self, Dependency, Installed, UntestedVersion};
use egui::{Color32, Context, CornerRadius, Margin, RichText, Stroke};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum InstallStatus {
    Waiting,
    Downloading { received: u64, total: Option<u64> },
    Installed,
    Untested(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct InstallItem {
    pub dependency: Dependency,
    pub status: InstallStatus,
}

/// Téléchargement de dépendances dans un thread, observable depuis l'interface.
#[derive(Clone, Default)]
pub struct InstallJob {
    items: Arc<Mutex<Vec<InstallItem>>>,
    finished: Arc<Mutex<bool>>,
}

impl InstallJob {
    /// Lance l'installation de `dependencies` dans le dossier de l'utilisateur.
    pub fn start(dependencies: Vec<Dependency>, ctx: Context) -> Self {
        let job = Self::default();
        *job.items.lock().unwrap() = dependencies
            .into_iter()
            .map(|dependency| InstallItem { dependency, status: InstallStatus::Waiting })
            .collect();
        let worker = job.clone();
        std::thread::Builder::new()
            .name("superpdf-deps".into())
            .spawn(move || worker.run(ctx))
            .expect("thread de téléchargement");
        job
    }

    fn run(&self, ctx: Context) {
        let root = deps::user_deps_dir();
        let count = self.items.lock().unwrap().len();
        for index in 0..count {
            let dependency = self.items.lock().unwrap()[index].dependency.clone();
            let items = self.items.clone();
            let repaint = ctx.clone();
            let outcome = deps::install(&dependency, &root, &mut |received, total| {
                items.lock().unwrap()[index].status = InstallStatus::Downloading { received, total };
                repaint.request_repaint();
            });
            self.items.lock().unwrap()[index].status = match outcome {
                Ok(Installed::TestedVersion) => InstallStatus::Installed,
                Ok(Installed::UntestedVersion(u)) => InstallStatus::Untested(u.warning()),
                Err(e) => InstallStatus::Failed(format!("{e:#}")),
            };
            ctx.request_repaint();
        }
        *self.finished.lock().unwrap() = true;
        ctx.request_repaint();
    }

    pub fn items(&self) -> Vec<InstallItem> {
        self.items.lock().unwrap().clone()
    }

    pub fn is_finished(&self) -> bool {
        *self.finished.lock().unwrap()
    }

    /// Toutes les dépendances obligatoires sont en place (dans la version testée ou non).
    pub fn required_ready(&self) -> bool {
        self.items().iter().all(|i| {
            !i.dependency.required
                || matches!(i.status, InstallStatus::Installed | InstallStatus::Untested(_))
        })
    }

    pub fn failures(&self) -> Vec<InstallItem> {
        self.items()
            .into_iter()
            .filter(|i| matches!(i.status, InstallStatus::Failed(_)))
            .collect()
    }
}

/// Écran affiché au premier lancement, tant que PDFium n'est pas installé.
pub struct SetupScreen {
    job: InstallJob,
    /// L'utilisateur a choisi de continuer malgré l'échec de composants facultatifs.
    pub proceed: bool,
}

impl SetupScreen {
    pub fn new(ctx: &Context) -> Self {
        Self { job: InstallJob::start(deps::missing(), ctx.clone()), proceed: false }
    }

    /// L'application peut démarrer.
    pub fn ready(&self) -> bool {
        self.job.is_finished()
            && self.job.required_ready()
            && (self.job.failures().is_empty() || self.proceed)
    }

    pub fn show(&mut self, ctx: &Context) {
        let muted = Color32::from_rgb(140, 150, 172);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(18, 20, 26)))
            .show(ctx, |ui| {
                let width = 520.0_f32.min(ui.available_width() - 32.0);
                ui.add_space((ui.available_height() * 0.18).max(16.0));
                ui.vertical_centered(|ui| {
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(24, 28, 39))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(60, 80, 130)))
                        .corner_radius(CornerRadius::same(14))
                        .inner_margin(Margin::same(20))
                        .show(ui, |ui| {
                            ui.set_width(width);
                            ui.label(RichText::new("Installation des composants").size(18.0).color(Color32::WHITE));
                            ui.label(
                                RichText::new(
                                    "Premier lancement : téléchargement du moteur PDF et de l'OCR \
                                     depuis leur source officielle.",
                                )
                                .size(12.0)
                                .color(muted),
                            );
                            ui.add_space(12.0);
                            for item in self.job.items() {
                                show_item(ui, &item, muted);
                                ui.add_space(4.0);
                            }
                            ui.add_space(8.0);
                            if self.job.is_finished() {
                                if !self.job.required_ready() {
                                    ui.label(
                                        RichText::new(
                                            "Le moteur PDF n'a pas pu être installé : vérifiez la \
                                             connexion internet.",
                                        )
                                        .color(Color32::from_rgb(255, 130, 120)),
                                    );
                                    if ui.button("Réessayer").clicked() {
                                        self.job = InstallJob::start(deps::missing(), ctx.clone());
                                    }
                                } else if !self.job.failures().is_empty() {
                                    ui.label(
                                        RichText::new("L'OCR n'a pas pu être installé ; tout le reste fonctionne.")
                                            .color(Color32::from_rgb(250, 200, 110)),
                                    );
                                    ui.horizontal(|ui| {
                                        if ui.button("Réessayer").clicked() {
                                            self.job = InstallJob::start(deps::missing(), ctx.clone());
                                        }
                                        if ui.button("Continuer sans OCR").clicked() {
                                            self.proceed = true;
                                        }
                                    });
                                }
                            }
                        });
                });
            });
    }
}

fn show_item(ui: &mut egui::Ui, item: &InstallItem, muted: Color32) {
    ui.horizontal(|ui| {
        let (mark, color) = match &item.status {
            InstallStatus::Waiting => ("·", muted),
            InstallStatus::Downloading { .. } => ("", Color32::from_rgb(120, 180, 255)),
            InstallStatus::Installed => ("✔", Color32::from_rgb(120, 210, 150)),
            InstallStatus::Untested(_) => ("⚠", Color32::from_rgb(250, 200, 110)),
            InstallStatus::Failed(_) => ("✖", Color32::from_rgb(255, 130, 120)),
        };
        if matches!(item.status, InstallStatus::Downloading { .. }) {
            ui.add(egui::Spinner::new().size(12.0).color(color));
        } else {
            ui.label(RichText::new(mark).color(color));
        }
        ui.label(RichText::new(&item.dependency.name).color(Color32::WHITE));
        ui.label(RichText::new(&item.dependency.version).size(11.0).color(muted));
        if let InstallStatus::Downloading { received, total } = item.status {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = match total {
                    Some(t) if t > 0 => format!("{} %", received * 100 / t),
                    _ => format!("{:.1} Mo", received as f32 / 1_048_576.0),
                };
                ui.label(RichText::new(text).size(11.0).color(muted));
            });
        }
    });
    match &item.status {
        InstallStatus::Untested(warning) | InstallStatus::Failed(warning) => {
            ui.label(RichText::new(warning).size(11.0).color(muted));
        }
        _ => {}
    }
}

/// Empreintes des versions non testées que l'utilisateur ne veut plus voir signalées.
fn ignored_versions_file() -> PathBuf {
    crate::settings::app_data_dir()
        .join("ignored_deps.txt")
}

pub fn ignored_versions() -> Vec<String> {
    std::fs::read_to_string(ignored_versions_file())
        .map(|t| t.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

pub fn ignore_version(found_sha256: &str) {
    let path = ignored_versions_file();
    let mut list = ignored_versions();
    list.push(found_sha256.to_lowercase());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, list.join("\n"));
}

/// Vérifie en tâche de fond les versions installées (lecture et hachage d'environ 30 Mo).
pub fn check_versions_in_background(ctx: Context) -> Arc<Mutex<Option<Vec<UntestedVersion>>>> {
    let result = Arc::new(Mutex::new(None));
    let slot = result.clone();
    let _ = std::thread::Builder::new()
        .name("superpdf-deps-check".into())
        .spawn(move || {
            let ignored = ignored_versions();
            let untested: Vec<UntestedVersion> = deps::untested_versions()
                .into_iter()
                .filter(|u| !ignored.contains(&u.found_sha256.to_lowercase()))
                .collect();
            *slot.lock().unwrap() = Some(untested);
            ctx.request_repaint();
        });
    result
}
