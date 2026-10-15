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
- The LLM runs embedded (`llama-cpp-2`, Metal). No Ollama server. (Changed in T4: candle, see the T4 decisions.)
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
suggestions(id, hearing_id, segment_id, chunk_id, claim, evidence_quote, claim_quote, kind, level 'conflict'|'check', nli_contradiction REAL, explanation, status 'open'|'confirmed'|'dismissed', created_at)
```

## Delivery

- Strategy: `auto-chain`, chain `stacked-to-main` with `gh stack`. Three branches, one PR each. Large PRs are accepted by the user.
- Forecast: layer 1 about 2500 lines, layer 2 about 1800 lines, layer 3 about 2000 lines (additions plus deletions, lockfile excluded).
- Review: receipt-driven development is off (decided by global). No native review runs.

## Tasks

- [x] T0 Scaffold commit on `main` (Cargo.toml with updated deps, .gitignore, README, hello-world main.rs) and `gh stack init foundation`. Route: inline.
- [x] T1 Layer 1 `foundation`: cases, evidence hashing, extraction (PDF text, OCR, images, txt/md, docx), chunking, FTS5 search, Spanish UI with analysis status and search. Route: delegated writer (opus). Commit on `foundation`.
- [x] T2 Create private GitHub repo, push `main`, `gh stack submit --auto`. Route: inline.
- [x] T3 Layer 2 `transcription`: hearings, audio capture, VAD, streaming STT with Handy model discovery and download, live transcript, speaker toggle, WAV per hearing. Route: delegated writer (opus).
- [x] T4 Layer 3 `analysis`: claim extraction, case-scoped retrieval, NLI scoring, LLM explanation, suggestion queue UI. Route: delegated writer (opus).
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

- T2: repository https://github.com/pulgueta/nicolaw created (private); `main` pushed; `gh stack submit --auto` created PR #1 for `foundation`. Parent spot check: `cargo fmt --check` exit 0, tree clean, no DEBUG prints; full `cargo test` rerun in progress.

- T3: commits on `transcription`: `ca0b6d0` feat(store): add hearings and segments, `e0a7c84` feat(models): discover and download Handy GGUF models, `89a4724` feat(audio): add capture, VAD segmenter, and streaming transcription, `5718d3e` feat(app): add hearing screen with live transcript. This document is in the next commit (`docs(odd): record layer 2 evidence`).
- T3 authored lines (`git diff --stat foundation..transcription`, before this document): 13 files, 4723 insertions, 23 deletions. Without `Cargo.lock`: 12 files, 4198 insertions, 23 deletions. Largest files: `src/hearings.rs` 1677 (about 350 of it view tests), `src/audio/pipeline.rs` 592 (about 280 tests), `src/audio/native.rs` 477, `src/store.rs` +398.
- T3 modules: `src/models.rs` (catalog, discovery, download), `src/audio/segmenter.rs` (pure VAD state machine), `src/audio/pipeline.rs` (synchronous `Processor`: WAV, level, VAD, segments, live text, storage; `refine`), `src/audio/native.rs` (`handy-recorder` capture, AVFoundation permission request, Silero VAD and `transcribe-cpp` transcriber), `src/audio/fake.rs` (test fakes), `src/hearings.rs` (Audiencias screen). `src/app.rs` has the top-level tabs Pruebas / Audiencias.
- T3 decisions that differ from the brief, or add to it:
  - Dependencies: `handy-recorder` 0.1 fits (16 kHz mono f32, fixed 512-sample chunks = one Silero frame, permission status), so no direct `cpal`/`rubato`/`rtrb`. It does not prompt, so Nicolaw calls `AVCaptureDevice::requestAccessForMediaType` (`objc2-av-foundation` + `block2`) when the status is not determined. `hf-hub` is 0.4.3 (`ureq`, sync API); 1.0 is async-first and pulls reqwest, tokio, and xet.
  - The processor is synchronous and runs in gpui background tasks (one for the processor, one for the refine pass), not in dedicated pipeline threads. Reason: headless view tests stay deterministic on the gpui test executor. The native transcriber owns its `Session` on one dedicated thread, because a `transcribe-cpp` `Stream` borrows its `Session`.
  - `Transcriber` is `begin` / `feed` (returns new `Partial { committed, tentative }`) / `finish`. `Event::Partial` also carries `speaker` and `t_start_ms`; `Event::Final` carries the stored `Segment`; `Event::Refined` reports the refine pass. Input is `Frame(Vec<f32>)` / `Pause` / `Failed`.
  - A segment row is inserted (`final = 0`) at speech start, with the speaker of the switch at that moment; it is finished at the end. A segment with no words is deleted (noise).
  - Segmenter: pre-roll keeps whole frames until at least 300 ms (10 frames = 320 ms); 600 ms of silence ends a segment (19 frames); 30 s maximum. `t_end_ms` is the end of the last speech frame. Pause flushes the segment and clears the pre-roll. Times are positions in the WAV (paused time is not in the WAV).
  - Cohere Transcribe can also be the live model (it is the only model on this machine). It is not a streaming model, so its text shows when the segment ends. The refine pass runs only when the live model is not Cohere and Cohere is installed.
  - The Silero VAD GGUF comes from `handy-computer/silero-vad-v6.2-gguf` (`silero-vad-v6.2-F32.gguf`, 1.2 MB). Handy bundles it in the app; the installed Handy.app on this machine still has `silero_vad_v4.onnx`. "Descargar" downloads every missing required model: Nemotron (Q4_K_M, as in the brief; Handy's default is Q8_0) and the VAD. The VAD runs on the CPU.
  - Downloads use Handy's pinned revisions. Discovery prefers the pinned snapshot and the download quant, accepts any quant, and ignores dead snapshot links.
  - WAV: 16-bit PCM, header flushed every 10 s. A hearing records one time: after Finalizar, or after an interruption, it is read-only (no append to the WAV).
  - `transcribe_cpp::disable_logging()` keeps stderr empty. `NativeTranscriber` joins its thread on drop: a Metal model that is still loaded at process exit makes ggml abort (`ggml-metal-device.m:1040 GGML_ASSERT([rsets->data count] == 0)`, observed before the fix). An `on_app_quit` hook ends a live recording like Finalizar and waits up to 10 s.
  - Icons: the component icon set has no `Download`, `CircleStop`, or `ArrowLeftRight`, so the view uses `ArrowDown`, `Square`, and `RefreshCw`.
  - Default hearing title: `Audiencia DD/MM/YYYY` (local date from SQLite, Colombian order).
- T3 TDD evidence (runner `cargo test`):
  - store RED: 37 compile errors (`unresolved import super::Speaker`, `no method named create_hearing`, `hearings`, `insert_segment`, ...). GREEN: 27 passed. `today` RED: `no method named today`; GREEN: 1 passed.
  - models RED: `unresolved imports super::CATALOG, super::COHERE, super::ModelDirs, ...`. First GREEN compile failed in the test helper (`expected &str, found String`); helper fixed; GREEN: 9 passed.
  - segmenter RED: `unresolved imports super::Output, super::Segmenter`. GREEN: 8 passed.
  - pipeline RED: `unresolved imports super::Event, super::Input, super::Processor, super::RefineJob, super::refine` and `super::AudioError, ...`. First run: 7 passed, 1 failed (the test expected a final at `finish`, but `Pause` had already closed the segment: test error, fixed). GREEN: 8 passed.
  - hearings view RED: `unresolved imports super::HearingsEnv, super::HearingsView, super::format_time, super::speaker_label`. First compile: 3 missing icons. GREEN: 7 passed.
  - navigation RED: `E0061 this function takes 3 arguments but 4 arguments were supplied`. GREEN: full suite 99 passed, 2 ignored.
- T3 checks (final tree `5718d3e`): `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0 (only the dependency note about `block v0.1.6`); `cargo test` 99 passed, 0 failed, 2 ignored; `cargo build` exit 0; start check `HOME=/tmp/nicolaw-home ./target/debug/nicolaw` alive after 8 s, stderr 0 bytes, database `user_version` 3 with `hearings` and `segments`.
- T3 per-commit checks: each commit was checked out alone in a temporary worktree: `ca0b6d0` store tests 28 passed; `e0a7c84` models tests 9 passed; `89a4724` audio tests 16 passed, 2 ignored; `5718d3e` build exit 0. The first three commits have dead-code warnings in the non-test build (their API is used by later commits), so `clippy -D warnings` passes on the final tree only.
- T3 real-model checks (`#[ignore]`, `cargo test -- --ignored --nocapture real_`): Cohere Transcribe Q5_K_M from Handy's cache, Metal. Paulina (es_MX): "Buenos días. La audiencia empieza a las nueve de la mañana. El acusado dice que no estaba en Bogotá el 12 de marzo." (exact). Monica (es_ES), same text: "Buenos días, la audiencia en Pisa a las nueve de la mañana, ..." ("empieza" misheard); Monica, short sentence only: "La Odincha en Pisa a las nueve de la mañana." Silero VAD downloaded into a temporary cache (157 progress calls, last 1241056/1241056), 222 speech frames in the Paulina WAV, 0 in 30 silent frames.
- T3 not observed: a real microphone and the macOS permission prompt; Nemotron Streaming (not installed, so the streaming and partial-text path of `NativeTranscriber` ran with the fake only); the download of Nemotron; the refine pass with real models; the quit hook; the Audiencias screen in a visible window (headless tests only).

- T4: commits on `analysis`: `93147c9` feat(store): add suggestions, `3ae65cc` feat(models): add NLI and LLM catalog entries, `daf0bb0` feat(review): add claim extraction, NLI scoring, and explanation worker, `28a4fb0` feat(app): add suggestions panel, `b44245b` test(review): add real pipeline check. This document is in the next commit (`docs(odd): record layer 3 evidence`).
- T4 authored lines (`git diff --stat transcription..analysis`, before this document): 15 files, 4903 insertions, 64 deletions. Without `Cargo.lock`: 14 files, 4022 insertions, 51 deletions. Largest: `src/hearings.rs` +1068 (about 420 of it view tests), `src/store.rs` +676 (about 330 tests), `src/review/analyzer.rs` 601 (about 330 tests), `src/review/native.rs` 520 (about 230 ignored real-model tests).
- T4 modules: `src/review/mod.rs` (traits `Llm`, `Nli`, `ReviewBackend`; `ReviewEvent`; thresholds and `decide`), `query.rs` (search terms of a claim), `parse.rs` (claims JSON, explanation JSON with exact-substring checks, premise windows), `prompt.rs` (Qwen3 chat prompts, untrusted data in tagged blocks, JSON end), `analyzer.rs` (`Analyzer`, `JobQueue`, `run`), `native.rs` (candle engines, one thread per model), `fake.rs` (test fakes). `src/analysis.rs` (evidence extraction queue) is unchanged.
- T4 decisions that differ from the brief, or add to it:
  - **LLM runtime: candle, not `llama-cpp-2`.** Observed with `llama-cpp-2` 0.1.159 (`metal`, `dynamic-link`): the binary links `@rpath/libllama.0.dylib` and `libggml*.0.dylib` but has no `LC_RPATH`, so it does not start (`dyld: Library not loaded: @rpath/libggml-base.0.dylib`). The fix needs a build script (outside the authorized edit surfaces). Also, transcribe-cpp links a static ggml 0.26 into the same binary (llama.cpp 0.1.159 has ggml 0.24); `nm -m` showed the Rust-side ggml symbols bound to the static 0.26 copy, an ABI risk. A static llama build would put two ggml versions in one link unit. candle runs both models with no second ggml and no dylibs. To return to llama.cpp later: add `build.rs` with `cargo:rustc-link-arg=-Wl,-rpath,...` and ship the dylibs in the app bundle, and keep the Rust code away from direct `ggml_*` calls.
  - **LLM model: `Qwen/Qwen3-4B-GGUF`, file `Qwen3-4B-Q4_K_M.gguf` (2497 MB), revision `bc640142c66e1fdd12af0bd68f40445458f3869b`, tokenizer from `Qwen/Qwen3-4B` revision `1cfa9a7208912126459214e8b04321603b3df60c` (the GGUF tokenizer is not readable by `tokenizers`).** Official Qwen GGUF, Apache 2.0. candle 0.11 has `quantized_qwen3` but no loader for Gemma 4 E4B or Qwen3.5 (new architectures). An official `Qwen/Qwen3-4B-Instruct-2507-GGUF` repo does not exist (HF API: not found). Thinking is off: the prompt ends with an empty `<think></think>` block, as the Qwen3 template does for `enable_thinking=False`.
  - **No grammar-constrained decoding** (candle has none). Instead: the answer starts with `[` or `{` in the prompt, generation stops when the first JSON value is complete (`json_end`), temperature 0.1 with a fixed seed, and `parse.rs` validates the output (claims: array of strings, at most 5; explanation: all keys, known kind, exact substrings). Invalid output gives no claims, or a `check` with the note "Nicolaw no puede verificar la explicación. Lea el pasaje de la prueba."
  - **NLI on Metal works** (no CPU fallback): `MoritzLaurer/mDeBERTa-v3-base-xnli-multilingual-nli-2mil7` revision `b5113eb38ab63efdd7f280f8c144ea8b13f978ce` (config, tokenizer.json, model.safetensors). The safetensors file is float16 on disk; Nicolaw loads it as float32. Tokenizer truncation at 512 tokens.
  - candle and tokenizers get `opt-level = 3` in the dev profile (like the gpui crates). Before: two NLI pairs took 8.5 s in the debug build; after: 121 ms, and 38 ms for one warm pair.
  - Retrieval adds `Store::search_any` (OR of the terms, BM25 order). The existing `Store::search` joins terms with AND, which rarely matches all words of a claim. Both share `fts_query`, so all input stays FTS-safe.
  - Thresholds (`src/review/mod.rs`, initial values to tune on real data): conflict when P(contradiction) >= 0.85 and P(contradiction) - P(entailment) >= 0.5; check when P(contradiction) >= 0.60. Both directions are scored; each label keeps its maximum. A pair at >= 0.85 that fails the margin is a check.
  - At most 2 suggestions per claim (strongest first, one per chunk), so that the queue stays quiet and the LLM explains few pairs. Long chunks split into premises of at most 150 words on sentence boundaries; each premise is an exact substring of the chunk.
  - A conflict needs NLI level `conflict` and the LLM `contradiction: true`. All other flagged pairs are `check`.
  - Jobs carry a hash of the segment text. The analyzer reads the segment from the store and skips it when the speaker is not `accused`, the text changed (refine pass), or the segment is gone; it checks again before each insert. The view enqueues accused finals and every refined text (the analyzer filters the speaker). The question context is `Store::question_before` (the last final `other` segment before the answer).
  - Queue: `JobQueue` (bounded, 32). When full, the new job replaces the oldest and a counter grows. `ReviewEvent::Progress` has `pending` and `dropped` (the brief had `pending` only); the panel shows the dropped count.
  - Models: `Role` gained `Nli` and `Llm` (the brief's "kinds"). Analysis models are `AnalysisModel` entries with several `RepoFile`s at pinned revisions; installed only when all files are in the cache snapshot. `download_files` skips files that are already there (hf-hub 0.4 `download_with_progress` always downloads).
  - The analysis models load when a recording starts with "Analizar en vivo" on (the queue keeps segments while they load), and stay loaded until the switch goes off or the app quits. Each model has its own thread (`ModelThread`), joined on drop. ggml is not involved, so the layer 2 abort at exit does not apply; at quit, the queue closes and the review task is dropped.
  - Deleting evidence deletes the suggestions of its chunks (a suggestion without its source passage has no meaning). Deleting a hearing deletes its suggestions.
  - Filter default: "Abiertas" during the live recording of the hearing, "Todas" for other hearings and after Finalizar.
  - The source link emits `OpenEvidence { evidence_id, page }`; the app switches to Pruebas and reuses the hit -> page navigation (`open_page`).
  - Test harness note: in the live view test, the speaker switch is set through state, because a click on the Start button right after a click on a Switch did not reach the button.
- T4 TDD evidence (runner `cargo test`):
  - store RED: 26 compile errors (`unresolved imports super::NewSuggestion, ...`, `no method named insert_suggestion`, `suggestions`, `chunk`, `segment`, `question_before`, `search_any`, ...). GREEN: 37 passed. Then a v3 -> v4 migration test (passed at once: regression guard, no RED).
  - models RED: `unresolved imports super::ANALYSIS_MODELS, super::LLM, super::NLI, super::find_files`, `no variant Role::Nli / Role::Llm`. GREEN: 12 passed.
  - query RED: `unresolved import super::query_terms`. GREEN: 3 passed.
  - decide and parse RED: `unresolved imports super::NliScores, super::decide` and `super::Explanation, super::MAX_CLAIMS, ...`. GREEN: 23 review tests passed.
  - analyzer and queue RED: 4 compile errors (`super::Analyzer, super::Job, super::JobQueue, super::text_hash`, `super::Engines, super::Llm, ...`, `UNVERIFIED_NOTE`). GREEN: 33 review tests passed.
  - prompt RED: `unresolved imports super::claims_prompt, super::explain_prompt, super::json_end`. First run: 3 passed, 1 failed (test error: the system prompt names `<pregunta>`; the test now checks the block `<pregunta>\n`). GREEN: 37 review tests passed.
  - view RED: 13 compile errors (`unresolved import super::Filter`, `HearingsEnv has no field review`, `no method apply_review`, `no field counts`, ...). First runs: 6 failed. Causes: the panel id had no `.test_support()`; the filter `Select` grew to 470 px in the column, so the card buttons were outside the window (fixed with a `flex_none` wrapper); a rediscovery loop (`start_review` -> `ModelsMissing` -> discovery -> `start_review`) hung the suite while a recording ran without analysis models (fixed: `start_review` returns quietly when files are missing; `ModelsMissing` comes only when the files are gone at load time). GREEN: 149 passed, 0 failed, 7 ignored.
- T4 checks (final tree `b44245b`): `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0 (only the dependency note about `block v0.1.6`); `cargo test` 149 passed, 0 failed, 7 ignored; `cargo build` exit 0; start check `HOME=/tmp/nicolaw-home ./target/debug/nicolaw` alive after 8 s, stderr 0 bytes, database `user_version` 4 with `suggestions`.
- T4 real-model checks (`#[ignore]`, models downloaded into `~/.cache/huggingface/hub` with `real_analysis_models_download`, which uses `models::download_files`):
  - `real_nli`: device Metal. "El 12 de marzo el acusado viajó a Medellín y no estuvo en Bogotá." vs "El acusado estaba en Bogotá el 12 de marzo.": evidence -> claim entailment 0.0018, neutral 0.0019, contradiction 0.9963; claim -> evidence entailment 0.0025, neutral 0.0040, contradiction 0.9935. Unrelated pair: contradiction 0.0004, neutral 0.9973. Load 1.7 s.
  - `real_llm` (Metal): question "¿Dónde estaba usted el 12 de marzo?", statement "Ese día yo estaba en Bogotá, en la casa de mi hermana, y le pagué dos millones de pesos." -> `[ "El acusado estaba en Bogotá el 12 de marzo.", "El acusado estaba en la casa de su hermana el 12 de marzo.", "El acusado le pagó dos millones de pesos el 12 de marzo." ]` in 5.6 s. Explanation for the first claim against a hotel passage: valid JSON, `contradiction: true`, kind `location`, quotes pass the exact-substring check, 8.8 s.
  - `real_pipeline` (full analyzer, both models): one answer -> 1 `conflict` (location, P=0.875, "registro-hotel.txt", página 1) and 2 `check` (P=0.805 and 0.722, página 2), in 66 s.
  - `real_backend`: both models load through `NativeReview` and the threads stop on drop.
- T4 not observed: the panel in a visible window (headless tests only); a real hearing with live analysis; the Descargar buttons of the panel against the network (the download code path ran through the ignored test); analysis speed in a release build.
- T4 open issues:
  - Speed: about 66 s for one answer with 2 claims and 3 explanations (debug build, Metal). In a long hearing the queue can drop old answers (counted and shown). Options: fewer explanations, a smaller LLM for claims, or a release build check.
  - The LLM often quotes the full claim as `quote_claim` (valid, but not precise). Prompt tuning on real data.
  - A speaker correction after the fact (toggle to Acusado) does not enqueue the segment.
  - The thresholds are initial values.

## UI copy (layer 2)

| Location | Text |
| --- | --- |
| Screen tabs | Pruebas, Audiencias |
| New hearing button / dialog title | Nueva audiencia |
| New hearing title placeholder | Título de la audiencia |
| New hearing default title | Audiencia <DD/MM/YYYY> |
| New hearing confirm / cancel | Crear / Cancelar |
| Hearing row, not started | Sin iniciar |
| Hearing row tags | En curso, Finalizada |
| No hearings | No hay audiencias. Seleccione Nueva audiencia. |
| No selection | Seleccione una audiencia. |
| Delete button | Eliminar |
| Delete dialog title | ¿Eliminar esta audiencia? |
| Delete dialog description | Nicolaw eliminará "<title>", su transcripción y su audio. No puede deshacer esta acción. |
| Delete dialog confirm / cancel | Eliminar / Cancelar |
| Model select placeholder / accessible name | Sin modelo instalado / Modelo de voz |
| Microphone select placeholder / accessible name | Sin micrófono / Micrófono |
| Recording buttons | Iniciar, Pausar, Reanudar, Finalizar |
| Download button | Descargar |
| Download details | <model names, joined with " y "> · <n> MB |
| Download progress accessible name | Descarga |
| Speaker switch | Habla el acusado |
| Mic level accessible name | Nivel del micrófono |
| Speaker tags | Acusado, Otro |
| Speaker toggle tooltip | Cambiar hablante |
| Ended hearing | Grabación finalizada: <ended_at>. |
| Interrupted hearing | La grabación se detuvo antes de Finalizar. |
| Transcript, not started | Seleccione Iniciar para grabar y transcribir la audiencia. |
| Transcript, recording, no text yet | Nicolaw escucha. El texto aparece aquí cuando alguien habla. |
| Transcript, ended, no text | Esta audiencia no tiene transcripción. |
| No model installed | No hay un modelo de voz instalado. Seleccione Descargar para instalarlo. |
| No microphone | Nicolaw no encuentra un micrófono. Conecte un micrófono y seleccione Iniciar. |
| Microphone permission denied | Nicolaw no tiene permiso para usar el micrófono. Abra Configuración del Sistema > Privacidad y seguridad > Micrófono y active el permiso. |
| Model busy | El modelo de voz está en uso. Finalice la otra audiencia y seleccione Iniciar. |
| Model load failed | Nicolaw no puede cargar el modelo de voz. Descargue el modelo de nuevo. |
| Microphone failed | El micrófono dejó de funcionar. Nicolaw guardó la transcripción hasta este punto. |
| Transcription failed | La transcripción de un segmento falló. |
| Storage error | Nicolaw no puede guardar la transcripción. |
| Audio file error | Nicolaw no puede guardar el audio de la audiencia. |
| Download failed | La descarga falló. Revise la conexión a internet y seleccione Descargar. |
| List error | Nicolaw no puede leer las audiencias. |
| Create error | Nicolaw no puede crear la audiencia. Intente de nuevo. |
| Delete error | Nicolaw no puede eliminar la audiencia. Intente de nuevo. |
| Speaker change error | Nicolaw no puede cambiar el hablante. Intente de nuevo. |

## UI copy (layer 3)

| Location | Text |
| --- | --- |
| Panel title | Sugerencias |
| Counters (tags) | Conflictos: <n>, Revisar: <n> |
| Live analysis switch | Analizar en vivo |
| Missing models | Faltan los modelos de análisis. Nicolaw graba y transcribe la audiencia sin ellos. |
| Model row | <model name> · <n> MB, with button Descargar |
| Download progress accessible name | Descarga |
| Models loading | Nicolaw carga los modelos de análisis. |
| Queue | Declaraciones en análisis: <n> |
| Dropped jobs | Nicolaw no analizó <n> declaraciones. Había demasiadas en espera. |
| Filter accessible name | Filtro de sugerencias |
| Filter options | Abiertas, Confirmadas, Descartadas, Todas |
| Empty panel | No hay sugerencias. Nicolaw analiza las declaraciones del acusado cuando terminan. |
| Empty filter | No hay sugerencias con este filtro. |
| Card level tags | Posible conflicto, Revisar |
| Card kinds | Fecha, Cantidad, Identidad, Lugar, Secuencia, Otro |
| Card labels | Declaración, Prueba |
| Card evidence quote | «<quote>» |
| Card source | <file name> · Página <n> (file name opens the evidence in Pruebas at the page) |
| Card buttons | Confirmar, Descartar |
| Card status tags | Confirmada, Descartada |
| Unverified explanation | Nicolaw no puede verificar la explicación. Lea el pasaje de la prueba. |
| Decision error | Nicolaw no puede guardar la decisión. Intente de nuevo. |
| Model load error | Nicolaw no puede cargar los modelos de análisis. Descargue los modelos de nuevo. |
| Analysis error | Nicolaw no puede analizar una declaración del acusado. |
| Storage error | Nicolaw no puede guardar las sugerencias. |

## Next step

T4 manual check in a visible window: download the analysis models with Descargar in the Sugerencias panel, record a hearing with the speaker switch on, and check the cards, the source link, Confirmar / Descartar, and the filter. Then T5 (`gh stack submit --auto`, `gh stack view --json`, `cargo clean`, session summary). The T3 manual check is still open.
