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
| Cohere Transcribe | Optional second pass on each segment, more accurate. Not a streaming model: it can also be the live model, but then the text shows only when a segment ends. | Not downloaded by Nicolaw |
| Silero VAD v6.2 | Finds the start and the end of speech | F32, about 1 MB |

Other Handy models (Parakeet EN, Voxtral, and the English-only models) are not eligible: they have no Spanish, or they are too slow for a live hearing.

Nicolaw looks for the files in the Hugging Face cache (`~/.cache/huggingface/hub`, or `HF_HUB_CACHE` / `HF_HOME`), where Handy keeps its catalog models, and in Handy's folder `~/Library/Application Support/com.pais.handy/models`. If Handy already downloaded a model, Nicolaw uses the same file. Nicolaw downloads a missing model into the same cache only when you select **Descargar**, never during a hearing.

## Microphone

A bundled app needs `NSMicrophoneUsageDescription` in its `Info.plist`, and the `com.apple.security.device.audio-input` entitlement when it uses the hardened runtime or the sandbox. Without the permission, macOS gives silent audio. When you run the binary from a terminal, macOS asks for the permission of the terminal app.

Each hearing records its audio to `~/Library/Application Support/nicolaw/hearings/<id>.wav` (16 kHz, mono).
## Deletion

When you delete a file in Nicolaw, Nicolaw removes the app's data of the file: its stored copy, its extracted text, and the copy that **Abrir** wrote to the temporary folder (`$TMPDIR/nicolaw/<id>`). Nicolaw also removes old **Abrir** copies when it starts. Deletion does not erase the original file that you added, and it does not erase the free pages of the database or the copies in your backups. It is not a secure erasure.

## Release

```sh
bash scripts/bundle.sh
open target/Nicolaw.app
```

The script builds the release binary and writes `target/Nicolaw.app` with the bundle ID `dev.pulgueta.nicolaw`, the microphone usage description, and an ad-hoc signature. The template of the `Info.plist` is `scripts/Info.plist`. The version comes from `Cargo.toml`.

Start Nicolaw from `target/Nicolaw.app`. The bare binary (`target/release/nicolaw`) cannot get the microphone permission.
