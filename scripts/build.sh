#!/usr/bin/env bash
#
# SoundForge build helper.
#
# One entry point for the common build/verify tasks so contributors and CI run
# the exact same commands. Mirrors the "everything green" rule from the README:
# cargo fmt --check, cargo clippy -- -D warnings, cargo test.
#
# Usage:
#   scripts/build.sh [command]
#
# Commands:
#   check      Format check, clippy (-D warnings) and tests for the whole
#              workspace. This is the gate that must be green before pushing.
#   core       Build the pure-Rust analysis core (sf-core) in release mode.
#   build      Debug build of the whole workspace.
#   release    Optimized release build of the whole workspace (default).
#   app        Bundle the native app (.app/.dmg/...) via `cargo tauri build`.
#   dev        Run the app in watch mode via `cargo tauri dev`.
#   clean      Remove build artifacts (cargo clean).
#   all        Run `check` and then `release`.
#   help       Show this help.
#
# Environment:
#   CARGO   Override the cargo binary (default: cargo).

set -euo pipefail

# Always operate from the repository root, regardless of the caller's CWD.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd -P)"
cd "${ROOT_DIR}"

CARGO="${CARGO:-cargo}"

# Colored logging, but stay plain when stdout is not a terminal (e.g. in CI).
if [ -t 1 ]; then
  C_BOLD="$(printf '\033[1m')"
  C_BLUE="$(printf '\033[34m')"
  C_GREEN="$(printf '\033[32m')"
  C_RED="$(printf '\033[31m')"
  C_RESET="$(printf '\033[0m')"
else
  C_BOLD="" C_BLUE="" C_GREEN="" C_RED="" C_RESET=""
fi

step() { printf '%s==>%s %s%s%s\n' "${C_BLUE}" "${C_RESET}" "${C_BOLD}" "$*" "${C_RESET}"; }
ok() { printf '%s✓%s %s\n' "${C_GREEN}" "${C_RESET}" "$*"; }
die() {
  printf '%serror:%s %s\n' "${C_RED}" "${C_RESET}" "$*" >&2
  exit 1
}

require_tauri_cli() {
  if ! "${CARGO}" tauri --version >/dev/null 2>&1; then
    die "the Tauri CLI is not installed. Install it with: cargo install tauri-cli --version '^2'"
  fi
}

cmd_check() {
  step "Format check"
  "${CARGO}" fmt --all --check
  step "Clippy (-D warnings)"
  "${CARGO}" clippy --workspace --all-targets -- -D warnings
  step "Tests"
  "${CARGO}" test --workspace
  ok "check passed"
}

cmd_core() {
  step "Building sf-core (release)"
  "${CARGO}" build -p sf-core --release
  ok "sf-core built"
}

cmd_build() {
  step "Building workspace (debug)"
  "${CARGO}" build --workspace
  ok "workspace built (debug)"
}

cmd_release() {
  step "Building workspace (release)"
  "${CARGO}" build --workspace --release
  ok "workspace built (release)"
}

cmd_app() {
  require_tauri_cli
  step "Bundling native app (cargo tauri build)"
  "${CARGO}" tauri build
  ok "app bundle produced under target/release/bundle/"
}

cmd_dev() {
  require_tauri_cli
  step "Starting dev app (cargo tauri dev)"
  exec "${CARGO}" tauri dev
}

cmd_clean() {
  step "Cleaning build artifacts"
  "${CARGO}" clean
  ok "clean done"
}

cmd_all() {
  cmd_check
  cmd_release
}

usage() {
  # Print the leading comment block (everything after the shebang up to the
  # first non-comment line), stripping the leading "# ".
  awk 'NR==1 && /^#!/ {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "${BASH_SOURCE[0]}"
}

main() {
  local command="${1:-release}"
  case "${command}" in
    check) cmd_check ;;
    core) cmd_core ;;
    build) cmd_build ;;
    release) cmd_release ;;
    app) cmd_app ;;
    dev) cmd_dev ;;
    clean) cmd_clean ;;
    all) cmd_all ;;
    help | -h | --help) usage ;;
    *) die "unknown command '${command}' (try: scripts/build.sh help)" ;;
  esac
}

main "$@"
