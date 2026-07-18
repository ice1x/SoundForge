#!/usr/bin/env bash
# Build SoundForge into a macOS .app + .dmg. One command, no prep needed.
#   scripts/mac-build.sh
set -euo pipefail
cd "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"

TARGET=aarch64-apple-darwin

command -v cargo >/dev/null || { echo "install Rust first: https://rustup.rs"; exit 1; }
rustup target add "$TARGET" >/dev/null 2>&1 || true
cargo tauri --version >/dev/null 2>&1 || cargo install tauri-cli --version '^2'

cargo tauri build --target "$TARGET"

echo
echo "Done:"
ls -1 "target/$TARGET/release/bundle/macos/"*.app 2>/dev/null || true
ls -1 "target/$TARGET/release/bundle/dmg/"*.dmg 2>/dev/null || true
