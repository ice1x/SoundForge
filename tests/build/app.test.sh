#!/usr/bin/env bash
#
# Tests for the `app` bundling path of scripts/build.sh (task 19).
#
# The real bundle is far too heavy to run in a test (it needs the Tauri CLI, an Apple
# toolchain and, to be signed, a certificate). What we CAN pin down cheaply is the pure
# decision logic: the exact `cargo tauri build` argv it constructs for an Apple-Silicon
# bundle, and whether it correctly reports the build as signed or unsigned from the
# environment. Both functions are side-effect-free, so we source the script (its `main`
# is guarded) and call them directly, using SF_APP_DRY_RUN to print the argv instead of
# running it.
#
# Pure bash + POSIX tools, no dependencies. Run directly (`bash tests/build/app.test.sh`)
# or via `scripts/build.sh check`.

set -euo pipefail

cd "$(dirname -- "$0")/../.."

# shellcheck source=scripts/build.sh
source scripts/build.sh

fails=0
check() {
  # check <description> <actual> <expected>
  if [ "$2" = "$3" ]; then
    printf 'ok   - %s\n' "$1"
  else
    printf 'FAIL - %s\n     expected: [%s]\n       actual: [%s]\n' "$1" "$3" "$2"
    fails=$((fails + 1))
  fi
}

# --- argv construction -------------------------------------------------------------

# Default: target Apple Silicon explicitly so the bundle is arm64 regardless of the host.
out="$(SF_APP_DRY_RUN=1 CARGO=cargo cmd_app)"
check "default argv targets aarch64-apple-darwin" \
  "$out" "cargo tauri build --target aarch64-apple-darwin"

# The CARGO override is honored (CI / alternate toolchains).
out="$(SF_APP_DRY_RUN=1 CARGO=my-cargo cmd_app)"
check "CARGO override flows into the argv" \
  "$out" "my-cargo tauri build --target aarch64-apple-darwin"

# The target is overridable (e.g. a universal build) without touching the script.
out="$(SF_APP_DRY_RUN=1 CARGO=cargo SF_APP_TARGET=universal-apple-darwin cmd_app)"
check "SF_APP_TARGET overrides the target" \
  "$out" "cargo tauri build --target universal-apple-darwin"

# Extra arguments pass straight through to `cargo tauri build`.
out="$(SF_APP_DRY_RUN=1 CARGO=cargo cmd_app --verbose --bundles dmg)"
check "extra args pass through to tauri build" \
  "$out" "cargo tauri build --target aarch64-apple-darwin --verbose --bundles dmg"

# --- signing detection -------------------------------------------------------------

out="$(unset APPLE_SIGNING_IDENTITY APPLE_CERTIFICATE 2>/dev/null; app_signing_status)"
check "no signing material -> unsigned" "$out" "unsigned"

out="$(unset APPLE_CERTIFICATE 2>/dev/null; APPLE_SIGNING_IDENTITY='Developer ID Application: Acme (TEAMID)' app_signing_status)"
check "APPLE_SIGNING_IDENTITY -> signed" "$out" "signed"

out="$(unset APPLE_SIGNING_IDENTITY 2>/dev/null; APPLE_CERTIFICATE='base64-encoded-p12' app_signing_status)"
check "APPLE_CERTIFICATE -> signed" "$out" "signed"

# An empty identity string must not count as signing material.
out="$(unset APPLE_CERTIFICATE 2>/dev/null; APPLE_SIGNING_IDENTITY='' app_signing_status)"
check "empty identity -> unsigned" "$out" "unsigned"

# -----------------------------------------------------------------------------------

if [ "$fails" -ne 0 ]; then
  printf '\n%d build-script test(s) failed\n' "$fails" >&2
  exit 1
fi
printf '\nall build-script tests passed\n'
