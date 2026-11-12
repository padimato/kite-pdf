//! Page Réglages (`Ctrl+,`) : préférences de lecture, OCR des images, historique, mises à jour.
//! Chaque changement s'applique tout de suite ; l'application l'enregistre.

use crate::settings::{ImageOcr, OpenZoom, Preferences};
use egui::{Color32, CornerRadius, Margin, RichText, Stroke};

const MUTED: Color32 = Color32::from_rgb(150, 160, 182);
const SECTION: Color32 = Color32::from_rgb(130, 170, 255);

/// État de la lecture des images, affiché dans la page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrStatus {
    Installed,
    Downloading,
    Missing,
}

pub struct PageInfo {
    pub ocr: OcrStatus,
    pub history_len: usize,
    /// « Effacer l'historique » cliqué une première fois
    pub confirm_clear_history: bool,
    /// Résultat de « Rechercher une mise à jour »
    pub update_check: crate::updates::ManualCheck,
}

/// Demandes faites depuis la page, exécutées par l'application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Close,
    AskClearHistory,
    ConfirmClearHistory,
    CancelClearHistory,
    InstallOcr,
    MakeDefaultReader,
    ShowShortcuts,
    ShowWelcome,
    OpenProjectPage,
    CheckUpdatesNow,
    OpenNewRelease,
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).size(14.0).strong().color(SECTION));
    ui.add_space(2.0);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(MUTED));
}

/// Affiche la page. `prefs` est modifié directement ; renvoie les autres demandes.
pub fn show(ctx: &egui::Context, prefs: &mut Preferences, info: &PageInfo) -> Vec<Action> {
    let mut actions = Vec::new();
    let screen = ctx.screen_rect();
    let width = 560.0_f32.min(screen.width() - 32.0);
    let frame = egui::Frame::NONE
        .fill(Color32::from_rgb(24, 28, 39))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(70, 100, 170)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(22));
    let modal = egui::Modal::new(egui::Id::new("kite_settings")).frame(frame).show(ctx, |ui| {
        ui.set_width(width - 44.0);
        ui.set_max_width(width - 44.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Réglages").size(19.0).strong().color(Color32::WHITE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Fermer").clicked() {
                    actions.push(Action::Close);
                }
            });
        });
        let max_height = (screen.height() - 140.0).max(240.0);
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .min_scrolled_height(max_height.min(620.0))
            .show(ui, |ui| {
                ui.set_width(width - 56.0);

                section(ui, "Ouverture des documents");
                ui.label(RichText::new("Zoom d'un PDF ouvert pour la première fois").size(12.5).color(Color32::WHITE));
                ui.horizontal_wrapped(|ui| {
                    ui.radio_value(&mut prefs.open_zoom, OpenZoom::Actual, "100 %");
                    ui.radio_value(&mut prefs.open_zoom, OpenZoom::FitWidth, "Toute la largeur");
                    ui.radio_value(&mut prefs.open_zoom, OpenZoom::FitPage, "Page entière");
                });
                ui.add_space(4.0);
                ui.add_enabled(
                    prefs.keep_history,
                    egui::Checkbox::new(&mut prefs.resume_reading, "Reprendre la lecture là où je l'avais laissée"),
                );
                note(ui, if prefs.keep_history {
                    "Un PDF déjà lu se rouvre à la même page et au même zoom."
                } else {
                    "Nécessite l'historique des PDF récents (plus bas)."
                });

                section(ui, "Texte dans les images");
                note(ui, "Kite lit le texte des images et des pages scannées pour que la recherche (Ctrl+F) le trouve. Ce travail se fait sur l'ordinateur, sans rien envoyer.");
                ui.add_space(4.0);
                ui.radio_value(&mut prefs.image_ocr, ImageOcr::Auto, "Lire les images en arrière-plan (recommandé)");
                ui.radio_value(&mut prefs.image_ocr, ImageOcr::OnSearch, "Seulement quand je cherche un mot");
                ui.radio_value(&mut prefs.image_ocr, ImageOcr::Never, "Jamais");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| match info.ocr {
                    OcrStatus::Installed => note(ui, "Module de lecture des images : installé."),
                    OcrStatus::Downloading => {
                        ui.add(egui::Spinner::new().size(12.0));
                        note(ui, "Module de lecture des images : téléchargement en cours…");
                    }
                    OcrStatus::Missing => {
                        note(ui, "Module de lecture des images : pas encore installé.");
                        if ui.small_button("Installer (environ 30 Mo)").clicked() {
                            actions.push(Action::InstallOcr);
                        }
                    }
                });

                section(ui, "Historique et confidentialité");
                ui.checkbox(&mut prefs.keep_history, "Garder la liste des PDF ouverts récemment");
                note(ui, "Elle sert à les retrouver avec Ctrl+T. Elle reste sur cet ordinateur.");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    if info.confirm_clear_history {
                        note(ui, &format!("Effacer les {} documents de l'historique ?", info.history_len));
                        if ui.small_button("Oui, effacer").clicked() {
                            actions.push(Action::ConfirmClearHistory);
                        }
                        if ui.small_button("Annuler").clicked() {
                            actions.push(Action::CancelClearHistory);
                        }
                    } else {
                        let clear = egui::Button::new("Effacer l'historique").small();
                        if ui.add_enabled(info.history_len > 0, clear).clicked() {
                            actions.push(Action::AskClearHistory);
                        }
                        if info.history_len == 0 {
                            note(ui, "L'historique est vide.");
                        }
                    }
                });

                section(ui, "Mises à jour");
                note(ui, &format!("Version installée : {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    use crate::updates::ManualCheck;
                    let checking = info.update_check == ManualCheck::Checking;
                    if ui.add_enabled(!checking, egui::Button::new("Rechercher une mise à jour")).clicked() {
                        actions.push(Action::CheckUpdatesNow);
                    }
                    match &info.update_check {
                        ManualCheck::Idle => {}
                        ManualCheck::Checking => {
                            ui.add(egui::Spinner::new().size(12.0));
                            note(ui, "Recherche…");
                        }
                        ManualCheck::UpToDate => {
                            ui.label(RichText::new("✔ Kite PDF est à jour.").size(12.0).color(Color32::from_rgb(120, 210, 150)));
                        }
                        ManualCheck::Available(release) => {
                            ui.label(
                                RichText::new(format!("Kite PDF {} est disponible.", release.version))
                                    .size(12.0)
                                    .color(Color32::WHITE),
                            );
                            if ui.small_button("Voir la nouvelle version").clicked() {
                                actions.push(Action::OpenNewRelease);
                            }
                        }
                        ManualCheck::Failed(reason) => {
                            ui.label(RichText::new(reason).size(12.0).color(Color32::from_rgb(250, 200, 110)));
                        }
                    }
                });
                ui.add_space(6.0);
                ui.checkbox(&mut prefs.check_updates, "Vérifier automatiquement (une fois par jour)");
                note(ui, "Kite consulte la page des versions sur GitHub et vous prévient. Rien n'est installé sans vous.");

                section(ui, "Windows");
                if ui.button("Faire de Kite PDF le lecteur PDF par défaut…").clicked() {
                    actions.push(Action::MakeDefaultReader);
                }
                note(ui, "Ouvre les Paramètres de Windows : choisissez Kite PDF pour les fichiers .pdf.");

                section(ui, "Aide");
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Raccourcis clavier (F1)").clicked() {
                        actions.push(Action::ShowShortcuts);
                    }
                    if ui.button("Revoir l'accueil").clicked() {
                        actions.push(Action::ShowWelcome);
                    }
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    note(ui, &format!("Kite PDF {} · logiciel libre (licence MIT) ·", env!("CARGO_PKG_VERSION")));
                    if ui.link(RichText::new("page du projet").size(11.5)).clicked() {
                        actions.push(Action::OpenProjectPage);
                    }
                });
                ui.add_space(4.0);
            });
    });
    if modal.should_close() && !actions.contains(&Action::Close) {
        actions.push(Action::Close);
    }
    actions
}
