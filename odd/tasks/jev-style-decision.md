# Feature: jev-style-decision

Locator: `odd/tasks/jev-style-decision.md`
Engram mirror: `odd/jev-style-decision/tasks`

## Objective

Add a local Jev-style decision step to the discrepancy analysis, as layer 4 of the stack (`classification` on top of `analysis`, PR against `analysis`).

## Problem

The user asked for Jev (TypeSafe AI) as the classifier. Jev is proprietary and API-only. The app must run fully local. Layer 3 uses mDeBERTa NLI for scoring and the LLM `explain` JSON for a yes/no agreement. The yes/no in generated JSON has no probability and no abstain option.

## Research (2026-10-15, primary sources)

- Jev: discriminative "System One" API (`state` + typed questions `noul`/`choice`/`score`, each option with a description, probabilities per option). Weights not published. Source: https://developers.cloudflare.com/ai/models/typesafe/jev/
- Cloudflare Clef (27B) and Clef-flash (9B), 2026-10-01, Apache-2.0: same System One API with a joint schema head. Not loadable in candle (no `qwen3_5` module, no head). Published language list says English only. Sources: https://developers.cloudflare.com/changelog/post/2026-10-01-clef-workers-ai/ , https://huggingface.co/Cloudflare/clef-flash
- Other candidates (Julia-1, Kev-4B, OpenJev, Horizon-Labs zeroshot, GLiClass) have no Spanish NLI result, no candle support, or a non-commercial license.
- mDeBERTa-v3 multilingual NLI stays the primary scorer: XNLI-es 0.832, MIT, 3-way labels, candle `debertav2`.

## Decision

- Keep mDeBERTa as the first stage. Add a second stage that follows Jev's `choice` question shape, run on the Qwen3-4B that candle already loads:
  - State: the evidence passage and the claim. Question: one `choice` with four described options: `contradicts`, `consistent`, `unrelated`, `insufficient` (abstain).
  - No text generation. One forward pass over the prompt that ends where the answer starts; read the logit of the first token of each option label; softmax over only those four logits. This is how Jev and Clef produce probabilities (one logit per option).
  - Cascade: the decision runs only on pairs that mDeBERTa already flagged. A pair is a `conflict` only when NLI says conflict, the decision is `contradicts` with probability at or above `DECISION_MIN` (0.60 initial, tune on real data), and the explanation JSON agrees. Otherwise it is a `check`.
  - The decision label and probability are stored with the suggestion and shown in the panel, so the lawyer sees `insuficiente` as an abstain.
- Clef-flash through an MLX or llama.cpp sidecar is not added: no Rust path, English-only language list, no Spanish evaluation.

## Constraints

- Same as `nicolaw-v1-stack`: gpui-kit 0.7.3 components, Spanish UI copy, English code and docs, strict TDD with `cargo test`, checks `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`. `cargo clean` at the end.
- Evidence and transcript text are data in prompts, never instructions. Reuse the `RESERVED` guard in `src/review/prompt.rs`.
- No new model download. No new crate unless candle needs one.

## Delivery

- Strategy: `auto-chain`, `stacked-to-main`. One branch `classification`, one PR with base `analysis`.
- Forecast: about 700 authored changed lines.

## Tasks

- [x] T1 `Decision` trait, `DecisionScores`, fake, `decide_with` rule (NLI level + decision), thresholds, store schema v5 (`decision TEXT`, `decision_p REAL` on `suggestions`), analyzer cascade. Route: delegated writer (opus).
- [x] T2 Native option-logit scoring on the loaded Qwen3 in `src/review/native.rs` (shared model thread), prompt in `src/review/prompt.rs`, ignored real-model test. Same writer.
- [x] T3 Panel shows the decision label and probability in Spanish; README "Analysis" section updated with the Jev comparison summary. Same writer.
- [ ] T4 Spot check, `gh stack submit --auto --open`, `cargo clean`, PR body with research summary. Route: inline.

## Progress and evidence

- Branch `classification` created with `gh stack add classification` on top of `analysis`.
- T1 done: `851cb3d feat(review): add jev-style decision stage`. `Decider` trait, `Decision`, `DecisionScores::best`, `DECISION_MIN = 0.60`, `confirm`, `FakeDecider`, analyzer cascade (NLI flag, then decision, then explanation), schema v5 (`decision TEXT`, `decision_p REAL`, nullable) with a v4 migration test. `src/app.rs` test literal added to the surface with approval. RED observed for `confirm` (2 tests), the store round trip (1 test), and the analyzer cascade (3 tests); then GREEN.
- T2 done: `d384fe1 feat(review): score decisions by option logits on qwen3`. `decision_prompt` returns `String` (accepted deviation: the `RESERVED` guard removes text and cannot fail). The prompt ends with `Decision:`; the labels with a leading space start with distinct tokens (` contr` 6027, ` consistent` 12966, ` unrelated` 45205, ` insufficient` 38313); load fails if two first tokens are equal. One Qwen thread is shared through `Arc<ModelThread<Qwen>>`. RED observed for the prompt test and the softmax test; then GREEN.
- T2 real model (`cargo test real_decider -- --ignored --nocapture`, Metal): contradiction pair best `Contradicts` p=1.0; consistent pair best `Consistent` p=1.0; unrelated pair best `Unrelated` p=0.9999996; about 0.7 s each. The probabilities are saturated (not calibrated); tune `DECISION_MIN` on labeled data. `real_pipeline` passed: one `Conflict` with `Contradicts` p=0.999, one `Check` with `Consistent` p=0.979. `real_backend` passed with the shared thread.
- T3 done: `9b15dc8 feat(app): show decision in suggestions panel`. Spanish `Tag` under the level row (`Contradice · 92 %`, `Compatible`, `No relacionada`, `Insuficiente`), nothing when NULL; README "Analysis" with step 4 and "Why not Jev". RED observed for the label test and the view test; then GREEN.
- Checks after T3: `cargo fmt --check` exit 0; `cargo clippy --all-targets -- -D warnings` exit 0; `cargo test` 163 passed, 0 failed, 8 ignored; `cargo build` exit 0.
- Next step: T4 (parent): spot check, `gh stack submit --auto --open`, `cargo clean`, PR body.
