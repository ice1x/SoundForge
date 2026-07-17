//! Tauri shell for SoundForge. Owns the window, exposes IPC commands to the web UI, and
//! holds the audio engine backed by `sf-core`.
//!
//! Audio: [`audio::AudioState`] keeps at most one open document — a decoded, memory-mapped
//! PCM cache plus a per-channel summary pyramid — and answers the [`stats`] and [`waveform`]
//! commands from it in O(log N), independent of selection length. The commands here are thin
//! wrappers so that the state logic stays unit-testable without a webview.
//!
//! Playback: [`player::Player`] streams a range of that same PCM to the default output
//! device (task 14). It shares the document's samples by `Arc` rather than reading them
//! through [`audio::AudioState`], so the audio path never contends with a selection drag for
//! the document lock.
//!
//! Logging: the `tauri-plugin-log` plugin fans every `log::*` record out to stdout,
//! a rotating file in the OS log directory, and the webview devtools console. The web
//! UI forwards its own `console.*` output and uncaught errors here via [`frontend_log`],
//! so a single log file captures both sides — essential for debugging webview-only
//! failures (e.g. `MediaRecorder` being unavailable in WKWebView) that never produce an
//! OS crash report.

pub mod audio;
pub mod cache;
pub mod player;

use std::path::PathBuf;

use tauri::{Manager, State};
use tauri_plugin_log::{Target, TargetKind};

use audio::{
    AudioInfo, AudioState, EditDto, EditOp, ExportDto, ExportFormat, StatsDto, WaveformDto,
};
use player::{PlaybackDto, Player};

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
    player: State<'_, Player>,
    path: String,
) -> Result<AudioInfo, String> {
    // Whatever is playing belongs to the outgoing document. Stopping first also releases the
    // device before the (slow) decode, rather than leaving a stream running through it.
    player.stop();
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
fn close_file(state: State<'_, AudioState>, player: State<'_, Player>) {
    if state.info().is_some() {
        log::info!("closing document");
    }
    // Playback holds its own `Arc` on the PCM, so it would happily keep playing a file the
    // user has closed. Stop it first so closing means what it says.
    player.stop();
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

/// Apply an in-place edit to `[start, end)` across every channel.
///
/// Stops playback first: the audio thread holds its own handle on the samples, so editing
/// under it would be rejected as busy — and, more to the point, would be editing audio the
/// user is currently listening to.
#[tauri::command]
fn edit(
    state: State<'_, AudioState>,
    player: State<'_, Player>,
    op: EditOp,
    start: usize,
    end: usize,
) -> Result<EditDto, String> {
    player.stop();
    state.edit(op, start, end).map_err(|e| {
        log::error!("edit({op:?}, [{start}, {end})) failed: {e}");
        e.to_string()
    })
}

/// Discard everything outside `[start, end)`. Changes the document's length, so the UI must
/// re-read the geometry from the returned `info`.
#[tauri::command]
fn trim(
    app: tauri::AppHandle,
    state: State<'_, AudioState>,
    player: State<'_, Player>,
    start: usize,
    end: usize,
) -> Result<EditDto, String> {
    player.stop();
    // A trim writes a whole new cache, so it needs its own unique path for the same reason
    // an open does — see `cache::next_path`.
    let cache = next_cache_path(&app)?;
    state.trim(start, end, &cache).map_err(|e| {
        log::error!("trim([{start}, {end})) failed: {e}");
        // The trim may have created the file before failing; do not leave it behind.
        let _ = std::fs::remove_file(&cache);
        e.to_string()
    })
}

/// Reverse the most recent edit.
#[tauri::command]
fn undo(state: State<'_, AudioState>, player: State<'_, Player>) -> Result<EditDto, String> {
    player.stop();
    state.undo().map_err(|e| e.to_string())
}

/// Write `[start, end)` of the open document to `path` as a WAV in `format`.
///
/// Read-only, so — unlike `edit`/`trim` — it does not stop playback: exporting audio the user
/// is listening to is harmless. The UI passes `[0, frames)` to export the whole file.
#[tauri::command]
fn export(
    state: State<'_, AudioState>,
    path: String,
    start: usize,
    end: usize,
    format: ExportFormat,
) -> Result<ExportDto, String> {
    state
        .export(std::path::Path::new(&path), start, end, format.into())
        .map_err(|e| {
            log::error!("export({path}, [{start}, {end}), {format:?}) failed: {e}");
            // A partially-written file is worse than none: the encoder failed mid-stream, so
            // clean it up rather than leaving a truncated WAV behind.
            let _ = std::fs::remove_file(&path);
            e.to_string()
        })
}

/// Play `[start, end)` of the open document on the default output device, replacing any
/// current playback. The range is clamped to the document; an empty one is an error.
#[tauri::command]
fn play(
    state: State<'_, AudioState>,
    player: State<'_, Player>,
    start: usize,
    end: usize,
) -> Result<PlaybackDto, String> {
    let pcm = state.pcm().map_err(|e| e.to_string())?;
    player
        .play(pcm, start, end)
        .inspect(|_| {
            log::info!("playing frames [{start}, {end})");
        })
        .map_err(|e| {
            log::error!("play([{start}, {end})) failed: {e}");
            e.to_string()
        })
}

/// Pause playback where it stands, keeping the device open. No-op when nothing is playing.
#[tauri::command]
fn pause_playback(player: State<'_, Player>) -> PlaybackDto {
    player.pause()
}

/// Resume a paused playback. No-op when nothing is playing.
#[tauri::command]
fn resume_playback(player: State<'_, Player>) -> PlaybackDto {
    player.resume()
}

/// Stop playback and release the output device.
#[tauri::command]
fn stop_playback(player: State<'_, Player>) -> PlaybackDto {
    player.stop();
    player.status()
}

/// Transport state and playhead position.
///
/// The UI polls this per animation frame to draw the playhead, so it is only a handful of
/// atomic loads — it never touches the device or the document.
#[tauri::command]
fn playback_status(player: State<'_, Player>) -> PlaybackDto {
    player.status()
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
        // Native file picker. `open_file` needs a real filesystem path, which the webview's
        // `<input type="file">` cannot supply — it only yields an opaque `File` handle.
        .plugin(tauri_plugin_dialog::init())
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
        .manage(Player::default())
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
            waveform,
            edit,
            trim,
            undo,
            export,
            play,
            pause_playback,
            resume_playback,
            stop_playback,
            playback_status
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
