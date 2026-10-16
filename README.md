# Nicolaw

Nicolaw is a desktop assistant for lawyers in Colombian court hearings. It stores case evidence locally, transcribes speech locally, and suggests statements that may conflict with the evidence. All data and all AI models stay on the local machine.

## Build

```sh
cargo build
cargo test
```

Requires macOS on Apple Silicon.

## Release

```sh
bash scripts/bundle.sh
open target/Nicolaw.app
```

The script builds the release binary and writes `target/Nicolaw.app` with the bundle ID `dev.pulgueta.nicolaw`, the microphone usage description, and an ad-hoc signature. The template of the `Info.plist` is `scripts/Info.plist`. The version comes from `Cargo.toml`.

Start Nicolaw from `target/Nicolaw.app`. The bare binary (`target/release/nicolaw`) cannot get the microphone permission.
