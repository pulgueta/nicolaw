# Feature: qa-swarm-round-1

Locator: `odd/tasks/qa-swarm-round-1.md`
Engram mirror: `odd/qa-swarm-round-1/tasks`

## Objective

Test the full app (stack top `classification`) on this MacBook Pro (Apple Silicon, 16 GB RAM) with a Codex swarm, collect UI, logic, performance, and AI issues, and fix them in the correct stack layer.

## Method

- Testers: `gpt-6.1-sol` through T3 `delegate_task`, one at a time, each on one screen of the app, driving the release binary with `screencapture` and `osascript` (System Events). Each tester writes a report file under `qa/round-1/` (untracked scratch, deleted at the end).
- Reviewer: `gpt-6-astra` reads the reports and the code, gives advanced feedback with concrete examples and priorities (UI design, ease of use, app flow, AI implementation on 16 GB).
- Orchestrator (Fable) routes each accepted fix to the stack layer that owns the code, with TDD, then rebases upstack and pushes.
- `cargo clean` after the release build is no longer needed and after each fix batch.

## Constraints

Same as `nicolaw-v1-stack`. Fixes keep Spanish UI copy, English code. No new product scope without the user's authorization.

## Tasks

- [x] T1 Release build and tester sessions: Pruebas (cases, import, OCR, search, preview, delete); Audiencias (hearing, models, recording, speaker switch, finalize, transcript); Análisis (switch, model load, suggestions, confirm/dismiss, latency, memory). Route: delegated Codex testers.
- [x] T2 Astra review of the reports with examples and priorities. Route: delegated Codex reviewer.
- [ ] T3 Fix batch by layer (writer per layer, bottom-up), rebase, push, replies. Route: delegated writers.
- [ ] T4 Clean up scratch files, `cargo clean`, summary. Route: inline.

## Progress and evidence

- T1: release build 10d0fdc wrapped as qa/Nicolaw.app (Info.plist with NSMicrophoneUsageDescription, ad-hoc signed) after the bare binary was denied the microphone. Reports: qa/round-1/pruebas-report.md (4 findings), audiencias-report.md (6 findings over two runs), analisis-report.md (3 findings). Orchestrator diagnostic qa/MicTest.app: handy-recorder gives non-zero samples from the AirPods and the built-in microphone; the app's zero WAVs are app-side (device selection or AirPods idle link).
- T2: qa/round-1/astra-review.md (gpt-6-astra): NO-GO verdict, F01–F19 with owners, UI layout spec for 1280x820 and 900x600, 16 GB memory budget, calibration plan, round 2 checklist.

## Fix batch decisions (orchestrator, 2026-10-15)

- Route: one writer per layer, bottom-up: foundation (F01 bundle script and menu/Quit action, F09 grouped search, F13 locator truthfulness, F14 deletion of temp copies, F16 source row, F17 validation copy and case identity, top navigation), transcription (F02 capture health, F03 control bounds and responsive layout, F04 safe shutdown, F05 storage worker, F06 speaker marker, F08 refine finish, F12 model repair, F15 live strip, F18 follow-live scroll, F19 model panel, microphone default and resolved name, downloads blocked while live, failed segment marker, capture failure state), analysis (F03 panel breakpoint and drawer, F07 revisions, F11 per-segment analysis state, F06 re-analysis, stop_review on Finalizar, question gap), classification (F10 copy "Puntuación del modelo; no calibrada", wider real tests, calibration plan in README).
- Accepted Astra recommendations that change current behavior: Cohere refinement is not loaded during live capture; it becomes an explicit post-hearing action. The decision label stays visible (accepted in jev-style-decision) with the uncalibrated-score wording.
- Not done in this round: transcript FTS, app-level encryption, semantic retrieval. Recorded as limits.
