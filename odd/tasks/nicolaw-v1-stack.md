# Feature: nicolaw-v1-stack

Locator: `odd/tasks/nicolaw-v1-stack.md`
Engram mirror: `odd/nicolaw-v1-stack/tasks`

## Objective

Build Nicolaw v1 as a three-layer PR stack: a private, source-linked hearing assistant for Colombian lawyers. All data and all AI stay on the local machine.

## Problem

The repository has an uncommitted upload-and-review UI (SQLite + gpui-kit 0.7) and no commits. The product needs cases, evidence analysis, live Spanish transcription, and discrepancy suggestions.

## Why

The user wants a desktop tool that flags statements in a hearing that conflict with uploaded evidence, with all processing local. Fewer and larger PRs are preferred over atomic PRs to reduce review-bot usage and stack maintenance.

## Decisions (accepted by the user on 2026-10-15)

- Three PRs: `foundation` -> `transcription` -> `analysis`, trunk `main`.
- OCR is in layer 1 (Apple Vision through `objc2-vision`; PDF text and page rendering through `objc2-pdf-kit`). macOS Apple Silicon is the target; 16 GB RAM minimum.
- UI copy in Spanish (ASD-STE100 style: short sentences, active voice). Code, comments, docs, commits in English.
- Speaker role is assigned manually in v1 (toggle "the accused is speaking"). No automatic diarization.
- The LLM runs embedded (`llama-cpp-2`, Metal). No Ollama server.
- STT reuses Handy's GGUF models through `transcribe-cpp` 0.4 (Metal). Primary: Nemotron Streaming 3.5 0.6B. Refine pass: Cohere Transcribe. Models download on demand, never during a hearing.
- Jev is API-only and cannot run locally. Contradiction scoring uses mDeBERTa-v3 multilingual NLI (candle, Metal). Suggestions are quiet, reviewable, and always carry the exact source passage. No credibility scores.
- The GitHub repository `pulgueta/nicolaw` (private) is created after layer 1 is ready. The first commit on `main` is a scaffold only, not the old working tree.
- `cargo clean` runs at the end of each layer to free disk.

## Constraints

- Framework: `gpui-kit = "0.7"` (docs: https://gpui-kit.com/llms.txt). Use components from `gpui_kit::component` and theme tokens. No hard-coded colors.
- Storage: `rusqlite` bundled with FTS5. Database path: `<data_dir>/nicolaw/nicolaw.db`.
- File reads, extraction, OCR, audio, and inference run off the UI thread.
- Strict TDD: observed RED, then GREEN, then refactor. Runner: `cargo test`. Checks per task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`.
- Simplicity first. No speculative abstractions.

## Data model (target)

```
cases(id, name, created_at)
evidence(id, case_id, name, mime, size, sha256, added_at, content BLOB, analysis_status TEXT, analysis_note TEXT)
evidence_pages(evidence_id, page, text, source 'text'|'ocr')
chunks(id, evidence_id, page, idx, text) + chunks_fts (FTS5, unicode61 remove_diacritics 2)
-- layer 2
hearings(id, case_id, title, started_at, ended_at, audio_path)
segments(id, hearing_id, t_start_ms, t_end_ms, text, speaker 'accused'|'other', final INTEGER)
-- layer 3
suggestions(id, hearing_id, segment_id, chunk_id, claim, evidence_quote, kind, nli_contra, explanation, status 'open'|'confirmed'|'dismissed', created_at)
```

## Delivery

- Strategy: `auto-chain`, chain `stacked-to-main` with `gh stack`. Three branches, one PR each. Large PRs are accepted by the user.
- Forecast: layer 1 about 2500 lines, layer 2 about 1800 lines, layer 3 about 2000 lines (additions plus deletions, lockfile excluded).
- Review: receipt-driven development is off (decided by global). No native review runs.

## Tasks

- [x] T0 Scaffold commit on `main` (Cargo.toml with updated deps, .gitignore, README, hello-world main.rs) and `gh stack init foundation`. Route: inline.
- [x] T1 Layer 1 `foundation`: cases, evidence hashing, extraction (PDF text, OCR, images, txt/md, docx), chunking, FTS5 search, Spanish UI with analysis status and search. Route: delegated writer (opus). Commit on `foundation`.
- [ ] T2 Create private GitHub repo, push `main`, `gh stack submit --auto`. Route: inline.
- [ ] T3 Layer 2 `transcription`: hearings, audio capture, VAD, streaming STT with Handy model discovery and download, live transcript, speaker toggle, WAV per hearing. Route: delegated writer (opus).
- [ ] T4 Layer 3 `analysis`: claim extraction, case-scoped retrieval, NLI scoring, LLM explanation, suggestion queue UI. Route: delegated writer (opus).
- [ ] T5 Final: `gh stack submit --auto`, `gh stack view --json` check, `cargo clean`, session summary.

## Progress and evidence

- T0: commit `767fa80` on `main` (chore: scaffold Rust workspace). `gh stack init foundation` created and checked out `foundation`. Working tree (old UI) carried over, uncommitted.

- T1: commits on `foundation`: `a81ff11` feat(app): add file upload and review window (the carried-over upload UI, unchanged, as its own unit), `f256e23` feat(extract): add PDF, OCR, docx, and text extraction (with fixtures), `81c2fa8` feat(app): add cases, evidence analysis, and search. This document is in the next commit (`docs(odd): record layer 1 evidence`).
- T1 authored lines (`git diff --stat main..foundation`): 18 files, 4249 insertions, 1 deletion. Without `Cargo.lock` and fixtures: 4111 insertions. Layer 1 work only (`a81ff11..foundation`, without lockfile and fixtures): 2327 insertions, 211 deletions. The rest is the carried-over upload UI (1995 lines).
- T1 decisions:
  - Status display uses `Tag`, not `Badge`. In gpui-kit 0.7.3, `Badge` is a count or dot overlay on a child element; `Tag` is the text status indicator.
  - The store, analysis, and app changes are one commit: the old app does not compile against the v2 store API.
  - Migration: `PRAGMA user_version` below 2 drops `files` and creates the v2 schema in one transaction. Foreign keys are on; delete removes chunks, pages, then evidence in one transaction (FTS delete trigger runs).
  - Search query: each word without `"` becomes a quoted prefix term (`"word"*`); words without a letter or digit are dropped. Results are scoped by `evidence.case_id` and ordered by `bm25`.
  - At start, evidence with status `pending` or `analyzing` goes back into the analysis queue (an interrupted analysis otherwise stays "Analizando" forever).
  - The queue keeps the case of each added file, so a case switch during a save does not move files to the other case.
  - OCR line order: rows of 1% of the image height, top to bottom, then left to right.
  - The PDF OCR fallback uses `PDFPage::thumbnailOfSize_forBox` at 2x, then `NSImage::TIFFRepresentation`, then `VNImageRequestHandler::initWithData_options`. No CoreGraphics context is needed.
  - Extra fixture `tests/fixtures/scanned.pdf` (image-only PDF made from `scanned.png` with `sips`) tests the PDF OCR path.
- T1 TDD evidence (runner `cargo test`):
  - chunk RED: `error[E0432]: unresolved imports super::Chunk, super::MAX_WORDS, super::MIN_WORDS, super::chunk`. GREEN: 4 passed.
  - extract RED: `error[E0432]: unresolved imports super::ExtractError, super::Page, super::PageSource, super::extract`. GREEN: 13 passed (with chunk), including OCR on `scanned.png` and `scanned.pdf`.
  - store RED: 77 compile errors (`no method named create_case`, `search`, `save_analysis`, `unresolved import super::AnalysisStatus`, ...). GREEN: 20 passed.
  - analysis RED: `error[E0432]: unresolved import super::run`. GREEN: 2 passed.
  - view RED: `no field case_id on type &FilesView`, `no field tab`, `unresolved import super::Tab`. First run: 2 failed (the `search` and `case-name` wrappers needed `.test_support()` to be observable). GREEN: 8 view tests passed.
  - preview label RED: `left: "File" right: "Archivo"`. GREEN: 6 passed.
- T1 checks (final): `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0 (only the dependency note about `block v0.1.6`); `cargo test` 58 passed, 0 failed; `cargo build` exit 0; start check `HOME=/tmp/nicolaw-home ./target/debug/nicolaw` alive after 8 s, stderr 0 bytes, database created.
- T1 not observed: the dialog, select, search, tabs, and OCR in a visible window (only headless tests); a password-protected PDF (code path exists, no fixture); HEIC and WebP images.

## UI copy (layer 1)

| Location | Text |
| --- | --- |
| Window title | Nicolaw |
| Case select placeholder | Seleccione un caso |
| Case select accessible name | Caso |
| New case button | Nuevo caso |
| New case dialog title | Nuevo caso |
| New case name placeholder | Nombre del caso |
| New case dialog confirm | Crear |
| New case dialog cancel | Cancelar |
| Add button | Agregar archivos |
| File dialog button | Agregar |
| Header summary | Guardando <n> de <total> |
| Search placeholder | Buscar en las pruebas… |
| No search hits | No hay resultados para "<query>". |
| Search hit page | Página <n> |
| No cases title | No hay casos |
| No cases description | Cree un caso para agregar pruebas. Seleccione Nuevo caso. |
| Empty case title | No hay pruebas |
| Empty case description | Agregue archivos a este caso. También puede soltar archivos en esta ventana. |
| No selection | Seleccione un archivo para revisarlo. |
| Status: pending | Pendiente |
| Status: analyzing | Analizando |
| Status: ready | Listo |
| Status: not analyzed | Sin analizar |
| Details labels | Tipo, Tamaño, SHA-256, Agregado, Estado, Páginas |
| Pages, none | — |
| Tabs | Vista previa, Texto |
| Text tab, page header | Página <n> (with tag "OCR" when the text comes from OCR) |
| Text tab, empty page | Esta página no tiene texto. |
| Text tab, analysis not done | El análisis no terminó. El texto aparece aquí cuando termine. |
| Text tab, not analyzed | Este archivo no tiene texto para mostrar. (then the note) |
| Text tab, no text found | Nicolaw no encontró texto en este archivo. |
| Note: unsupported type | Tipo de archivo sin análisis de texto |
| Note: unreadable | Nicolaw no puede leer el texto de este archivo. |
| Note: PDF with password | El PDF tiene contraseña. Nicolaw no puede leer el texto. |
| Note: OCR failed | El reconocimiento de texto falló. |
| Note: not macOS | El análisis de PDF e imágenes necesita macOS. |
| Preview, loading | Cargando · <n>% |
| No preview | No hay vista previa para este tipo de archivo. Seleccione Abrir para ver el archivo. |
| Open button | Abrir |
| Delete button | Eliminar |
| Delete dialog title | ¿Eliminar este archivo? |
| Delete dialog description | Nicolaw eliminará "<name>" y su texto. No puede deshacer esta acción. |
| Delete dialog confirm / cancel | Eliminar / Cancelar |
| Transfer row, waiting | En espera |
| Transfer row, saving | Guardando · <n>% |
| Added notification | Se agregó 1 archivo. / Se agregaron <n> archivos. |
| Too large error | "<name>" pesa más de 100 MB. Nicolaw no agregó el archivo. |
| Read error | Nicolaw no puede leer "<name>". |
| Storage error | Nicolaw no puede guardar el archivo. Intente de nuevo. |
| List error | Nicolaw no puede leer las pruebas. |
| Delete error | Nicolaw no puede eliminar el archivo. Intente de nuevo. |
| Open error | Nicolaw no puede abrir "<name>". |
| Case error | Nicolaw no puede crear el caso. Intente de nuevo. |
| Search error | Nicolaw no puede buscar. Intente de nuevo. |
| Add without a case | Cree un caso antes de agregar archivos. |
| Type label fallback | Archivo |

## Next step

T2: manual check of layer 1 in a visible window (case dialog, select, drag and drop, OCR of a real scan, search), then create the private GitHub repository and submit the stack.
