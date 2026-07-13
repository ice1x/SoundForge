//! Tauri shell for SoundForge. Owns the window, exposes IPC commands to the web UI,
//! and (in later tasks) holds the audio engine backed by `sf-core`.
//!
//! Logging: the `tauri-plugin-log` plugin fans every `log::*` record out to stdout,
//! a rotating file in the OS log directory, and the webview devtools console. The web
//! UI forwards its own `console.*` output and uncaught errors here via [`frontend_log`],
//! so a single log file captures both sides — essential for debugging webview-only
//! failures (e.g. `MediaRecorder` being unavailable in WKWebView) that never produce an
//! OS crash report.

use tauri::Manager;
use tauri_plugin_log::{Target, TargetKind};

/// IPC smoke-test command: verifies the web UI ↔ Rust bridge is wired up.
/// Returns `"pong: <msg>"`.
#[tauri::command]
fn ping(msg: String) -> String {
    format!("pong: {msg}")
}

/// Receive a log line from the web UI and record it through the backend logger, so that
/// UI diagnostics land in the same file as native logs. `level` is one of
/// `error|warn|info|debug|trace` (anything else is treated as `info`).
#[tauri::command]
fn frontend_log(level: String, message: String) {
    match level.as_str() {
        "error" => log::error!("[ui] {message}"),
        "warn" => log::warn!("[ui] {message}"),
        "debug" => log::debug!("[ui] {message}"),
        "trace" => log::trace!("[ui] {message}"),
        _ => log::info!("[ui] {message}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(level)
                .targets([
                    // Terminal (visible under `cargo tauri dev`).
                    Target::new(TargetKind::Stdout),
                    // Persistent file: ~/Library/Logs/com.soundforge.app/soundforge.log on macOS.
                    Target::new(TargetKind::LogDir {
                        file_name: Some("soundforge".into()),
                    }),
                    // Webview devtools console.
                    Target::new(TargetKind::Webview),
                ])
                .build(),
        )
        .setup(|app| {
            log::info!(
                "SoundForge {} starting (debug_assertions={})",
                env!("CARGO_PKG_VERSION"),
                cfg!(debug_assertions)
            );
            match app.path().app_log_dir() {
                Ok(dir) => log::info!("log directory: {}", dir.display()),
                Err(e) => log::warn!("could not resolve log directory: {e}"),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![ping, frontend_log])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_replies_pong() {
        assert_eq!(ping("hello".to_string()), "pong: hello");
    }

    #[test]
    fn frontend_log_accepts_all_levels() {
        // Should not panic for any level string (unknown levels fall back to info).
        for lvl in ["error", "warn", "info", "debug", "trace", "weird"] {
            frontend_log(lvl.to_string(), "smoke".to_string());
        }
    }
}
