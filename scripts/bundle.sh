#!/usr/bin/env bash
# Builds the release binary and wraps it in target/Nicolaw.app with an ad-hoc signature.
# macOS gives the microphone only to an app bundle with NSMicrophoneUsageDescription.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$version" ]]; then
    echo "bundle.sh: cannot read the version from Cargo.toml" >&2
    exit 1
fi

cargo build --release

app="target/Nicolaw.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/nicolaw "$app/Contents/MacOS/Nicolaw"
sed "s/@VERSION@/$version/g" scripts/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" > /dev/null
codesign --force --sign - "$app"

echo "$app $version"
