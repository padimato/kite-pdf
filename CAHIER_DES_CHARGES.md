# Cahier des Charges — Super Lecteur PDF (Windows / Rust)

## 1. Vision & Objectifs du Projet

Concevoir un lecteur et éditeur d'annotations PDF natif pour Windows, écrit en **Rust**, combinant :
1. **Empreinte mémoire minimale & Haute concurrence** : ouverture fluide de **15+ fichiers PDF volumineux en simultané** sans saturation de la RAM.
2. **Maximisation de l'espace de travail (UI façon Arc Browser)** : suppression des barres d'outils horizontales chronophages en haut d'écran au profit d'une barre latérale gauche intelligente et escamotable, laissant jusqu'à **100 % de la hauteur d'écran** au document.
3. **OCR Hybride & Multi-orientation ("Smart OCR")** : détection et reconnaissance de texte dans toutes les directions (0° à 360°, texte en diagonale, vertical) ciblant intelligemment les images embarquées (ex. graphiques JPG/PNG insérés dans un document Word exporté en PDF) sans altérer le texte natif déjà présent.
4. **Annotations 100 % Standard ISO 32000 (Adobe Acrobat Compatible)** : création et édition d'annotations (`FreeText`, `Highlight`, `Ink`, `Square`, etc.) stockées sous forme d'objets PDF normalisés (`/Annots` + `/AP`), éditables et déplaçables dans Adobe Acrobat ou tout autre lecteur standard.
5. **Productivité au clavier (Keyboard-First)** : contrôle intégral via raccourcis configurables et palette de commandes rapide.

---

## 2. Architecture Technique & Briques Open-Source (GitHub)

Pour accélérer le développement tout en garantissant des performances natives, le projet s'appuie sur les meilleures briques open-source de l'écosystème Rust :

| Composant | Brique / Crate retenue | Rôle & Justification |
| :--- | :--- | :--- |
| **Interface Graphique & Rendu GPU** | **`egui` + `eframe` (`wgpu` / DirectX 12)** | UI immédiate ultra-légère, rendu 60-144 FPS accéléré matériellement, zéro surcharge WebView/Chromium (contrairement à Electron/Tauri), idéal pour un canevas PDF zoomable et une sidebar façon Arc. |
| **Moteur de Rendu & Parsing PDF** | **`pdfium-render`** + [`bblanchon/pdfium-binaries`](https://github.com/bblanchon/pdfium-binaries) | Wrapper Rust autour de PDFium (C++). Rendu matriciel ultra-rapide, extraction des boîtes de texte natif, inspection des `XObject` images embarqués par page et API native d'annotations (`FPDFAnnot_*`). |
| **Manipulation Bas-Niveau ISO 32000** | **`lopdf`** (en complément de PDFium) | Manipulation directe de l'arbre d'objets PDF (`/Annots`, `/AP` appearance streams, `/QuadPoints`, injection de calque texte invisible `Tr 3` pour la sauvegarde OCR durable). |
| **Moteur OCR Multi-Orientation** | **`ort` (ONNX Runtime)** + **PaddleOCR (PP-OCRv4 / DBNet + Angle Cls + SVTR)** (inspiré de [`oar-ocr`](https://github.com/ando-ri/oar-ocr)) | Contrairement à Tesseract ou `ocrs` (limités aux textes horizontaux), le pipeline DBNet (polygones orientés) + redressement géométrique (warp perspective) + reconnaissance permet de lire les légendes d'axes inclinées ou en diagonale avec accélération DirectML (GPU Windows) ou CPU AVX2. |
| **Concurrence & Workers Asynchrones** | **`tokio` / `rayon` + `crossbeam-channel`** | Rendu des tuiles PDF et exécution de l'OCR en arrière-plan sans jamais bloquer le thread UI. |

*(Références d'inspiration sur GitHub : `needyamin/pdf-reader` pour le couplage `egui` + `pdfium-render`, `oar-ocr` / `paddleocr-rs` pour le pipeline ONNX orienté, `pdf-annot` pour la conformité ISO 32000-2 §12.5).*

---

## 3. Spécifications Fonctionnelles Détaillées

### 3.1. Gestion de la Mémoire (Objectif : 15+ PDF ouverts simultanément)
- **Chargement paresseux (Lazy Loading)** : à l'ouverture d'un PDF, seules la table de références croisées (`xref`) et les dimensions des pages sont chargées.
- **Virtualisation du Viewport** : seules les pages actuellement visibles à l'écran (plus une marge de pré-chargement de $\pm 1$ page) sont rendues sous forme de textures GPU.
- **Cache Global LRU à budget mémoire plafonné** : un gestionnaire centralisé (`TextureCache`) partage un quota de RAM/VRAM strict (ex: 250 Mo max au total pour l'ensemble des 15 onglets).
- **Hibernation automatique des onglets inactifs** : dès qu'un onglet passe en arrière-plan, ses textures haute résolution sont évincées en priorité ; seuls son état de navigation (page courante, facteur de zoom, scroll) et son index de recherche léger restent en RAM.

### 3.2. Interface Utilisateur "Zéro Perte Verticale" (Style Arc Browser)
- **Suppression de la barre supérieure** : aucune barre d'onglets ou de ruban en haut de l'écran. Le document PDF occupe **100 % de la hauteur** de la fenêtre.
- **Sidebar Latérale Gauche (Arc-style)** :
  - **Épinglée ou Auto- escamotable (`Ctrl+S` ou survol du bord gauche)** : en mode masqué, le PDF occupe 100 % de la largeur et 100 % de la hauteur.
  - **Zone haute de la Sidebar** : barre d'outils compacte en grille d'icônes (Sélection, Surligneur, Texte libre, Dessin/Encre, Formes, Recherche, OCR).
  - **Zone centrale de la Sidebar** : liste verticale des **onglets PDF ouverts** (15+ visibles d'un coup d'œil sans troncature des titres, avec indicateur de modification `*` et bouton de fermeture).
  - **Zone basse / Tiroir contextuel** : miniature des pages, table des matières (signets PDF), ou liste des résultats de recherche (`Ctrl+F`).
- **Palette de commandes flottante (`Ctrl+K` / `Ctrl+P`)** : barre de commande centrale éphémère (façon Arc / VS Code) pour changer d'onglet, sauter à une page, lancer un outil ou exécuter l'OCR.

### 3.3. OCR "Malin" (Hybride Texte Natif + Images Embarquées & Multi-Angles)
Le problème classique des lecteurs PDF : lorsqu'un fichier Word contenant un graphique en `.jpg`/`.png` est exporté en PDF, la page possède déjà du texte natif. Les outils OCR classiques ignorent donc la page ou écrasent tout le document.

**Notre pipeline "Smart Hybrid OCR" :**
1. **Analyse structurelle de la page** :
   - Extraction de la couche de texte natif existante (via `pdfium`) avec ses boîtes englobantes exactes.
   - Inspection des objets graphiques de la page (`FPDFPage_CountObjects` / `FPDF_PAGEOBJ_IMAGE`) pour isoler chaque image matricielle intégrée (ex: un graphique JPG inséré dans un rapport Word) ainsi que les zones vectorielles dépourvues de texte.
2. **Détection de texte orienté (360° / Diagonale / Vertical)** :
   - Application d'un détecteur **DBNet orienté** (via ONNX Runtime) uniquement sur les images extraites (ou sur la page entière s'il s'agit d'un scan).
   - Détection sous forme de **boîtes englobantes orientées (Polygones / Rotated Bounding Boxes $(x, y, w, h, \theta)$)**.
   - Découpage et redressement géométrique (*Perspective Warp* / rotation inverse d'angle $-\theta$) de chaque segment de texte détecté (même à 45°, 90° ou à l'envers).
   - Passage dans le classifieur d'orientation ($0^\circ / 180^\circ$) puis dans le modèle de reconnaissance (CRNN / SVTR multilingue FR/EN).
3. **Fusion dans l'Index de Recherche (`Ctrl+F`) & Injection PDF** :
   - **Recherche unifiée immédiate** : l'index `Ctrl+F` interroge simultanément le texte natif et le texte OCRisé des images embarquées, et surligne la zone exacte (même inclinée) sur le graphique !
   - **Sauvegarde optionnelle dans le PDF** : possibilité d'injecter le texte reconnu dans le fichier PDF sous forme de texte invisible normalisé (`Text Rendering Mode = 3`, avec matrice de transformation `Tm` incluant la rotation $\theta$), permettant à n'importe quel autre lecteur PDF de trouver ces mots via `Ctrl+F`.

### 3.4. Outils d'Annotation Standardisés (Compatibilité Adobe Acrobat)
Toutes les annotations respectent la norme **ISO 32000-1 / 32000-2 (§12.5)** afin d'être modifiables nativement dans Adobe Acrobat, Foxit, macOS Preview ou PDF-XChange :
- **Surlignage de texte (`/Subtype /Highlight`)** : attaché aux coordonnées `/QuadPoints` du texte sélectionné (couleur et opacité configurables).
- **Surlignage libre & Dessin à main levée (`/Subtype /Ink`)** : tracés vectoriels fluides stockés dans `/InkList` avec lissage de courbe de Bézier.
- **Ajout de bloc de texte (`/Subtype /FreeText`)** : boîte de texte éditable avec police, taille, couleur (`DefaultAppearance /DA`) et flux d'apparence (`/AP`). Un utilisateur sur Adobe Acrobat peut double-cliquer dessus pour modifier le texte ou déplacer le bloc.
- **Notes & Commentaires (`/Subtype /Text`)** : post-it / bulles de commentaire.
- **Formes géométriques (`/Square`, `/Circle`, `/Line`)** : encadrés, flèches et traits pour annoter des schémas.
- **Édition / Suppression d'annotations existantes** : sélection au clic de n'importe quelle annotation (créée dans notre lecteur ou dans Adobe Acrobat) pour la déplacer, la redimensionner, modifier son texte/couleur ou la supprimer, avec historique `Ctrl+Z` / `Ctrl+Y`.

### 3.5. Raccourcis Clavier
- **Navigation** : `Flèches` / `J`/`K` (défilement), `Espace` / `Shift+Espace` (page suivante/précédente), `Ctrl+Molette` ou `Ctrl++`/`Ctrl+-` (zoom), `Ctrl+0` (ajuster à la page).
- **Onglets & UI** : `Ctrl+S` ou `Alt+S` (afficher/masquer la sidebar Arc), `Ctrl+Tab` / `Ctrl+Shift+Tab` (onglet suivant/précédent), `Ctrl+W` (fermer l'onglet), `Ctrl+O` (ouvrir), `Ctrl+K` (palette de commandes).
- **Outils rapides (1 touche hors saisie texte)** : `V` (Sélection), `H` (Surligner texte), `M` (Surligneur libre), `T` (Ajouter texte `FreeText`), `D` (Dessin `Ink`), `R` (Rectangle), `Suppr` (Supprimer l'annotation sélectionnée).
- **Recherche & Fichier** : `Ctrl+F` (Recherche hybride Natif + OCR), `Ctrl+S` (Enregistrer le PDF avec annotations standard).

---

## 4. Plan de Développement par Jalons (Milestones)

Chaque jalon sera développé, testé et **validé avec toi** avant de passer au suivant, avec un commit/tag Git dédié.

### Jalon 1 : Socle Rust, Intégration PDFium & Affichage Haute Performance
- Initialisation du projet Cargo (`egui` + `eframe` + `pdfium-render`).
- Script/mécanisme d'intégration de `pdfium.dll` pour Windows x64.
- Ouverture d'un fichier PDF, rendu matriciel d'une page, affichage dans le canevas GPU avec zoom fluide centré sur le curseur et défilement continu vertical.
- **Critère de validation** : ouverture et navigation à 60+ FPS dans un PDF standard.

### Jalon 2 : Interface "Arc Browser" (Sidebar Latérale) & Raccourcis Clavier
- Implémentation de la barre latérale gauche repliable (`Ctrl+B` / `Ctrl+S` ou auto-hide au survol du bord gauche), zéro barre en haut d'écran.
- Gestion multi-onglets verticale dans la sidebar (ouverture, fermeture, réorganisation, indicateurs d'état).
- Système centralisé de raccourcis clavier et palette de commandes rapide (`Ctrl+K`).
- **Critère de validation** : bascule instantanée entre plusieurs onglets et mode plein espace (~100 % de surface PDF).

### Jalon 3 : Moteur de Virtualisation Mémoire (Objectif 15+ PDF en simultané)
- Mise en place du `TextureCache` global avec politique d'éviction LRU et plafond mémoire strict.
- Rendu asynchrone multi-threadé des pages visibles et pré-chargement intelligent.
- Hibernation automatique des onglets en arrière-plan et affichage temps réel de la consommation RAM dans un mini-indicateur de diagnostic.
- **Critère de validation** : ouverture simultanée de 15 gros PDF avec une consommation RAM maîtrisée et une navigation instantanée d'un onglet à l'autre.

### Jalon 4 : Sélection de Texte Natif & Recherche `Ctrl+F` Classique
- Extraction des caractères et boîtes englobantes du texte natif via PDFium.
- Sélection de texte à la souris (copier `Ctrl+C`) et recherche instantanée `Ctrl+F` sur tout le document avec mise en surbrillance et navigation d'occurrence en occurrence.
- **Critère de validation** : sélection et recherche `Ctrl+F` fluides sur le texte natif.

### Jalon 5 : Outils d'Annotation Standard ISO 32000 (Adobe Acrobat Compatible)
- Création, affichage interactif, déplacement et modification d'annotations :
  - Surlignage de texte (`Highlight` avec `/QuadPoints`).
  - Bloc de texte éditable (`FreeText`).
  - Dessin / Surlignage libre (`Ink` avec `/InkList`).
  - Formes (`Square`, `Line`/Flèche) et Notes (`Text`).
- Sauvegarde incrémentale ou complète dans le PDF respectant la norme ISO 32000 (`/Annots` + `/AP`).
- Pile d'annulation `Ctrl+Z` / `Ctrl+Y`.
- **Critère de validation** : un bloc de texte ou surlignage créé dans notre lecteur est directement modifiable/déplaçable dans Adobe Acrobat (et inversement).

### Jalon 6 : OCR Hybride Intelligent & Multi-Orientation (Textes en diagonale & Graphiques JPG)
- Détection automatique des objets `XObject Image` intégrés dans les pages PDF (ex: graphiques JPG dans un PDF issu de Word) et des pages scannées.
- Intégration du pipeline **ONNX Runtime (`ort`) + PaddleOCR orienté** (détection polygonale DBNet + redressement d'angle $0^\circ\text{--}360^\circ$ + reconnaissance).
- Fusion transparente des résultats OCR orientés dans l'index de recherche `Ctrl+F` (avec surlignage orienté sur les graphiques) et option d'écriture d'une couche texte invisible (`Tr 3`) dans le PDF.
- **Critère de validation** : sur un PDF contenant du texte Word + un graphique en image `.jpg` avec des titres d'axes verticaux ou en diagonale, `Ctrl+F` trouve et surligne les textes du graphique aussi bien que le texte Word.

### Jalon 7 : Finitions, Persistance de Session & Packaging Windows
- Restauration automatique de la session (les 15 onglets ouverts, positions de lecture et états de la sidebar au redémarrage).
- Table des matières (signets PDF) et vignettes de pages dans le tiroir de la sidebar.
- Compilation optimisée (`release` avec LTO) et packaging d'un exécutable Windows propre.
