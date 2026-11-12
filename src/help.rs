//! Aide : accueil du premier lancement et aide-mémoire des raccourcis (`F1`).

use egui::{Color32, CornerRadius, Margin, RichText, Stroke, Vec2};

const CARD_FILL: Color32 = Color32::from_rgb(24, 28, 39);
const CARD_STROKE: Color32 = Color32::from_rgb(70, 100, 170);
const MUTED: Color32 = Color32::from_rgb(150, 160, 182);
const KEY_FILL: Color32 = Color32::from_rgb(44, 50, 66);

/// Raccourcis par thème : (touches, action).
pub const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Fichiers",
        &[
            ("Ctrl+O", "Ouvrir un PDF (ou le glisser ici)"),
            ("Ctrl+S", "Enregistrer"),
            ("Ctrl+Maj+S", "Enregistrer une copie"),
            ("Ctrl+P", "Imprimer"),
        ],
    ),
    (
        "Fenêtre et onglets",
        &[
            ("Ctrl+T", "PDF récents, page, zoom ou outil"),
            ("Ctrl+Maj+T", "Rouvrir l'onglet fermé"),
            ("Ctrl+W", "Fermer l'onglet"),
            ("Ctrl+Tab", "Onglet suivant (Maj : précédent)"),
            ("Ctrl+1 … 8", "Aller à l'onglet n°"),
            ("Ctrl+B", "Garder la barre latérale ouverte"),
            ("Ctrl+,", "Réglages"),
            ("F1", "Cette aide"),
        ],
    ),
    (
        "Lecture",
        &[
            ("Ctrl+F", "Chercher un mot"),
            ("Flèches", "Faire défiler"),
            ("Pg préc. / Pg suiv.", "Écran précédent / suivant"),
            ("Début / Fin", "Première / dernière page"),
            ("Ctrl+molette", "Zoomer (ou pincer)"),
            ("Ctrl+9 / Ctrl+0", "Ajuster à la largeur / à la page"),
            ("Espace + glisser", "Déplacer la page"),
        ],
    ),
    (
        "Annotations",
        &[
            ("V", "Sélectionner, copier du texte"),
            ("H", "Surligner"),
            ("T", "Écrire du texte"),
            ("D", "Dessiner"),
            ("R", "Flèche ou rectangle"),
            ("N", "Commentaire"),
            ("M", "Mesurer"),
            ("Ctrl+Z / Ctrl+Y", "Annuler / rétablir"),
            ("Suppr", "Supprimer l'annotation choisie"),
        ],
    ),
];

/// Choix fait sur l'écran d'accueil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeAction {
    OpenFile,
    Start,
    ShowShortcuts,
}

fn key_badge(ui: &mut egui::Ui, keys: &str) {
    egui::Frame::NONE
        .fill(KEY_FILL)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(keys).monospace().size(11.5).color(Color32::WHITE));
        });
}

fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(CARD_FILL)
        .stroke(Stroke::new(1.0_f32, CARD_STROKE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(22))
}

/// Écran d'accueil du premier lancement. Renvoie le choix fait, s'il y en a un.
pub fn show_welcome(ctx: &egui::Context, logo: Option<&egui::TextureHandle>) -> Option<WelcomeAction> {
    let mut action = None;
    let width = 520.0_f32.min(ctx.screen_rect().width() - 32.0);
    let modal = egui::Modal::new(egui::Id::new("kite_welcome")).frame(card_frame()).show(ctx, |ui| {
        ui.set_width(width - 44.0);
        ui.horizontal(|ui| {
            if let Some(logo) = logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(40.0)));
            }
            ui.vertical(|ui| {
                ui.label(RichText::new("Bienvenue dans Kite PDF").size(20.0).strong().color(Color32::WHITE));
                ui.label(RichText::new("Quelques gestes pour bien démarrer").size(12.5).color(MUTED));
            });
        });
        ui.add_space(14.0);
        let tips: [(&str, &str, &str); 5] = [
            ("Ouvrir un PDF", "Ctrl+O", "ou glissez le fichier dans la fenêtre."),
            ("Tout retrouver", "Ctrl+T", "vos PDF récents, une page (« 12 »), un zoom (« 150 % ») ou un outil (« surligner »)."),
            ("Chercher un mot", "Ctrl+F", "y compris dans le texte des images."),
            ("Annoter", "H  T  D  N", "surligner, écrire, dessiner, commenter. Ctrl+S enregistre dans le PDF."),
            ("Barre latérale", "Ctrl+B", "onglets, outils et sommaire, au bord gauche de la fenêtre."),
        ];
        for (title, keys, text) in tips {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(title).size(13.0).strong().color(Color32::WHITE));
                key_badge(ui, keys);
                ui.label(RichText::new(text).size(12.5).color(MUTED));
            });
            ui.add_space(6.0);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.link(RichText::new("Tous les raccourcis (F1)").size(12.0)).clicked() {
                action = Some(WelcomeAction::ShowShortcuts);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let start = egui::Button::new(RichText::new("Commencer").size(13.5).color(Color32::WHITE))
                    .fill(Color32::from_rgb(52, 92, 190))
                    .min_size(Vec2::new(110.0, 32.0));
                if ui.add(start).clicked() {
                    action = Some(WelcomeAction::Start);
                }
                let open = egui::Button::new(RichText::new("Ouvrir un PDF…").size(13.5)).min_size(Vec2::new(0.0, 32.0));
                if ui.add(open).clicked() {
                    action = Some(WelcomeAction::OpenFile);
                }
            });
        });
    });
    if action.is_none() && modal.should_close() {
        action = Some(WelcomeAction::Start);
    }
    action
}

/// Aide-mémoire des raccourcis. Renvoie `false` quand il faut le fermer.
pub fn show_shortcuts(ctx: &egui::Context) -> bool {
    let mut keep_open = true;
    let screen = ctx.screen_rect();
    // Trois colonnes sur un grand écran, deux sur un écran moyen, une sur une fenêtre étroite.
    let columns = if screen.width() >= 1480.0 { 3 } else if screen.width() >= 960.0 { 2 } else { 1 };
    let width = match columns {
        3 => 1400.0,
        2 => 920.0,
        _ => (screen.width() - 32.0).min(480.0),
    };
    let modal = egui::Modal::new(egui::Id::new("kite_shortcuts")).frame(card_frame()).show(ctx, |ui| {
        ui.set_width(width - 44.0);
        ui.set_max_width(width - 44.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Raccourcis clavier").size(18.0).strong().color(Color32::WHITE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Fermer").clicked() {
                    keep_open = false;
                }
            });
        });
        ui.add_space(10.0);
        let max_height = (screen.height() - 160.0).max(200.0);
        // Sans hauteur minimale, la zone se réduirait à la place laissée au premier affichage.
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .min_scrolled_height(max_height.min(600.0))
            .show(ui, |ui| {
            let group = |ui: &mut egui::Ui, (title, entries): &(&str, &[(&str, &str)])| {
                ui.label(RichText::new(*title).size(13.5).strong().color(Color32::from_rgb(130, 170, 255)));
                ui.add_space(2.0);
                egui::Grid::new(("kite_shortcuts_grid", *title)).num_columns(2).spacing([10.0, 5.0]).show(ui, |ui| {
                    for (keys, action) in entries.iter() {
                        key_badge(ui, keys);
                        ui.label(RichText::new(*action).size(12.5).color(MUTED));
                        ui.end_row();
                    }
                });
                ui.add_space(12.0);
            };
            // Groupes répartis dans l'ordre : les deux premiers (courts) partagent la première colonne.
            let layout: &[&[usize]] = match columns {
                3 => &[&[0, 1], &[2], &[3]],
                2 => &[&[0, 1], &[2, 3]],
                _ => &[&[0, 1, 2, 3]],
            };
            ui.columns(layout.len(), |cols| {
                for (col, groups) in cols.iter_mut().zip(layout) {
                    for &g in *groups {
                        group(col, &SHORTCUTS[g]);
                    }
                }
            });
        });
        ui.label(RichText::new("F1 ouvre et ferme cette aide · Échap la ferme").size(11.0).color(MUTED));
    });
    if modal.should_close() {
        keep_open = false;
    }
    keep_open
}
