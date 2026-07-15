//! Tauri shell for SoundForge. Owns the window, exposes IPC commands to the web UI, and
//! holds the audio engine backed by `sf-core`.
//!
//! Audio: [`audio::AudioState`] keeps at most one open document — a decoded, memory-mapped
//! PCM cache plus a per-channel summary pyramid — and answers the [`stats`] and [`waveform`]
//! commands from it in O(log N), independent of selection length. The commands here are thin
//! wrappers so that the state logic stays unit-testable without a webview.
//!
//! Logging: the `tauri-plugin-log` plugin fans every `log::*` record out to stdout,
//! a rotating file in the OS log directory, and the webview devtools console. The web
//! UI forwards its own `console.*` output and uncaught errors here via [`frontend_log`],
//! so a single log file captures both sides — essential for debugging webview-only
//! failures (e.g. `MediaRecorder` being unavailable in WKWebView) that never produce an
//! OS crash report.

pub mod audio;
pub mod cache;

use std::path::PathBuf;

use tauri::{Manager, State};
use tauri_plugin_log::{Target, TargetKind};

use audio::{AudioInfo, AudioState, StatsDto, WaveformDto};

/// IPC smoke-test command: verifies the web UI ↔ Rust bridge is wired up.
/// Returns `"pong: <msg>"`.
#[tauri::command]
fn ping(msg: String) -> String {
    format!("pong: {msg}")
}

/// The app cache directory, created if needed. This is where PCM caches live.
fn cache_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no app cache directory: {e}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// A fresh PCM cache path in the app cache directory. See [`cache::next_path`] for why each
/// open must get its own.
fn next_cache_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(cache::next_path(&cache_dir(app)?))
}

/// Decode `path` into a memory-mapped PCM cache and make it the open document.
/// Returns the document geometry; the decode is O(n) and the only slow step.
#[tauri::command]
fn open_file(
    app: tauri::AppHandle,
    state: State<'_, AudioState>,
    path: String,
) -> Result<AudioInfo, String> {
    let cache = next_cache_path(&app)?;
    let info = state
        .open(std::path::Path::new(&path), &cache)
        .map_err(|e| {
            log::error!("open_file({path}) failed: {e}");
            e.to_string()
        })?;
    log::info!(
        "opened {path}: {} ch, {} Hz, {} frames ({:.3} s)",
        info.channels,
        info.sample_rate,
        info.frames,
        info.duration_s
    );
    Ok(info)
}

/// Geometry of the open document, or `null` if nothing is open.
#[tauri::command]
fn audio_info(state: State<'_, AudioState>) -> Option<AudioInfo> {
    state.info()
}

/// Close the open document and delete its PCM cache.
#[tauri::command]
fn close_file(state: State<'_, AudioState>) {
    if state.info().is_some() {
        log::info!("closing document");
    }
    state.close();
}

/// Seamless statistics for the selection `[start, end)` on channel `ch`.
///
/// O(log N) in the selection length: this is the command the UI hits on every mouse-move of
/// a selection drag, so it must never scan the selection.
#[tauri::command]
fn stats(
    state: State<'_, AudioState>,
    ch: usize,
    start: usize,
    end: usize,
) -> Result<StatsDto, String> {
    state.stats(ch, start, end).map_err(|e| e.to_string())
}

/// Min/max envelope of `[start, end)` on channel `ch`, bucketed into `bins` pixels.
#[tauri::command]
fn waveform(
    state: State<'_, AudioState>,
    ch: usize,
    start: usize,
    end: usize,
    bins: usize,
) -> Result<WaveformDto, String> {
    state
        .waveform(ch, start, end, bins)
        .map_err(|e| e.to_string())
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
        .manage(AudioState::default())
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

            // Reclaim PCM caches orphaned by an instance that died without running `Drop`
            // (SIGKILL, force-quit, panic=abort). These are gigabyte-scale files, so leaving
            // them would leak disk across crashes.
            //
            // This must stay in `setup`, which runs before the webview can invoke `open_file`:
            // `sweep_at_startup` deletes caches bearing our own pid, which is only sound while
            // this process has not written any yet. See its docs.
            match cache_dir(app.handle()) {
                Ok(dir) => {
                    let swept =
                        cache::sweep_at_startup(&dir, std::process::id(), cache::pid_is_live);
                    if swept.removed > 0 || swept.failed > 0 {
                        log::info!(
                            "PCM cache sweep: reaped {} orphan(s), {} bytes; kept {}; {} failed",
                            swept.removed,
                            swept.bytes_freed,
                            swept.kept,
                            swept.failed
                        );
                    } else {
                        log::debug!("PCM cache sweep: nothing to reap ({} kept)", swept.kept);
                    }
                }
                // Never block startup over cache hygiene.
                Err(e) => log::warn!("PCM cache sweep skipped: {e}"),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping,
            frontend_log,
            open_file,
            audio_info,
            close_file,
            stats,
            waveform
        ])
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
