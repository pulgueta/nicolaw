# Nicolaw

Nicolaw is a desktop assistant for lawyers in Colombian court hearings. It stores case evidence locally, transcribes speech locally, and suggests statements that may conflict with the evidence. All data and all AI models stay on the local machine.

## Build

```sh
cargo build
cargo test
```

Requires macOS on Apple Silicon.

## Models

Nicolaw transcribes hearings on this machine with [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) (Metal). It uses the GGUF models of [Handy](https://github.com/cjpais/Handy):

| Model | Use | Download |
| --- | --- | --- |
| Nemotron Streaming 3.5 0.6B | Live Spanish transcription (primary) | Q4_K_M, about 500 MB |
| Cohere Transcribe | Optional refine pass after the hearing, more accurate. Not a streaming model: it can also be the live model, but then the text shows only when a segment ends. | Q5_K_M, about 1.8 GB, from **Modelos** |
| Silero VAD v6.2 | Finds the start and the end of speech | F32, about 1 MB |

Other Handy models (Parakeet EN, Voxtral, and the English-only models) are not eligible: they have no Spanish, or they are too slow for a live hearing.

Nicolaw does not load Cohere Transcribe during a recording, so that the recording and the analysis have the memory. After **Finalizar**, **Refinar transcripción** loads it, transcribes each segment again from the hearing audio, stores the new text, and unloads the model. The database keeps the live text of each segment, and the revision of a segment grows by 1 at each change of its text.

**Modelos**, in the title row of a hearing, shows the state of each model: `Instalado · <quant>`, `En uso`, `No cargado`, `Dañado`, or not installed. Nicolaw checks each file before it uses it: a GGUF file must start with the GGUF header, and the download file must have its exact size. **Reparar** moves a damaged file aside as `<file>.invalid` (it does not delete it) and downloads the file again. A model that has valid files but does not load shows `Nicolaw no pudo cargar el modelo.`

Nicolaw looks for the files in the Hugging Face cache (`~/.cache/huggingface/hub`, or `HF_HUB_CACHE` / `HF_HOME`), where Handy keeps its catalog models, and in Handy's folder `~/Library/Application Support/com.pais.handy/models`. If Handy already downloaded a model, Nicolaw uses the same file. Nicolaw downloads a missing model into the same cache only when you select **Descargar** or **Reparar**, never during a recording: `Finalice la audiencia para descargar modelos.`

## Microphone

A bundled app needs `NSMicrophoneUsageDescription` in its `Info.plist`, and the `com.apple.security.device.audio-input` entitlement when it uses the hardened runtime or the sandbox. Without the permission, macOS gives silent audio. When you run the binary from a terminal, macOS asks for the permission of the terminal app.

Each hearing records its audio to `~/Library/Application Support/nicolaw/hearings/<id>.wav` (16 kHz, mono).

The microphone select has **Micrófono del sistema** first: it uses the input that macOS selects when the recording starts. A microphone that you select stays selected for the next hearings. If it is not connected, Nicolaw does not start and shows `El micrófono seleccionado no está disponible. Seleccione otro.` At each start, Nicolaw writes the open device and its formats to stderr, and the live strip shows `Entrada: <device>`.

The audio file is written by its own worker, apart from the transcription. When the transcription is slower than the speech, the audio file continues, the transcription reads the waiting audio from the file, and Nicolaw shows `Transcripción retrasada: N s. El audio sigue guardándose.` The live strip shows the capture health: `Grabando · Señal recibida`, `El micrófono no entrega señal. Revise la entrada seleccionada.` (no audio, or only zero samples, for 3 s), or `La señal es baja. Acérquese al micrófono.` (quiet audio for 10 s).

## Recording controls

The live strip under the top navigation shows the live hearing on every case and screen, with **Habla: Acusado** / **Habla: Otra persona**, **Pausar** / **Reanudar**, **Finalizar**, and **Volver a la audiencia**. Keys: Cmd+Shift+R starts the recording of the selected hearing, Cmd+Shift+P pauses or continues, and Cmd+Shift+A changes the speaker. A speaker change goes into the audio stream at that point, so the open segment ends there.

Cmd+Q, **Salir**, and the window close all save the recording before Nicolaw quits: the audio file and the end of the hearing first, then the last segments. A refine pass stops at its next segment.

## Analysis

During a hearing, Nicolaw analyzes each finished answer of the accused (the segments with the speaker **Acusado**). The question before the answer gives the context, only when it ends at most 90 seconds before the answer and no other answer of the accused is between them.

1. The language model writes the factual claims of the answer as standalone Spanish sentences (at most 5).
2. A full-text search finds the evidence passages of the case for each claim (top 20).
3. The NLI model scores each pair of a claim and a passage, in both directions.
4. For a flagged pair, the language model answers one choice question with four options: **contradicts**, **consistent**, **unrelated**, or **insufficient** (abstain). Nicolaw reads the probability of each option from one forward pass, not from generated text.
5. For a flagged pair, the language model explains the possible conflict and quotes the exact words of the claim and of the evidence. Nicolaw checks that each quote is an exact part of its source.

| Model | Use | Download |
| --- | --- | --- |
| [mDeBERTa-v3 multilingual NLI](https://huggingface.co/MoritzLaurer/mDeBERTa-v3-base-xnli-multilingual-nli-2mil7) (MIT) | Probability that a passage contradicts a claim | About 575 MB |
| [Qwen3 4B](https://huggingface.co/Qwen/Qwen3-4B-GGUF) (Apache 2.0) | Claims, decisions, and explanations | Q4_K_M, about 2.5 GB |

Both models run with [candle](https://github.com/huggingface/candle) on the GPU (Metal). Nicolaw downloads them into the Hugging Face cache when you select **Descargar** in the **Sugerencias** panel. Without them, Nicolaw still records and transcribes the hearing.

A suggestion is **Posible conflicto** when the NLI model gives a high probability of contradiction, the decision is **contradicts** with a probability of at least 60 %, and the explanation agrees. Every other flagged pair is **Revisar**. Each card shows the decision and its probability, for example **Contradice · 92 %** or **Insuficiente · 70 %**. Suggestions are not verdicts. Nicolaw does not score credibility and does not say who lies. A suggestion only shows a statement and an evidence passage that may not agree, with the file name and the page, so that the lawyer can read the source. Suggestions wait in the panel until the lawyer selects **Confirmar** or **Descartar**.

Each answer of the accused shows its analysis state in the transcript: analyzed, no verifiable claim, no passage found, failed, or not analyzed (with **Reintentar** after the hearing). No passage and no suggestion do not confirm a statement. When the text of a segment changes (refine) or its speaker changes, its open suggestions become stale; confirmed and dismissed suggestions stay. Nicolaw stores new suggestions only after the full analysis succeeds, and a dismissed suggestion does not come back as an open one. The analysis models unload after **Finalizar**, when no answer waits, and a new load waits until the last models are free.

### Why not Jev

The decision step follows the `choice` question of [Jev](https://developers.cloudflare.com/ai/models/typesafe/jev/) (TypeSafe AI): described options, and one probability for each option. Jev itself is API-only, and its weights are not published, so Nicolaw cannot run it locally.
[Cloudflare Clef and Clef-flash](https://developers.cloudflare.com/changelog/post/2026-10-01-clef-workers-ai/) (Apache 2.0) use the same System One API, but candle cannot load them, and the [published language list](https://huggingface.co/Cloudflare/clef-flash) has English only.
The mDeBERTa NLI model stays the first stage: it has a published Spanish result (XNLI-es 0.832).
The decision step runs on the Qwen3 4B that Nicolaw already loads. It scores the first token of each option label (option-logit scoring), with a softmax over the four options only.
The thresholds (the NLI thresholds and the 60 % decision minimum) are initial values. Tune them on labeled Spanish hearing data.

## Privacy

Everything stays on this machine. Nicolaw sends no evidence, audio, transcript, or suggestion to a server. The network is used only to download models when you select **Descargar**.

| Data | Location |
| --- | --- |
| Cases, evidence files, extracted text, transcripts, suggestions | `~/Library/Application Support/nicolaw/nicolaw.db` (SQLite) |
| Hearing audio | `~/Library/Application Support/nicolaw/hearings/<id>.wav` |
| Models | The Hugging Face cache (`~/.cache/huggingface/hub`) and Handy's model folder |

To remove data from the app, delete the hearing or the file in Nicolaw. See [Deletion](#deletion) for what deletion does not erase. Evidence text goes into the prompts of the language model as data inside tagged blocks, never as instructions.

## Deletion

When you delete a file in Nicolaw, Nicolaw removes the app's data of the file: its stored copy, its extracted text, and the copy that **Abrir** wrote to the temporary folder (`$TMPDIR/nicolaw/<id>`). Nicolaw also removes old **Abrir** copies when it starts. Deletion does not erase the original file that you added, and it does not erase the free pages of the database or the copies in your backups. It is not a secure erasure.

## Release

```sh
bash scripts/bundle.sh
open target/Nicolaw.app
```

The script builds the release binary and writes `target/Nicolaw.app` with the bundle ID `dev.pulgueta.nicolaw`, the microphone usage description, and an ad-hoc signature. The template of the `Info.plist` is `scripts/Info.plist`. The version comes from `Cargo.toml`.

Start Nicolaw from `target/Nicolaw.app`. The bare binary (`target/release/nicolaw`) cannot get the microphone permission.
