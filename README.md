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

## Deletion

When you delete a file in Nicolaw, Nicolaw removes the app's data of the file: its stored copy, its extracted text, and the copy that **Abrir** wrote to the temporary folder (`$TMPDIR/nicolaw/<id>`). Nicolaw also removes old **Abrir** copies when it starts. Deletion does not erase the original file that you added, and it does not erase the free pages of the database or the copies in your backups. It is not a secure erasure.

## Release

```sh
bash scripts/bundle.sh
open target/Nicolaw.app
```

The script builds the release binary and writes `target/Nicolaw.app` with the bundle ID `dev.pulgueta.nicolaw`, the microphone usage description, and an ad-hoc signature. The template of the `Info.plist` is `scripts/Info.plist`. The version comes from `Cargo.toml`.

Start Nicolaw from `target/Nicolaw.app`. The bare binary (`target/release/nicolaw`) cannot get the microphone permission.
