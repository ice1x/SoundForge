---
name: verify
description: Build, launch and drive SoundForge to observe a change actually working — the Tauri app, the analysis backend, or the web UI. Use when verifying a change end-to-end rather than just running tests.
---

# Verifying SoundForge

Tests are not verification here. `cargo test` + `npm test` are the CI gate; this skill is
about watching the running app do the thing.

## The gate (setup, not evidence)

```bash
scripts/build.sh check     # fmt + clippy -D warnings + cargo test + npm test
```

## Surfaces

| Change touches | Drive it via |
|---|---|
| `crates/sf-core/`, `src-tauri/src/audio.rs` | the HTTP bridge below (real `AudioState`, no webview) |
| `ui/` | the HTTP bridge below (real `app.js` in a scriptable browser) |
| shell wiring, plugins, `lib.rs` `run()` | `cargo tauri dev` (only the real app proves plugins/capabilities/WKWebView) |

## Running the real app

```bash
nohup cargo tauri dev > /tmp/tauri-dev.log 2>&1 &
sleep 40
grep -aE "UI booted|error|panic|\[ui\]" /tmp/tauri-dev.log
```

`[ui] UI booted. tauri=true dpr=2` is a **real IPC round-trip** (the UI log bridge calls
`frontend_log`), so that one line proves: the ES modules resolved, `app.js` ran to its last
statement, and the webview↔Rust bridge works. `screencapture -x -o shot.png` grabs the
screen — but the window may sit on another Space, and you cannot bring it forward.

**You cannot click the real app.** `osascript` has no assistive access ("osascript is not
allowed assistive access", -25211) and there is no `cliclick`/pyobjc. Granting it is a
security setting — don't. So the native file dialog cannot be driven; use the bridge.

## The HTTP bridge (how to actually drive the UI)

`AudioState` is deliberately free of Tauri types, so a plain binary can host it. Build a
throwaway crate **in the scratchpad, never in the repo**:

```toml
# Cargo.toml — needs [workspace] to escape the repo workspace
[dependencies]
soundforge = { path = "/Users/cold00n/repo/SoundForge/src-tauri" }   # crate is `soundforge_lib`
serde_json = "1"
[workspace]
```

A ~130-line `std::net::TcpListener` server (no HTTP deps needed) that:
- serves `ui/index.html|app.js|lib.js` from disk,
- injects, before the `<script type="module">` tag, a shim defining
  `window.__TAURI__.core.invoke = (cmd,args) => fetch('/invoke',{...})` and
  `window.__TAURI__.event.listen = async () => () => {}`,
- `POST /invoke` dispatches to the real `AudioState`: `open_file`, `audio_info`,
  `close_file`, `stats`, `waveform`, `frontend_log`, and — since task 14 — to a real
  `player::Player` for `play`, `pause_playback`, `resume_playback`, `stop_playback`,
  `playback_status` (audio really comes out of the speakers; `Player` needs no Tauri types
  either). Mirror the shell's ordering: `open_file`/`close_file` call `player.stop()` first.
- stubs `plugin:dialog|open` by returning a path from an env var — a **bare string**, exactly
  as the real command does (`OpenResponse::File` → untagged `FilePath` → JSON string). This
  makes the UI's real `pickFile()` → `openPath()` path run.

Then `mcp__Claude_Browser__preview_start` at the URL and drive it. This exercises the real
`app.js`/`lib.js` against the real Rust analysis code; only the literal Tauri invoke
transport is not crossed (and the `UI booted` line above covers that).

### Browser-driving gotchas

- `computer{action:"scroll"}` **times out**: the wheel handler calls `preventDefault()`, so
  the page never scrolls and the tool waits forever. Dispatch `new WheelEvent(...)` via
  `javascript_tool` instead.
- `computer` click/drag coordinates are in **screenshot-pixel space** (e.g. 800×500), scaled
  to the viewport (1280×800) — multiply by 1.6. Element refs from `read_page` are safer.
- A `computer` click can silently miss; `element.click()` via `javascript_tool` is reliable.
- Wrap `window.__TAURI__.core.invoke` to count/inspect calls — that is how you prove
  coalescing and the no-redraw-on-drag rule.
- **`requestAnimationFrame` never fires in the pane** — it reports `visibilityState:
  'hidden'` (measured: 0 frames in 500 ms). So anything rAF-driven (the playhead loop, and
  its button label) silently stalls, and the DOM goes stale while the backend runs on
  correctly. Don't read that as a bug: poll `playback_status` over IPC for truth, and take a
  `computer{action:"screenshot"}` — it forces a paint, which resumes rAF and lets the UI
  catch up in one step.
- `computer{action:"zoom"}` ignores its `region` and just returns the full screenshot.

## Verifying playback

Audio plays through the **bridge process's** default output device, so you cannot hear it —
but you can prove it ran: `playback_status.positionFrames` advances at *exactly* real time
(only the device clock can pace it), lands exactly on the range end, and `underruns` stays 0.

**A same-rate fixture silently skips the resampler.** This Mac's default output runs at
44.1 kHz, so a 44.1 kHz file gives `ratio == 1.0` and `Source` is a straight copy. To
exercise resampling use an **8 kHz** fixture (no CoreAudio device does 8 k natively); make it
**mono** and it covers mono→stereo duplication at the same time. A wrong ratio shows up as a
wrong duration, so check it played for as long as the file is.

## Proving shell wiring you cannot click

`manage(...)` and `generate_handler` registration only fail **at invoke time**, and the real
app cannot be clicked. To cross that seam, temporarily append a probe to `ui/app.js`:

```js
invoke('playback_status').then(d => sflog('info', `PROBE ok: ${JSON.stringify(d)}`))
  .catch(e => sflog('error', `PROBE FAILED: ${e.message || e}`));
```

`cargo tauri dev` hot-reloads the UI, the result lands in `/tmp/tauri-dev.log` via the log
bridge, and you **revert the probe immediately** (keep a backup copy first). This proves
managed state + command registration + DTO serialization across the real WKWebView boundary.

## Use a signal with a known answer

Generate a fixture whose statistics you can compute by hand — then the panel is checkable,
not just "looks plausible":

```python
# 5 s stereo @ 48k: L 440 Hz @ 0.5, R 880 Hz @ 0.25  (python3 stdlib `wave`, no deps)
```

Expected: ch0 peak **−6.02 dB**, RMS **−9.03 dB** (sine RMS = A/√2), **439.9 Hz**, 4 399
zero-crossings; ch1 **−12.04**, **−15.05**, **879.9 Hz**. Silence must show `DC offset −∞ dB`
(the reason the backend sends linear, not dB).

## Flows worth driving

1. Open → header geometry, waveform, whole-file stats.
2. Drag a selection → stats track the cursor; **count invokes**: a 200-move drag must issue
   ~2 `stats` and **0** `waveform` (envelope must never redraw mid-drag).
3. Zoom past one sample per pixel → the envelope must still render (it once vanished: the
   backend returns `(0,0)` for a bin containing no sample).
4. Switch channel → stats follow the selector.
5. Open a bad file **over an open document** → error banner, previous document survives, and
   `audio_info` still agrees with the UI.
6. Close → controls disabled, hint back; dragging the empty canvas is an inert no-op.

## Cleanup

```bash
pkill -f "target/debug/soundforge"; pkill -f "cargo-tauri"; lsof -ti:8777 | xargs kill -9
```
