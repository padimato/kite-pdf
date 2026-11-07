use crate::pdf_engine::LoadedPdfDocument;
use crate::viewport::ViewportState;
use egui::Color32;

pub const SIDEBAR_WIDTH: f32 = 264.0;
pub const HOVER_TRIGGER_EDGE_PX: f32 = 18.0;
pub const HOVER_KEEP_OPEN_PX: f32 = SIDEBAR_WIDTH + 18.0;

/// Outils disponibles dans la grille supérieure de la Sidebar Arc (et activables à 1 touche)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTool {
    Select,
    Pan,
    Highlight,
    FreeText,
    Ink,
    Shape,
    StickyNote,
    Measure,
}

impl ActiveTool {
    pub fn all() -> &'static [ActiveTool] {
        &[
            ActiveTool::Select,
            ActiveTool::Pan,
            ActiveTool::Highlight,
            ActiveTool::FreeText,
            ActiveTool::Ink,
            ActiveTool::Shape,
            ActiveTool::StickyNote,
            ActiveTool::Measure,
        ]
    }

    pub fn icon_and_label(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            ActiveTool::Select => ("↖", "Sélection", "V"),
            ActiveTool::Pan => ("✋", "Main / Pan", "Espace"),
            ActiveTool::Highlight => ("✒", "Surligner", "H"),
            ActiveTool::FreeText => ("🅰", "Texte", "T"),
            ActiveTool::Ink => ("✏", "Dessin", "D"),
            ActiveTool::Shape => ("▭", "Formes", "R"),
            ActiveTool::StickyNote => ("💬", "Note", "N"),
            ActiveTool::Measure => ("📏", "Mesure", "M"),
        }
    }
}

/// État de la barre latérale gauche inspirée d'Arc Browser
#[derive(Debug, Clone)]
pub struct SidebarState {
    /// Si vrai, la sidebar est épinglée à gauche et décale le canevas PDF.
    /// Si faux, le PDF prend 100% de la largeur et la sidebar glisse en surimpression au survol du bord gauche.
    pub pinned: bool,
    /// Indique si la sidebar auto-masquée est actuellement déployée par survol du trackpad
    pub hover_revealed: bool,
    /// Progression d'animation fluide entre 0.0 (masquée) et 1.0 (déployée)
    pub anim_progress: f32,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            pinned: true,
            hover_revealed: false,
            anim_progress: 1.0,
        }
    }
}

impl SidebarState {
    /// Bascule entre le mode Épinglé et le mode Auto-masqué (`Ctrl+B`)
    pub fn toggle_pinned(&mut self) {
        self.pinned = !self.pinned;
        if self.pinned {
            self.hover_revealed = false;
        }
    }

    /// Met à jour la détection du survol du bord gauche au Trackpad avec hystérésis
    /// pour éviter que la barre ne clignote quand on déplace le curseur vers les onglets.
    pub fn update_hover_hysteresis(&mut self, pointer_x: Option<f32>) {
        if self.pinned {
            self.hover_revealed = false;
            return;
        }
        if let Some(x) = pointer_x {
            if !self.hover_revealed && x <= HOVER_TRIGGER_EDGE_PX {
                self.hover_revealed = true;
            } else if self.hover_revealed && x > HOVER_KEEP_OPEN_PX {
                self.hover_revealed = false;
            }
        }
    }

    /// Indique si la sidebar doit être visible (épinglée ou déployée au survol)
    pub fn is_expanded(&self) -> bool {
        self.pinned || self.hover_revealed
    }

    /// Fait avancer l'animation fluide d'ouverture/fermeture
    pub fn step_animation(&mut self, dt: f32) -> bool {
        let target = if self.is_expanded() { 1.0 } else { 0.0 };
        let diff = target - self.anim_progress;
        if diff.abs() < 0.005 {
            self.anim_progress = target;
            false
        } else {
            let speed = 14.0;
            self.anim_progress = (self.anim_progress + diff * (speed * dt).clamp(0.08, 0.45))
                .clamp(0.0, 1.0);
            true
        }
    }
}

/// Représente un onglet PDF ouvert dans la barre verticale Arc
pub struct DocumentTab {
    pub id: u64,
    pub document: LoadedPdfDocument<'static>,
    pub viewport: ViewportState,
    #[allow(dead_code)]
    pub is_modified: bool,
    pub badge_color: Color32,
}

impl DocumentTab {
    pub fn page_count(&self) -> usize {
        self.document.page_dimensions.len()
    }
}

/// Action exécutable depuis la Palette de Commandes (`Ctrl+K`)
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteCommand {
    SwitchTab(usize),
    ToggleSidebarPin,
    OpenFileDialog,
    NewDemoTab,
    CloseActiveTab,
    SelectTool(ActiveTool),
    FitToWidth,
    FitToPage,
    ZoomReset100,
    JumpToPage(usize),
}

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub category: &'static str,
    pub label: String,
    pub shortcut: &'static str,
    pub command: PaletteCommand,
}

/// État de la Palette de Commandes rapide (`Ctrl+K` / `Ctrl+P`)
#[derive(Debug, Clone, Default)]
pub struct CommandPaletteState {
    pub is_open: bool,
    pub query: String,
    pub selected_index: usize,
    pub focus_requested: bool,
}

impl CommandPaletteState {
    pub fn open(&mut self) {
        self.is_open = true;
        self.query.clear();
        self.selected_index = 0;
        self.focus_requested = true;
    }

    pub fn close(&mut self) {
        self.is_open = false;
        self.focus_requested = false;
    }

    pub fn toggle(&mut self) {
        if self.is_open {
            self.close();
        } else {
            self.open();
        }
    }

    /// Construit et filtre la liste des commandes en fonction de `self.query`
    pub fn filtered_entries(
        &self,
        tabs_info: &[(usize, String, usize)],
        active_tab_page_count: usize,
    ) -> Vec<PaletteEntry> {
        let mut all = Vec::with_capacity(24);

        // 1. Onglets ouverts
        for (idx, title, pages) in tabs_info {
            let shortcut = match idx {
                0 => "Ctrl+1",
                1 => "Ctrl+2",
                2 => "Ctrl+3",
                3 => "Ctrl+4",
                4 => "Ctrl+5",
                _ => "Ctrl+Tab",
            };
            all.push(PaletteEntry {
                category: "Onglet",
                label: format!("Basculer vers : {} ({} pages)", title, pages),
                shortcut,
                command: PaletteCommand::SwitchTab(*idx),
            });
        }

        // 2. Commandes d'interface & Navigation
        all.push(PaletteEntry {
            category: "Interface Arc",
            label: "Épingler / Masquer la barre latérale gauche (Mode Plein Écran 100%)".to_string(),
            shortcut: "Ctrl+B",
            command: PaletteCommand::ToggleSidebarPin,
        });
        all.push(PaletteEntry {
            category: "Fichier",
            label: "Ouvrir un fichier PDF depuis le disque...".to_string(),
            shortcut: "Ctrl+O",
            command: PaletteCommand::OpenFileDialog,
        });
        all.push(PaletteEntry {
            category: "Onglet",
            label: "Ouvrir un nouvel onglet PDF de démonstration".to_string(),
            shortcut: "Ctrl+T",
            command: PaletteCommand::NewDemoTab,
        });
        all.push(PaletteEntry {
            category: "Onglet",
            label: "Fermer l'onglet PDF actif".to_string(),
            shortcut: "Ctrl+W",
            command: PaletteCommand::CloseActiveTab,
        });
        all.push(PaletteEntry {
            category: "Affichage",
            label: "Ajuster le PDF à la largeur de la fenêtre (Fit Width)".to_string(),
            shortcut: "Ctrl+9",
            command: PaletteCommand::FitToWidth,
        });
        all.push(PaletteEntry {
            category: "Affichage",
            label: "Ajuster une page entière à la hauteur (Fit Page)".to_string(),
            shortcut: "Ctrl+0",
            command: PaletteCommand::FitToPage,
        });
        all.push(PaletteEntry {
            category: "Affichage",
            label: "Réinitialiser le zoom à 100%".to_string(),
            shortcut: "100%",
            command: PaletteCommand::ZoomReset100,
        });

        // 3. Outils d'annotation et de lecture
        for tool in ActiveTool::all() {
            let (icon, name, key) = tool.icon_and_label();
            all.push(PaletteEntry {
                category: "Outil",
                label: format!("{} Activer l'outil : {}", icon, name),
                shortcut: key,
                command: PaletteCommand::SelectTool(*tool),
            });
        }

        // 4. Saut direct vers une page du document actif
        for p in 1..=active_tab_page_count.min(20) {
            all.push(PaletteEntry {
                category: "Navigation",
                label: format!("Aller à la Page {}", p),
                shortcut: "Entrée",
                command: PaletteCommand::JumpToPage(p - 1),
            });
        }

        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return all;
        }

        all.into_iter()
            .filter(|entry| {
                entry.label.to_lowercase().contains(&q)
                    || entry.category.to_lowercase().contains(&q)
                    || entry.shortcut.to_lowercase().contains(&q)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sidebar_autohide_trackpad_hover_hysteresis() {
        let mut sidebar = SidebarState::default();
        assert!(sidebar.pinned);
        assert!(sidebar.is_expanded());

        // Passage en mode auto-masqué (Ctrl+B)
        sidebar.toggle_pinned();
        assert!(!sidebar.pinned);
        assert!(!sidebar.is_expanded());

        // Curseur au centre de l'écran : reste masquée
        sidebar.update_hover_hysteresis(Some(500.0));
        assert!(!sidebar.is_expanded());

        // Curseur glisse au trackpad vers le bord gauche (x = 8 px <= 18 px) : déploiement !
        sidebar.update_hover_hysteresis(Some(8.0));
        assert!(sidebar.hover_revealed);
        assert!(sidebar.is_expanded());

        // Curseur navigue à l'intérieur de la sidebar (x = 180 px <= 282 px) : reste ouverte grâce à l'hystérésis
        sidebar.update_hover_hysteresis(Some(180.0));
        assert!(sidebar.is_expanded());

        // Curseur retourne sur le PDF (x = 340 px > 282 px) : se replie automatiquement
        sidebar.update_hover_hysteresis(Some(340.0));
        assert!(!sidebar.hover_revealed);
        assert!(!sidebar.is_expanded());
    }

    #[test]
    fn test_command_palette_filtering_tabs_and_tools() {
        let mut palette = CommandPaletteState::default();
        palette.open();

        let tabs = vec![
            (0, "Architecture_Rust.pdf".to_string(), 5),
            (1, "Spécification_ISO_32000.pdf".to_string(), 8),
        ];

        palette.query = "iso".to_string();
        let results = palette.filtered_entries(&tabs, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, PaletteCommand::SwitchTab(1));

        palette.query = "surligner".to_string();
        let tool_results = palette.filtered_entries(&tabs, 5);
        assert_eq!(tool_results.len(), 1);
        assert_eq!(
            tool_results[0].command,
            PaletteCommand::SelectTool(ActiveTool::Highlight)
        );
    }
}
