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
#              workspace, plus the ui/ tests. This is the gate that must be
#              green before pushing.
#   ui         Test the web UI's pure logic (node --test, no dependencies).
#   core       Build the pure-Rust analysis core (sf-core) in release mode.
#   build      Debug build of the whole workspace.
#   release    Optimized release build of the whole workspace (default).
#   bench      Run the seamless-statistics benchmark (task 18): a 2-hour (~1.2 GB)
#              file, asserting stats update < 5 ms/drag, independent of selection
#              length, with a stable resident set. Kept out of `check` (too heavy
#              for CI); tune with SF_BENCH_SECS / SF_BENCH_SR / SF_BENCH_MOVES.
#   app        Bundle the signed Apple-Silicon .app/.dmg via `cargo tauri build`
#              (--target aarch64-apple-darwin). Signs + notarizes when the Apple
#              env vars are set (see below); otherwise makes an unsigned local build.
#              Override the target with SF_APP_TARGET; SF_APP_DRY_RUN=1 prints the cmd.
#   dev        Run the app in watch mode via `cargo tauri dev`.
#   clean      Remove build artifacts (cargo clean).
#   all        Run `check` and then `release`.
#   help       Show this help.
#
# Environment:
#   CARGO          Override the cargo binary (default: cargo).
#   NODE           Override the node binary (default: node), used by the ui/ tests.
#   SF_APP_TARGET  Rust target for `app` (default: aarch64-apple-darwin).
#   SF_APP_DRY_RUN Print the `app` command instead of running it.
#   Apple signing (read by Tauri when bundling with `app`):
#     APPLE_SIGNING_IDENTITY | APPLE_CERTIFICATE + APPLE_CERTIFICATE_PASSWORD  — code-sign
#     APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID | APPLE_API_KEY + APPLE_API_ISSUER
#       + APPLE_API_KEY_PATH                                                   — notarize

set -euo pipefail

# Always operate from the repository root, regardless of the caller's CWD.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd -P)"
cd "${ROOT_DIR}"

CARGO="${CARGO:-cargo}"
NODE="${NODE:-node}"

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

require_node() {
  if ! "${NODE}" --version >/dev/null 2>&1; then
    die "Node is not installed (needed for the ui/ tests). Install Node 18+ or set NODE=/path/to/node"
  fi
}

cmd_check() {
  step "Format check"
  "${CARGO}" fmt --all --check
  step "Clippy (-D warnings)"
  "${CARGO}" clippy --workspace --all-targets -- -D warnings
  step "Tests"
  "${CARGO}" test --workspace
  step "UI tests"
  cmd_ui
  step "Build-script tests"
  bash tests/build/app.test.sh
  ok "check passed"
}

# The web UI's pure logic is tested with the Node built-in runner: no dependencies, no
# build step, nothing to install.
cmd_ui() {
  require_node
  "${NODE}" --test "tests/ui/**/*.test.js"
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

# Reports whether the environment carries Apple code-signing material. Tauri reads the
# certificate itself (APPLE_SIGNING_IDENTITY for a keychain identity, or APPLE_CERTIFICATE
# + APPLE_CERTIFICATE_PASSWORD for an imported .p12); we only need to know if either is
# present so we can tell the user whether the bundle will be signed. Kept side-effect-free
# and echo-only so it can be unit-tested (tests/build/app.test.sh).
app_signing_status() {
  if [ -n "${APPLE_SIGNING_IDENTITY:-}" ] || [ -n "${APPLE_CERTIFICATE:-}" ]; then
    printf 'signed\n'
  else
    printf 'unsigned\n'
  fi
}

# Bundle the native app for Apple Silicon via `cargo tauri build`.
#
# The target is pinned to aarch64-apple-darwin so the bundle is arm64 regardless of the
# host (override with SF_APP_TARGET, e.g. universal-apple-darwin). Signing and notarization
# are driven entirely by Tauri's own environment variables — set APPLE_SIGNING_IDENTITY
# (or APPLE_CERTIFICATE + APPLE_CERTIFICATE_PASSWORD) to code-sign with the hardened runtime
# and entitlements.plist, plus APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID (or APPLE_API_KEY,
# APPLE_API_ISSUER, APPLE_API_KEY_PATH) to notarize. With none of them set the bundle is
# still produced, just unsigned — fine for local runs, not for distribution.
#
# Any extra arguments are forwarded to `cargo tauri build`. Set SF_APP_DRY_RUN=1 to print
# the exact command without running it.
cmd_app() {
  local target="${SF_APP_TARGET:-aarch64-apple-darwin}"
  local -a cmd=("${CARGO}" tauri build --target "${target}" "$@")

  if [ -n "${SF_APP_DRY_RUN:-}" ]; then
    printf '%s\n' "${cmd[*]}"
    return 0
  fi

  require_tauri_cli
  if [ "$(app_signing_status)" = "signed" ]; then
    step "Signing with '${APPLE_SIGNING_IDENTITY:-<certificate from APPLE_CERTIFICATE>}'"
  else
    step "No Apple signing identity in the environment — producing an UNSIGNED local build"
    step "  set APPLE_SIGNING_IDENTITY (and APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID to notarize) to sign"
  fi
  step "Bundling Apple-Silicon app: ${cmd[*]}"
  "${cmd[@]}"
  ok "app bundle produced under target/${target}/release/bundle/"
}

cmd_dev() {
  require_tauri_cli
  step "Starting dev app (cargo tauri dev)"
  exec "${CARGO}" tauri dev
}

cmd_bench() {
  step "Seamless-statistics benchmark (task 18)"
  # A plain self-checking `fn main` (harness = false): it prints a latency table and
  # exits non-zero if the < 5 ms/drag, length-independence, or RAM-stability checks fail.
  "${CARGO}" bench -p sf-core --bench seamless
  ok "benchmark passed"
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
  # Consume the command word so the remaining "$@" is the command's own arguments
  # (only `app` forwards them today, to `cargo tauri build`).
  [ "$#" -gt 0 ] && shift
  case "${command}" in
    check) cmd_check ;;
    ui) cmd_ui ;;
    core) cmd_core ;;
    build) cmd_build ;;
    release) cmd_release ;;
    bench) cmd_bench ;;
    app) cmd_app "$@" ;;
    dev) cmd_dev ;;
    clean) cmd_clean ;;
    all) cmd_all ;;
    help | -h | --help) usage ;;
    *) die "unknown command '${command}' (try: scripts/build.sh help)" ;;
  esac
}

# Only dispatch when executed directly. When sourced (e.g. by tests/build/app.test.sh to
# call the pure helpers in isolation), skip main so sourcing has no side effects.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  main "$@"
fi
