//! Integration test for the signed Apple-Silicon bundle configuration (task 19).
//!
//! `cargo tauri build` must produce a `.app`/`.dmg` that (a) runs on Apple Silicon,
//! (b) can be code-signed with the hardened runtime so it survives notarization, and
//! (c) still reaches the microphone once that runtime is enabled. None of that requires
//! the bundler to actually run — it is all declared up front in `tauri.conf.json`,
//! `entitlements.plist` and `Info.plist`. This test locks those declarations together so
//! a stray edit cannot silently ship an app that notarization rejects, or that macOS
//! kills the first time the user presses Record.
//!
//! It is a plain file/JSON check: no toolchain, no signing material, no network — so it
//! runs anywhere `cargo test` runs (Ubuntu CI included), not just on a Mac.

use std::path::PathBuf;

use serde_json::Value;

/// The `src-tauri` directory (this crate's manifest dir), the root all bundle paths in
/// `tauri.conf.json` are resolved against.
fn src_tauri() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = src_tauri().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn config() -> Value {
    serde_json::from_str(&read("tauri.conf.json")).expect("tauri.conf.json is valid JSON")
}

#[test]
fn bundling_is_active_and_yields_a_dmg_and_app() {
    let conf = config();
    let bundle = &conf["bundle"];
    assert_eq!(
        bundle["active"],
        Value::Bool(true),
        "bundling must stay active or `cargo tauri build` produces no installer"
    );

    // On macOS "all" expands to the `.app` plus a `.dmg`; an explicit list is also fine
    // as long as it names both.
    let targets = &bundle["targets"];
    let yields_app_and_dmg = targets == &Value::String("all".into())
        || targets.as_array().is_some_and(|a| {
            let has = |t: &str| a.iter().any(|v| v == &Value::String(t.into()));
            has("app") && has("dmg")
        });
    assert!(
        yields_app_and_dmg,
        "bundle.targets must produce a .app and a .dmg (got {targets})"
    );
}

#[test]
fn macos_bundle_targets_apple_silicon_and_is_signable() {
    let conf = config();
    let macos = &conf["bundle"]["macOS"];
    assert!(
        macos.is_object(),
        "a bundle.macOS block is required to set the deployment target and entitlements"
    );

    // Apple Silicon first shipped on macOS 11 (Big Sur); nothing older can run the app, and
    // declaring the floor keeps the bundle honest about what it supports.
    let min = macos["minimumSystemVersion"]
        .as_str()
        .expect("bundle.macOS.minimumSystemVersion must be set");
    let major: u32 = min
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("minimumSystemVersion should look like \"11.0\" (got {min:?})"));
    assert!(
        major >= 11,
        "minimumSystemVersion must be >= 11.0 for Apple Silicon (got {min})"
    );

    // Notarization requires the hardened runtime, which requires an entitlements file; the
    // path is relative to src-tauri and must actually exist on disk.
    let ent = macos["entitlements"]
        .as_str()
        .expect("bundle.macOS.entitlements must point at an entitlements file");
    assert!(
        src_tauri().join(ent).is_file(),
        "entitlements file `{ent}` referenced by tauri.conf.json must exist"
    );
}

#[test]
fn entitlements_grant_microphone_under_the_hardened_runtime() {
    let conf = config();
    let ent = conf["bundle"]["macOS"]["entitlements"]
        .as_str()
        .expect("entitlements path");
    let plist = read(ent);
    assert!(
        plist.contains("com.apple.security.device.audio-input"),
        "the entitlements must grant `com.apple.security.device.audio-input`, or recording \
         (task 15) is blocked once the hardened runtime is enabled for notarization"
    );
}

#[test]
fn info_plist_still_explains_microphone_use() {
    // Belt-and-braces with task 15 and the entitlement above: the usage string is what the
    // system permission prompt shows, and a hardened-runtime app that has the entitlement
    // but no usage string is still terminated on its first microphone access.
    let plist = read("Info.plist");
    assert!(
        plist.contains("NSMicrophoneUsageDescription"),
        "Info.plist must keep the NSMicrophoneUsageDescription string"
    );
}
