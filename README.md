# Super Lecteur PDF

Lecteur PDF pour Windows écrit en Rust : rapide, économe en mémoire, avec annotations compatibles Adobe Acrobat et OCR des images intégrées.

## Fonctionnalités

- **Onglets verticaux** dans une barre latérale épinglable ou masquée au survol (`Ctrl+B`). Les onglets en arrière-plan sont mis en veille pour libérer la mémoire.
- **Barre centrale `Ctrl+T`** : rouvre un PDF de l'historique (recherche tolérante aux accents et aux abréviations), va à une page, ou trouve un outil en langage courant (« commentaire », « stabilo », « sauvegarder »). `Ctrl+Maj+T` rouvre le dernier onglet fermé.
- **Annotations** : surlignage, texte libre, dessin, flèches et rectangles, notes avec fils de discussion. Elles sont enregistrées au format standard (ISO 32000) et relues par Acrobat.
- **Recherche `Ctrl+F`** dans le texte du document, y compris le texte des figures et des scans grâce à l'OCR.
- **OCR des images** (PaddleOCR PP-OCRv5, ONNX Runtime) en tâche de fond. Le texte reconnu est enregistré dans le PDF sous forme de couche invisible : il devient cherchable dans Acrobat, et les pages déjà analysées ne le sont plus à la réouverture.
- **Pavé tactile** : défilement fluide et zoom par pincement centré sur le pointeur.

## Raccourcis

| Raccourci | Action |
|---|---|
| `Ctrl+T` / `Ctrl+K` | Barre centrale (PDF récents, outils, pages) |
| `Ctrl+Maj+T` | Rouvrir le dernier onglet fermé |
| `Ctrl+O` | Ouvrir un fichier |
| `Ctrl+S` | Enregistrer (annotations et texte OCR) |
| `Ctrl+F` | Rechercher dans le document |
| `Ctrl+W` / `Ctrl+Tab` | Fermer / changer d'onglet |
| `Ctrl+B` | Épingler ou masquer la barre latérale |
| `Ctrl+9` / `Ctrl+0` | Ajuster à la largeur / à la page |
| `V` `H` `T` `D` `R` `N` `M` | Outils : sélection, surlignage, texte, dessin, flèche, note, mesure |
| `Ctrl+Z` / `Ctrl+Y` | Annuler / rétablir |

## Compilation

Prérequis : Windows 10 ou 11 (x64) et [Rust](https://rustup.rs) stable.

```bash
git clone https://github.com/padimato/super-lecteur-pdf.git
cd super-lecteur-pdf
cargo build --release
```

Les bibliothèques natives et les modèles OCR ne sont pas dans le dépôt. Placez-les ainsi :

```
super-lecteur-pdf/
├── pdfium.dll                       (à côté de l'exécutable ou à la racine)
└── assets/models/
    ├── onnxruntime.dll
    ├── pp-ocrv5_mobile_det.onnx
    ├── latin_pp-ocrv5_mobile_rec.onnx
    ├── ppocrv5_latin_dict.txt
    └── pp-lcnet_x0_25_textline_ori.onnx
```

- **`pdfium.dll`** : archive `pdfium-win-x64` des [binaires PDFium de bblanchon](https://github.com/bblanchon/pdfium-binaries/releases) (fichier `bin/pdfium.dll`).
- **`onnxruntime.dll`** : archive `onnxruntime-win-x64` des [versions d'ONNX Runtime](https://github.com/microsoft/onnxruntime/releases) (1.28 testée).
- **Modèles OCR** : modèles PaddleOCR PP-OCRv5 au format ONNX (détection, reconnaissance latine et son dictionnaire, orientation des lignes), tels que distribués pour la bibliothèque [`oar-ocr`](https://crates.io/crates/oar-ocr).

Sans les modèles, l'application fonctionne normalement ; seul l'OCR est désactivé.

## Lancement

```bash
cargo run --release -- chemin/vers/document.pdf
```

Sans argument, l'application s'ouvre sur la barre centrale et propose les PDF récents. L'historique est conservé dans `%APPDATA%\SuperLecteurPDF\recent.tsv`.

## Tests

```bash
cargo test
```

Les tests créent leurs PDF dans `test_pdfs/` (ignoré par git). Les tests OCR demandent les modèles ci-dessus.

## Licence

Code publié sous licence [MIT](LICENSE).

Composants tiers, chacun sous sa propre licence : [PDFium](https://pdfium.googlesource.com/pdfium/) (BSD-3-Clause et Apache-2.0), [ONNX Runtime](https://github.com/microsoft/onnxruntime) (MIT), modèles [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) (Apache-2.0), ainsi que les crates listées dans `Cargo.toml`.
