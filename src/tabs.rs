use crate::annotations::{load_annotations_from_pdf, AnnotationManager};
use crate::memory_manager::LruTextureCache;
use crate::pdf_engine::{LoadedPdfDocument, PageDimensions, PdfEngine};
use crate::text_search::{PageTextLayer, TextSelectionState};
use crate::viewport::ViewportState;
use anyhow::Result;
use egui::Color32;
use std::path::PathBuf;

pub const SIDEBAR_WIDTH: f32 = 268.0;
pub const HOVER_TRIGGER_EDGE_PX: f32 = 18.0;
pub const HOVER_KEEP_OPEN_PX: f32 = SIDEBAR_WIDTH + 18.0;

/// Outils disponibles dans la grille supérieure de la barre latérale (et activables à 1 touche)
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

/// État de la barre latérale gauche escamotable
#[derive(Debug, Clone)]
pub struct SidebarState {
    /// Si vrai, la barre latérale est épinglée à gauche et décale le canevas PDF.
    /// Si faux, le PDF prend 100% de la largeur et la barre glisse en surimpression au survol du bord gauche.
    pub pinned: bool,
    /// Indique si la barre latérale auto-masquée est actuellement déployée par survol du trackpad
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

    /// Indique si la barre latérale doit être visible (épinglée ou déployée au survol)
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

/// Représente un onglet PDF avec support d'hibernation mémoire automatique.
/// Lorsqu'un onglet est en arrière-plan (`document == None`), son handle C++ PDFium et ses bitmaps
/// sont intégralement libérés de la RAM ; seules ses métadonnées légères (~200 octets) et sa position de lecture sont conservées.
pub struct DocumentTab {
    pub id: u64,
    pub path: PathBuf,
    pub title: String,
    pub page_dimensions: Vec<PageDimensions>,
    pub document: Option<LoadedPdfDocument<'static>>,
    pub text_layers: Vec<PageTextLayer>,
    pub text_selection: TextSelectionState,
    pub annotations: AnnotationManager,
    pub viewport: ViewportState,
    pub is_modified: bool,
    pub badge_color: Color32,
}

impl DocumentTab {
    pub fn new_active(
        id: u64,
        doc: LoadedPdfDocument<'static>,
        badge_color: Color32,
    ) -> Self {
        let path = doc.path.clone();
        let title = doc.title.clone();
        let page_dimensions = doc.page_dimensions.clone();
        let mut annotations = AnnotationManager::new();
        if let Ok(loaded_annots) = load_annotations_from_pdf(&path, &page_dimensions) {
            for a in loaded_annots {
                annotations.add_annotation(a);
            }
            annotations.selected_id = None;
        }
        Self {
            id,
            path,
            title,
            page_dimensions,
            document: Some(doc),
            text_layers: Vec::new(),
            text_selection: TextSelectionState::default(),
            annotations,
            viewport: ViewportState::default(),
            is_modified: false,
            badge_color,
        }
    }

    pub fn page_count(&self) -> usize {
        self.page_dimensions.len()
    }

    /// Extrait à la demande la couche de texte natif des pages si elle n'est pas encore en cache
    pub fn ensure_text_layers(&mut self, engine: &'static PdfEngine) {
        if !self.text_layers.is_empty() {
            return;
        }
        let _ = self.ensure_awake(engine);
        if let Some(doc) = &self.document {
            self.text_layers = doc.extract_all_pages_text_layers();
        }
    }

    /// Indique si l'onglet est actuellement en hibernation mémoire (0 Mo de handle PDFium / textures)
    pub fn is_hibernated(&self) -> bool {
        self.document.is_none()
    }

    /// Met l'onglet en hibernation : libère le handle C++ PDFium et évince ses textures du cache LRU
    pub fn hibernate(&mut self, cache: &mut LruTextureCache) -> usize {
        {
            let _lock = PdfEngine::lock_guard();
            self.document = None;
        }
        cache.evict_tab(self.id)
    }

    /// Réveille l'onglet instantanément s'il était hiberné en rechargeant le document PDFium à la volée
    pub fn ensure_awake(&mut self, engine: &'static PdfEngine) -> Result<()> {
        if self.document.is_none() {
            let doc = engine.load_document(&self.path)?;
            self.page_dimensions = doc.page_dimensions.clone();
            self.document = Some(doc);
        }
        Ok(())
    }
}

/// Action exécutable depuis la Palette de Commandes (`Ctrl+K`)
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteCommand {
    SwitchTab(usize),
    ToggleSidebarPin,
    OpenFileDialog,
    SaveAnnotatedPdf,
    NewDemoTab,
    Open15TabsBenchmark,
    HibernateInactiveTabs,
    PopulateDemoAnnotations,
    UndoAnnotation,
    RedoAnnotation,
    DeleteSelectedAnnotation,
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
        tabs_info: &[(usize, String, usize, bool)],
        active_tab_page_count: usize,
    ) -> Vec<PaletteEntry> {
        let mut all = Vec::with_capacity(32);

        // 1. Onglets ouverts
        for (idx, title, pages, hibernated) in tabs_info {
            let shortcut = match idx {
                0 => "Ctrl+1",
                1 => "Ctrl+2",
                2 => "Ctrl+3",
                3 => "Ctrl+4",
                4 => "Ctrl+5",
                5 => "Ctrl+6",
                6 => "Ctrl+7",
                7 => "Ctrl+8",
                _ => "Ctrl+Tab",
            };
            let state_tag = if *hibernated { " [Hiberné 0 Mo]" } else { " [Actif]" };
            all.push(PaletteEntry {
                category: "Onglet",
                label: format!("#{} : {} ({} p.){}", idx + 1, title, pages, state_tag),
                shortcut,
                command: PaletteCommand::SwitchTab(*idx),
            });
        }

        // 2. Commandes Annotations ISO 32000, Mémoire & Onglets
        all.push(PaletteEntry {
            category: "Annotations",
            label: "Enregistrer le PDF avec les annotations standard ISO 32000 (Compatible Adobe Acrobat)".to_string(),
            shortcut: "Ctrl+S",
            command: PaletteCommand::SaveAnnotatedPdf,
        });
        all.push(PaletteEntry {
            category: "Annotations",
            label: "Générer la vitrine complète d'annotations ISO 32000 sur la page courante".to_string(),
            shortcut: "Démo Annot",
            command: PaletteCommand::PopulateDemoAnnotations,
        });
        all.push(PaletteEntry {
            category: "Annotations",
            label: "Annuler la dernière action d'annotation (Undo)".to_string(),
            shortcut: "Ctrl+Z",
            command: PaletteCommand::UndoAnnotation,
        });
        all.push(PaletteEntry {
            category: "Annotations",
            label: "Rétablir l'action d'annotation (Redo)".to_string(),
            shortcut: "Ctrl+Y",
            command: PaletteCommand::RedoAnnotation,
        });
        all.push(PaletteEntry {
            category: "Annotations",
            label: "Supprimer l'annotation sélectionnée".to_string(),
            shortcut: "Suppr",
            command: PaletteCommand::DeleteSelectedAnnotation,
        });
        all.push(PaletteEntry {
            category: "Mémoire",
            label: "Ouvrir 15 PDF en simultané (Stress-Test Virtualisation & Hibernation)".to_string(),
            shortcut: "15 PDF",
            command: PaletteCommand::Open15TabsBenchmark,
        });
        all.push(PaletteEntry {
            category: "Mémoire",
            label: "Purger le cache de textures & Hiberner tous les onglets inactifs".to_string(),
            shortcut: "Purge RAM",
            command: PaletteCommand::HibernateInactiveTabs,
        });
        all.push(PaletteEntry {
            category: "Interface",
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
    use crate::pdf_engine::ensure_custom_sample_pdf;
    use egui::Vec2;

    #[test]
    fn test_sidebar_autohide_trackpad_hover_hysteresis() {
        let mut sidebar = SidebarState::default();
        assert!(sidebar.pinned);
        assert!(sidebar.is_expanded());

        sidebar.toggle_pinned();
        assert!(!sidebar.pinned);
        assert!(!sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(500.0));
        assert!(!sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(8.0));
        assert!(sidebar.hover_revealed);
        assert!(sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(180.0));
        assert!(sidebar.is_expanded());

        sidebar.update_hover_hysteresis(Some(340.0));
        assert!(!sidebar.hover_revealed);
        assert!(!sidebar.is_expanded());
    }

    #[test]
    fn test_command_palette_filtering_tabs_and_tools() {
        let mut palette = CommandPaletteState::default();
        palette.open();

        let tabs = vec![
            (0, "Architecture_Rust.pdf".to_string(), 5, false),
            (1, "Spécification_ISO_32000.pdf".to_string(), 8, true),
        ];

        palette.query = "spécification".to_string();
        let results = palette.filtered_entries(&tabs, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, PaletteCommand::SwitchTab(1));

        palette.query = "iso".to_string();
        let iso_results = palette.filtered_entries(&tabs, 5);
        assert!(iso_results
            .iter()
            .any(|e| e.command == PaletteCommand::SaveAnnotatedPdf));

        palette.query = "surligner".to_string();
        let tool_results = palette.filtered_entries(&tabs, 5);
        assert_eq!(tool_results.len(), 1);
        assert_eq!(
            tool_results[0].command,
            PaletteCommand::SelectTool(ActiveTool::Highlight)
        );
    }

    #[test]
    fn test_15_pdf_tabs_simultaneous_hibernation_and_instant_wakeup() {
        let engine = PdfEngine::instance();
        let mut cache = LruTextureCache::default();
        let mut tabs: Vec<DocumentTab> = Vec::with_capacity(15);

        // Ouverture de 15 fichiers PDF distincts en simultané
        for i in 1..=15 {
            let path = PathBuf::from(format!("test_pdfs/bench_15_tab_{:02}.pdf", i));
            ensure_custom_sample_pdf(
                &path,
                &format!("DOCUMENT BENCHMARK #{:02}", i),
                (0.15, 0.30, 0.55),
                6,
            )
            .expect("Doit générer le PDF");

            let doc = engine.load_document(&path).expect("Doit charger le PDF");
            let mut tab = DocumentTab::new_active(i as u64, doc, Color32::WHITE);
            tab.viewport.zoom = 1.0 + (i as f32) * 0.05;
            tab.viewport.scroll_offset = Vec2::new(10.0 * i as f32, 40.0 * i as f32);

            // Hiberner tous les onglets précédents, seul le dernier ouvert reste actif
            for prev in tabs.iter_mut() {
                prev.hibernate(&mut cache);
            }
            tabs.push(tab);
        }

        assert_eq!(tabs.len(), 15);
        let active_count = tabs.iter().filter(|t| !t.is_hibernated()).count();
        let hibernated_count = tabs.iter().filter(|t| t.is_hibernated()).count();
        assert_eq!(active_count, 1, "Seul l'onglet actif doit garder un handle PDFium en RAM");
        assert_eq!(hibernated_count, 14, "Les 14 onglets en arrière-plan doivent être hibernés");

        // Bascule sur l'onglet #7 (index 6) : réveil instantané et vérification de la conservation du zoom/scroll
        tabs[14].hibernate(&mut cache);
        tabs[6].ensure_awake(engine).expect("Doit réveiller l'onglet #7");
        assert!(!tabs[6].is_hibernated());
        assert!((tabs[6].viewport.zoom - 1.35).abs() < 1e-4);
        assert!((tabs[6].viewport.scroll_offset.y - 280.0).abs() < 1e-4);
    }
}
