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

- [ ] T1 Release build and tester sessions: Pruebas (cases, import, OCR, search, preview, delete); Audiencias (hearing, models, recording, speaker switch, finalize, transcript); Análisis (switch, model load, suggestions, confirm/dismiss, latency, memory). Route: delegated Codex testers.
- [ ] T2 Astra review of the reports with examples and priorities. Route: delegated Codex reviewer.
- [ ] T3 Fix batch by layer (writer per layer, bottom-up), rebase, push, replies. Route: delegated writers.
- [ ] T4 Clean up scratch files, `cargo clean`, summary. Route: inline.

## Progress and evidence

