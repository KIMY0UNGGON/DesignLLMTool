# G0 regression fixtures

`fixture-manifest.json` lists each input, its SHA-256, source/license, preservation checks, and expected semantic values. The fixture envelope uses `id`, `path`, `format`, `sha256`, `origin`, `license`, `checks`, and `expected`.

## Contents

- `office/basic-preserve.pptx`: directly authored package with editable styled text, a 16×12 checker image, a slide master/layout/theme, speaker notes, a related opaque XML part, and protected `a:extLst`, foreign-attribute, and `mc:AlternateContent` regions in the edited slide part. The package has the parts PowerPoint writes: the slide links its layout, the notes master has a theme part of its own, the presentation relates `presProps`, `viewProps` and `tableStyles`, and `docProps/` holds the core and app properties.
- `office/basic-preserve.docx`: directly authored package with styled text, a merged table, a 16×12 inline checker image, header/footer, a custom XML part, and protected `mc:AlternateContent` inside the edited `word/document.xml` part. The document part relates its `settings.xml`.
- `hwpx/opaque-image-preserve.hwpx`: genuine HWPX sample with a manifest-listed opaque XML part and a protected foreign attribute on an `hp:t` in the edited section. The original `BinData/image1.bmp` payload is preserved.
- `hwp/test-image.hwp`: unchanged, genuine HWP 5 sample. It is not a ZIP renamed to `.hwp`.
- `data/known-sd.csv`: three exact decimal strings with SD and `n=3`.
- `data/invalid-cases.csv`: missing, locale-ambiguous, negative-error, asymmetric-error, invalid-boundary, incomplete-CI, missing-n, log-axis, and duplicate-category cases.
- `pdf/evidence-p12.pdf`: twelve physical pages, with A-to-B evidence only on physical page index 11. Its printed page label is `9`.
- `pdf/evidence-p12-userunit2.pdf`: the same 12-page evidence fixture with `/UserUnit 2` to exercise the physical-coordinate scale in `PdfPageGeometry`.
- `assets/invalid-crc.png`: deliberately malformed negative test input retaining the original invalid PNG bytes; it is never embedded in a document.

The DOCX, PPTX, CSV, PDF, checker image, and malformed PNG are authored for this test set and dedicated to CC0-1.0. HWP/HWPX samples come from [edwardkim/rhwp](https://github.com/edwardkim/rhwp/tree/bc38ff55a7e8acb65aebebe237dca0542480d381/samples), pinned to commit `bc38ff55a7e8acb65aebebe237dca0542480d381`. That repository's MIT license is copied to `_sources/rhwp/LICENSE`. The derived HWPX changes `Contents/content.hpf`, adds `Luna/opaque.xml`, and annotates the `자리차지` text element with a fixture namespace attribute; source image and other original parts are retained.

## PDF evidence coordinates

`Evidence.region` uses `Rect { x, y, width, height }` in integer EMU, relative to the page CropBox's top-left, with x increasing right and y increasing down, before page rotation. The fixture has MediaBox `[0, 0, 612, 792]` pt and CropBox `[18, 20, 594, 760]` pt, with rotation zero and PDF `UserUnit=1`. `PdfPageGeometry` stores both boxes in PDF user space with x right/y up, after converting points to EMU. The expected evidence area is `(72, 60, 260, 20)` PDF points, or `[914400, 762000, 3302000, 254000]` EMU at `12700 EMU/pt`. The expected evidence records `origin: "paper_evidence"` and `review_state: "unreviewed"`: evidence read automatically from a PDF stays unreviewed until a user confirms it, so the manifest never states `reviewed`.

For the expected transform, CropBox EMU is `{ x_min: 228600, y_min: 254000, x_max: 7543800, y_max: 9652000 }`. The evidence box maps to PDF user-space EMU as `left=x_min+region.x`, `top=crop.y_max-region.y`, and `bottom=top-region.height`.

The physical page index is zero-based (`11`); the viewer page number is one-based (`12`); the printed label (`9`) is independent. These values must not be conflated. The manifest records both index bases and the independent printed label.

## Checks

The files are kept byte for byte (`.gitattributes` marks `fixtures/**` as `-text`), and `fixture-manifest.json` records each SHA-256. `tests/integration` and the crate tests open them with the real format adapters. Do not edit a fixture by hand: a changed file needs a new manifest hash and a reason in the commit.
