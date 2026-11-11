# Super Lecteur PDF

Lecteur PDF pour Windows écrit en Rust : rapide, économe en mémoire, avec annotations compatibles Adobe Acrobat et OCR des images intégrées.

## Fonctionnalités

- **Onglets verticaux** dans une barre latérale épinglable ou masquée au survol (`Ctrl+B`). Les onglets en arrière-plan sont mis en veille pour libérer la mémoire.
- **Barre centrale `Ctrl+T`** : rouvre un PDF de l'historique (recherche tolérante aux accents et aux abréviations), va à une page, ou trouve un outil en langage courant (« commentaire », « stabilo », « sauvegarder »). `Ctrl+Maj+T` rouvre le dernier onglet fermé.
- **Annotations** : surlignage, texte libre, dessin, flèches et rectangles, notes avec fils de discussion. Elles sont enregistrées au format standard (ISO 32000) et relues par Acrobat.
- **Recherche `Ctrl+F`** dans le texte du document, y compris le texte des figures et des scans grâce à l'OCR.
- **OCR des images** (PaddleOCR PP-OCRv5, ONNX Runtime) en tâche de fond. Le texte reconnu est enregistré dans le PDF sous forme de couche invisible : il devient cherchable dans Acrobat, et les pages déjà analysées ne le sont plus à la réouverture.
- **Pavé tactile** : défilement fluide et zoom par pincement centré sur le pointeur.
- **Usage courant** : sommaire du document (signets), formulaires à remplir (texte, cases, boutons radio, listes), impression, PDF protégés par mot de passe, reprise de la lecture à la page et au zoom où on l'avait laissée.

## Raccourcis

| Raccourci | Action |
|---|---|
| `Ctrl+T` / `Ctrl+K` | Barre centrale (PDF récents, outils, page ou zoom) |
| `Ctrl+Maj+T` | Rouvrir le dernier onglet fermé |
| `Ctrl+O` | Ouvrir un fichier (ou glisser-déposer un PDF dans la fenêtre) |
| `Ctrl+S` | Enregistrer (annotations, formulaire et texte OCR) |
| `Ctrl+Maj+S` | Enregistrer sous (l'original reste intact) |
| `Ctrl+P` | Imprimer |
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

Les bibliothèques natives et les modèles OCR ne sont pas dans le dépôt : ils sont téléchargés depuis leur source officielle, dans la version listée dans [`deps.toml`](deps.toml) (adresse, version et empreinte SHA-256 de chaque fichier).

```bash
cargo run --release -- --fetch-deps
```

Cette commande installe dans le dossier courant les fichiers manquants :

| Fichier | Source |
|---|---|
| `pdfium.dll` | [binaires PDFium de bblanchon](https://github.com/bblanchon/pdfium-binaries/releases) |
| `assets/models/onnxruntime.dll` | [ONNX Runtime, Microsoft](https://github.com/microsoft/onnxruntime/releases) |
| `assets/models/*.onnx`, `ppocrv5_latin_dict.txt` | modèles PaddleOCR PP-OCRv5 au format ONNX, [publiés par oar-ocr](https://github.com/GreatV/oar-ocr/releases/tag/v0.3.0) |

Sans cette étape, l'application télécharge elle-même ces fichiers au premier lancement, dans `%LOCALAPPDATA%\SuperLecteurPDF\deps` : l'écran d'installation attend PDFium, l'OCR peut être installé plus tard. La variable d'environnement `SUPERPDF_DEPS_DIR` désigne un autre dossier (installation portable).

**Autre version** : un fichier différent de la version testée est accepté. L'application affiche alors un avertissement qui indique la version testée et où la télécharger, en cas de problème. Pour adopter une nouvelle version, mettez à jour `deps.toml` puis lancez `cargo test`.

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

Les tests créent leurs PDF dans `test_pdfs/` (ignoré par git). Les tests OCR demandent les dépendances ci-dessus (`--fetch-deps`).

## Outils de développement

| Commande ou variable | Rôle |
|---|---|
| `--fetch-deps [dossier]` | Installe PDFium et l'OCR (voir plus haut) |
| `--bench fichier.pdf` | Mesure le temps de chaque étape : ouverture, rendu, texte, OCR (à lancer en `--release`) |
| `SUPERPDF_TIMING=1` | Affiche le temps entre le lancement et la première page affichée |
| `SUPERPDF_DEPS_DIR` | Dossier des dépendances (installation portable, essais) |
| `SUPERPDF_QA=1` | Active dans une compilation `--release` les options de test (`--screenshot`, `--page`, `--search`…), toujours actives en compilation de développement |

## Licence

Code publié sous licence [MIT](LICENSE).

Composants tiers, chacun sous sa propre licence : [PDFium](https://pdfium.googlesource.com/pdfium/) (BSD-3-Clause et Apache-2.0), [ONNX Runtime](https://github.com/microsoft/onnxruntime) (MIT), modèles [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) (Apache-2.0), ainsi que les crates listées dans `Cargo.toml`.
