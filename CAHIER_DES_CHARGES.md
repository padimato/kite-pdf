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

### 3.2. Interface Utilisateur "Zéro Perte Verticale" (Style Arc Browser Complet)
- **Suppression totale de la barre supérieure** : aucune barre d'onglets ou de ruban en haut de l'écran. Le document PDF occupe **100 % de la hauteur** de la fenêtre.
- **Sidebar Latérale Gauche (Style Arc Complet)** :
  - **Deux modes instantanés** : **Épinglée** (visible à gauche) ou **Auto-masquée** (le PDF prend 100 % de la largeur et 100 % de la hauteur ; la barre glisse en surimpression avec une animation fluide dès que la souris approche du bord gauche de l'écran ou via le raccourci `Ctrl+B`).
  - **Zone haute de la Sidebar** : barre d'outils compacte en grille d'icônes (Sélection, Surligneur, Texte libre, Dessin/Encre, Formes/Flèches, Notes, Signatures/Mesure, Recherche, Statut OCR).
  - **Zone centrale de la Sidebar** : liste verticale des **onglets PDF ouverts** (15+ visibles d'un coup d'œil sans troncature des titres, avec indicateur d'état OCR/modification `*` et bouton de fermeture).
  - **Zone basse / Tiroir contextuel** : miniature des pages, table des matières (signets PDF), ou liste des résultats de recherche (`Ctrl+F`).
- **Palette de commandes flottante (`Ctrl+K` / `Ctrl+P`)** : barre de commande centrale éphémère (façon Arc / VS Code) pour changer d'onglet, sauter à une page, lancer un outil ou exécuter une action.

### 3.3. OCR "Malin" à 3 Vitesses (Hybride Texte Natif + Images Embarquées & Multi-Angles)
Le problème classique des lecteurs PDF : lorsqu'un fichier Word contenant un graphique en `.jpg`/`.png` est exporté en PDF, la page possède déjà du texte natif. Les outils OCR classiques ignorent donc la page ou écrasent tout le document.

**Notre architecture "Smart Hybrid OCR" avec Ordonnanceur Adaptatif (3 vitesses) :**
1. **Phase 1 — Ouverture Instantanée (0 ms de blocage)** :
   - À l'ouverture d'un ou de 15 PDF, l'affichage des pages et l'extraction du texte natif sont immédiats.
   - Le lecteur vérifie si les images embarquées possèdent déjà une couche texte OCR enregistrée lors d'une session précédente : si oui, aucun recalcul n'est nécessaire !
2. **Phase 2 — Mode "Éco / Tâche de fond" (Basse Priorité)** :
   - Dès que l'application est au repos, un worker asynchrone basse priorité (bridé pour consommer un minimum de CPU/RAM et ne jamais ralentir le PC) inspecte les pages de l'onglet actif (en commençant par la page visible et ses voisines).
   - Il isole uniquement les objets `XObject Image` intégrés (ex: graphiques JPG dans un PDF Word) dépourvus de couche texte sous-jacente, et les passe au moteur OCR orienté en arrière-plan.
3. **Phase 3 — Mode "Turbo" à l'activation du `Ctrl+F`** :
   - Dès que l'utilisateur lance `Ctrl+F`, les occurrences du texte natif et des pages déjà OCRisées s'affichent **à la milliseconde**.
   - Simultanément, si des images du document n'ont pas encore été scannées, l'ordonnanceur bascule en **Mode Turbo** (parallélisation maximale des workers / accélération matérielle tout en isolant strictement le thread UI pour garantir 0 lag à l'écran) afin de scanner tout le reste du document à pleine vitesse et d'ajouter les résultats en temps réel dans la liste `Ctrl+F`.
4. **Détection de texte orienté (360° / Diagonale / Vertical) & Modèle Linguistique** :
   - Détecteur **DBNet orienté** (boîtes polygonales $(x, y, w, h, \theta)$) + redressement géométrique (*Perspective Warp*) + classifieur d'angle ($0^\circ / 180^\circ$) + reconnaissance **PP-OCRv4 Latin** (ultra-rapide, couvrant nativement Français, Anglais, langues européennes à alphabet latin, chiffres et unités scientifiques/techniques d'axes de graphes).
   - Architecture modulaire permettant d'ajouter des packs de langues supplémentaires (ex: langues asiatiques/cyrilliques) uniquement si nécessaire sans alourdir le modèle par défaut.
5. **Phase 4 — Sauvegarde Définitive dans le PDF (`Ctrl+S`)** :
   - Lors de l'enregistrement du PDF, les textes reconnus dans les graphiques/images sont écrits directement dans le PDF sous forme de couche texte standard invisible (`Text Rendering Mode = 3`, avec matrice de transformation `Tm` orientée à l'angle $\theta$) et marqués comme traités.
   - **Avantage clé** : à la prochaine ouverture du PDF (chez toi ou chez un tiers sur Adobe Acrobat), le `Ctrl+F` trouvera instantanément les mots des graphiques sans jamais avoir à relancer l'OCR !

### 3.4. Outils d'Annotation Standardisés (Compatibilité Adobe Acrobat — ISO 32000)
Toutes les annotations respectent la norme **ISO 32000-1 / 32000-2 (§12.5)** afin d'être modifiables nativement dans Adobe Acrobat, Foxit, macOS Preview ou PDF-XChange :
1. **Surlignage de texte (`/Subtype /Highlight`) & Surlignage libre** : attaché aux coordonnées `/QuadPoints` du texte sélectionné (ou tracé libre au marqueur translucent).
2. **Ajout de bloc de texte visible (`/Subtype /FreeText`)** : boîte de texte directement affichée et éditable sur la page avec police, taille, couleur (`DefaultAppearance /DA`) et flux d'apparence (`/AP`). Un utilisateur sur Adobe Acrobat peut double-cliquer dessus pour modifier le texte ou déplacer le bloc.
3. **Dessin à main levée, Flèches & Formes (`/Ink`, `/Line`, `/Square`, `/Circle`)** : tracés vectoriels fluides stockés dans `/InkList`, flèches d'annotation de schémas et rectangles/cercles.
4. **Notes & Commentaires en info-bulle (`/Subtype /Text`)** : icône de note (Sticky Note) ouvrant une bulle de commentaire pliable/dépliable.
5. **Signatures, Tampons & Outil de Mesure (`/Stamp`, `/Ink`, `/Line` + `/Measure`)** : apposition rapide de signature/tampon et règle de mesure de distance/cote sur les plans et graphiques.
6. **Édition / Suppression universelle** : sélection au clic de n'importe quelle annotation (créée dans notre lecteur ou dans Adobe Acrobat) pour la déplacer, la redimensionner, modifier son texte/couleur ou la supprimer, avec historique `Ctrl+Z` / `Ctrl+Y`.

### 3.5. Ergonomie Trackpad Windows (Precision Touchpad) & Raccourcis Clavier
- **Compatibilité Trackpad Native (Windows Precision Touchpad)** :
  - **Défilement fluide 2 doigts (Vertical & Horizontal)** : prise en charge native des deltas continus au pixel près (`smooth_scroll_delta.x` et `smooth_scroll_delta.y`) pour naviguer dans les pages et se déplacer latéralement lorsqu'on est zoomé, sans saccades par crans.
  - **Pinch-to-Zoom à 2 doigts** : zoom progressif centré sur la position du pointeur via `zoom_delta()` (compatible pincement 2 doigts sur trackpad Windows et `Ctrl + glissement 2 doigts`).
  - **Panoramique au clic/glisser (Outil Main ou Espace + glisser à 1 doigt)** : déplacement libre sur la page au trackpad d'un seul doigt lorsque la barre d'espace est maintenue.
  - **Survol du bord gauche au trackpad** : détection douce avec hystérésis pour déployer/replier la sidebar Arc sans déclenchement intempestif.
- **Navigation clavier** : `Flèches` / `J`/`K` (défilement), `Espace` / `Shift+Espace` (page suivante/précédente), `Ctrl+Molette` ou `Ctrl++`/`Ctrl+-` (zoom), `Ctrl+0` (ajuster à la page), `Ctrl+9` (ajuster à la largeur).
- **Onglets & UI** : `Ctrl+B` (afficher/masquer la sidebar Arc), `Ctrl+Tab` / `Ctrl+Shift+Tab` (onglet suivant/précédent), `Ctrl+W` (fermer l'onglet), `Ctrl+O` (ouvrir), `Ctrl+K` (palette de commandes).
- **Outils rapides (1 touche hors saisie texte)** : `V` (Sélection), `H` (Surligner texte), `M` (Surligneur libre), `T` (Ajouter texte `FreeText`), `D` (Dessin `Ink`), `R` (Rectangle), `Suppr` (Supprimer l'annotation sélectionnée).
- **Recherche & Fichier** : `Ctrl+F` (Recherche hybride Natif + OCR), `Ctrl+S` (Enregistrer le PDF avec annotations standard).

### 3.6. Protocole de Validation par Sous-Agent QA & Preuves Visuelles
Pour chaque jalon :
1. **Tests automatisés & fonctionnels** : tests unitaires et d'intégration Cargo (`cargo test`), incluant la simulation d'événements Trackpad (scroll 2D, pinch-to-zoom) et clavier.
2. **Sous-Agent QA indépendant** : un sous-agent dédié est invoqué à la fin de chaque jalon pour inspecter, compiler, exécuter l'application en conditions réelles et vérifier de manière objective tous les critères d'acceptation.
3. **Captures d'écran systématiques** : chaque fonctionnalité visuelle est validée par une ou plusieurs captures d'écran réelles de l'application en cours d'exécution (et/ou export de frame GPU), intégrées au rapport de validation présenté à l'utilisateur.

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
  - Surlignage de texte (`Highlight` avec `/QuadPoints`) et surlignage libre.
  - Bloc de texte directement visible et éditable (`FreeText`).
  - Dessin à main levée (`Ink` avec `/InkList`), flèches et formes (`Line`, `Square`, `Circle`).
  - Notes / Commentaires en info-bulle (`Text` / Sticky Notes).
  - Signatures / Tampons (`Stamp`) et outil de mesure de cotes sur graphiques/plans.
- Sauvegarde dans le PDF respectant la norme ISO 32000 (`/Annots` + `/AP`).
- Pile d'annulation `Ctrl+Z` / `Ctrl+Y`.
- **Critère de validation** : un bloc de texte ou surlignage créé dans notre lecteur est directement modifiable/déplaçable dans Adobe Acrobat (et inversement).

### Jalon 6 : OCR Hybride à 3 Vitesses & Multi-Orientation (Textes en diagonale & Graphiques JPG)
- Détection automatique des objets `XObject Image` intégrés dans les pages PDF (ex: graphiques JPG dans un PDF issu de Word) et des pages scannées.
- Intégration du pipeline **ONNX Runtime (`ort`) + PaddleOCR orienté (PP-OCRv4 Latin FR/EN + symboles)** : détection polygonale DBNet + redressement d'angle $0^\circ\text{--}360^\circ$ + reconnaissance.
- **Ordonnanceur OCR à 3 vitesses** :
  1. **Ouverture instantanée** (zéro blocage, lecture du cache/couche existante).
  2. **Tâche de fond Éco** (basse priorité sur l'onglet actif pour ne jamais ralentir le PC).
  3. **Mode Turbo sur `Ctrl+F`** (scan haute vitesse multi-threadé du document complet sans lag de l'UI, avec résultats affichés au fil de l'eau).
- **Persistance dans le PDF (`Ctrl+S`)** : injection d'une couche texte invisible normalisée (`Tr 3` orientée) dans le PDF afin que le scan n'ait plus jamais besoin d'être refait aux ouvertures suivantes.
- **Critère de validation** : sur un PDF contenant du texte Word + un graphique en image `.jpg` avec des titres d'axes verticaux ou en diagonale, `Ctrl+F` trouve et surligne les textes du graphique, et après sauvegarde (`Ctrl+S`), ces textes sont immédiatement cherchables dès l'ouverture (y compris dans un autre lecteur PDF).

### Jalon 7 : Finitions, Persistance de Session & Packaging Windows
- Restauration automatique de la session (les 15 onglets ouverts, positions de lecture et états de la sidebar au redémarrage).
- Table des matières (signets PDF) et vignettes de pages dans le tiroir de la sidebar.
- Compilation optimisée (`release` avec LTO) et packaging d'un exécutable Windows propre.
