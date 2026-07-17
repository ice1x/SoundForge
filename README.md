# SoundForge

A native, Apple-Silicon-first audio editor in the spirit of SoundForge Pro — record,
play, and edit audio, with **seamless statistics on any selection**: Peak, RMS, DC
offset, min/max, zero-crossings and frequency update instantly as you drag a selection,
even across multi-hour / multi-gigabyte files. That instant, no-"compute-and-wait"
analysis is the feature competitors (Audacity et al.) lack, and it is the reason this
project exists.

## Stack

Native desktop app; macOS / Apple Silicon now, cross-platform (Windows/Linux) later.

| Layer | Technology |
|---|---|
| Shell / window | **Tauri v2** (system WebView, native `.app`/`.dmg`, web↔Rust IPC) |
| UI | `miniforge.html` design (vanilla JS + Canvas, no build step) calling Rust via IPC — see [Web UI](#web-ui) |
| Audio core | **Rust** — `sf-core`: decode, summary pyramid, statistics, edits |
| Decode | `symphonia` (WAV/FLAC/MP3/AAC/OGG/ALAC, streaming) |
| WAV I/O | `hound` |
| Playback / record | `cpal` (CoreAudio backend on macOS) + lock-free ring buffer (`rtrb`) |
| Memory-mapping | `memmap2` (PCM cache on disk, huge files without holding them in RAM) |

## How seamless analysis works

The differentiator lives in `crates/sf-core`. A **summary pyramid** of associative
`Agg` blocks is built over the sample buffer once. Any range query is answered by
scanning only the short unaligned head/tail and stitching O(log N) precomputed blocks,
so cost is independent of the selection length. The same pyramid feeds the waveform
view (min/max per pixel). See `crates/sf-core/src/summary.rs`.

The pyramid is built **once per channel when a file is opened** (`Pyramid::build`) and kept
alongside the memory-mapped PCM in `src-tauri/src/audio.rs`; each query then borrows it via
`Analyzer::with_pyramid` for free. Measured on a 60 s stereo file (2.88 M frames/channel),
release build: ~1.2 µs per stats query whether the selection is 1 000 samples or the whole
file, i.e. a full selection drag costs ~1.2 µs per mouse-move.

## IPC commands

The web UI calls these via `invoke()` (see `src-tauri/src/lib.rs`):

| Command | Args | Returns |
|---|---|---|
| `open_file` | `path` | `AudioInfo` — decodes to a PCM cache, builds the pyramids (the only O(n) step) |
| `audio_info` | — | `AudioInfo` or `null` |
| `close_file` | — | — (releases the document and deletes its cache) |
| `stats` | `ch`, `start`, `end` | `StatsDto` — seamless selection statistics, O(log N) |
| `waveform` | `ch`, `start`, `end`, `bins` | `WaveformDto` — parallel `min`/`max` arrays, one entry per pixel |
| `play` | `start`, `end` | `PlaybackDto` — plays that range on the default output device |
| `pause_playback` | — | `PlaybackDto` |
| `resume_playback` | — | `PlaybackDto` |
| `stop_playback` | — | `PlaybackDto` |
| `playback_status` | — | `PlaybackDto` — transport state + playhead (polled per animation frame) |
| `edit` | `op`, `start`, `end` | `EditDto` — `normalize`/`fadeIn`/`fadeOut`/`silence`, in place across every channel |
| `trim` | `start`, `end` | `EditDto` — discard everything outside the range (changes `frames`) |
| `undo` | — | `EditDto` — reverse the most recent edit |
| `export` | `path`, `start`, `end`, `format` | `ExportDto` — write the range to a WAV (`pcm16`/`float32`) |
| `start_recording` | — | `RecordDto` — open the input device and begin capturing into a fresh cache |
| `stop_recording` | — | `AudioInfo` or `null` — seal the take as the open document (`null` = nothing captured) |
| `recording_status` | — | `RecordDto` — recording state + elapsed frames + dropped frames (polled while recording) |

Ranges are half-open and clamped to the document; an empty selection yields zeroed stats.
Values are **linear** — dB formatting stays in the UI (as in the `miniforge.html` prototype),
which also keeps the JSON free of non-finite floats.

### Edits

Edits write straight through the memory-mapped cache — that file *is* the document's backing
store — and apply to **every** channel: the Statistics channel selector chooses what you look
at, not what gets edited. Normalize computes one gain from the loudest channel and applies it
to all of them; a per-channel gain would equalise the channels and shift the stereo image.

Every path that changes samples must rebuild the summary pyramid of the channels it touched,
which is why edits go through `Document` rather than the cache directly — it is the only place
that cannot forget. A pyramid whose length still matches but whose contents are stale is
**undetectable** (`Analyzer::with_pyramid` only asserts the length) and would silently answer
every later query from pre-edit blocks.

Undo snapshots the original samples, so its cost follows the *selection*, not the file. That
still means "Select all → Normalize" on a 2-hour stereo file would snapshot ~2.8 GB, so the
stack is capped (`MAX_UNDO_BYTES`, 256 MB): older entries are evicted, and an edit whose own
snapshot exceeds the cap applies **without being undoable** rather than pretending —
`EditDto.lastUndoable` says which, and the UI says so out loud.

Trim is the exception: it changes the document's length, so it writes a fresh planar cache and
swaps onto it. The previous cache file becomes the undo record instead of being deleted, which
makes it reversible without copying the samples into memory; the entry deletes that file if it
is ever dropped unapplied. A trim also clears older undo entries, whose offsets index the
untrimmed document.

### Playback

`play` streams the range straight off the memory-mapped PCM cache — the same bytes `stats`
reads, with no second copy of the audio. The path is deliberately shaped around one rule:
**the audio callback must never block**. Reading an `mmap` can page-fault, so the callback
only ever pops from a lock-free `rtrb` ring that a feeder thread keeps full, and the feeder
holds an `Arc` on the PCM rather than the document lock — a selection drag takes that lock
thousands of times a minute, and waiting on it would be an audible dropout.

`Source` does the two conversions a real device needs: channel mapping (mono duplicated to
both speakers, stereo downmixed for a mono device) and resampling when the device cannot run
at the file's rate. `PlaybackDto.positionFrames` is the playhead in **source** frames, derived
from what has actually reached the device — not from what the feeder has queued, which runs
up to 250 ms ahead of the sound. `underruns` counts starved callbacks; non-zero means audible
dropouts. See `src-tauri/src/player.rs`.

### PCM cache files

`open_file` decodes into a cache file (`pcm-<pid>-<counter>.cache`) in the app cache
directory, memory-maps it, and deletes it when the document is closed or replaced. Those
files are large — roughly 1.2 GB for a 2-hour source — so an instance that dies without
running `Drop` (SIGKILL, force-quit, `panic=abort`) would leak one permanently. To prevent
that, startup sweeps the cache directory and reaps caches whose owning process is gone; a
concurrently running instance's caches are left alone. See `src-tauri/src/cache.rs`.

## Web UI

`ui/` is the `miniforge.html` design ported onto the IPC commands above. It holds **no
samples**: the document lives in Rust, and the UI only asks for what it needs to paint —
`waveform` for the envelope of the visible range, `stats` for the current selection. That is
what makes a multi-hour file behave like a short one.

| File | Role |
|---|---|
| `ui/index.html` | Markup + styles only |
| `ui/lib.js` | Pure logic — formatting, dB conversion, view/pixel geometry, request coalescing. No DOM, no IPC, so it is unit-tested under plain Node |
| `ui/app.js` | DOM + IPC wiring (the part that needs a real webview) |

Two rules keep a selection drag at 60 fps, and both are load-bearing:

1. **The envelope is only refetched when the view changes** (zoom/scroll/resize) — never per
   drag frame. A full-width `waveform` redraw costs one range query per bin (~4 ms), while a
   `stats` query is ~1.2 µs. The selection lives on its own overlay canvas stacked above the
   envelope, so dragging repaints only the overlay. A 200-mouse-move drag issues **zero**
   `waveform` calls.
2. **Stats requests are coalesced** (`createCoalescer`): at most one in flight, always
   finishing with the newest selection. Superseded requests are dropped rather than queued,
   so the panel cannot lag behind the cursor. A 200-mouse-move drag issues **2** `stats` calls.

Never request more bins than the view has samples (`binsForView`): the backend fills a bin
containing no sample with `(0, 0)`, so asking for a bin per pixel while zoomed in past one
sample per pixel makes the envelope collapse onto the zero line and vanish.

The playhead follows the same two rules: it is drawn on the overlay (never the envelope), and
its position is **polled** from `playback_status` on `requestAnimationFrame` rather than
pushed — so the UI asks exactly as often as it can paint, and not at all while the window is
hidden. The position always comes from the backend; the UI never extrapolates it from a
timer, because only the audio callback knows what has actually been heard.

Opening a file needs a real filesystem path, which the webview's `<input type="file">` cannot
give, so the shell registers `tauri-plugin-dialog`. The project has no JS bundler, so the UI
calls the plugin by its raw command name (`invoke('plugin:dialog|open', …)`) rather than
importing the plugin's npm package. The capability grants `dialog:allow-open`,
`dialog:allow-save` (the export destination picker) and also `dialog:allow-message` — the
plugin unconditionally rewires `window.alert` to `plugin:dialog|message`, so leaving that
ungranted would turn any stray `alert()` into an opaque permission error.

The transport plays exactly what the Statistics panel describes: the selection, or the whole
file when there is none. Play/pause is the `playBtn` toggle or the space bar.

Edits and export act on that same range. **Save WAV** (`⌘S`) exports the selection — or the
whole file when there is none — through the native save dialog; the backend reads straight
from the memory-mapped PCM and writes it interleaved via `hound`, so a multi-hour export
never materialises in RAM. It writes 16-bit PCM (the universally-playable interchange format);
the backend `export` command also supports lossless 32-bit float. The `sf_core::export` module
is pure and range-agnostic — it takes one already-sliced `&[f32]` per channel — so "export the
selection" is the same slicing the edits do.

**Record** (`recBtn`) captures the default input device natively, through `cpal`'s CoreAudio
backend — not the browser `MediaRecorder`, which does not exist in the macOS WKWebView the
shell runs in. It is the mirror image of playback: the realtime input callback only pushes the
device's frames into a lock-free ring, and a writer thread spills them per channel into a
`sf_core::CaptureWriter`, which seals a planar PCM cache on stop — the *same* on-disk layout a
decode produces. So a finished take is adopted as the open document with no decode step
(`AudioState::adopt_planar`), and behaves identically to an opened file: seamless statistics,
playback, edits, export. Nothing larger than one callback batch is ever held in RAM, so an
arbitrarily long take is bounded the same way a multi-hour file is. Recording replaces whatever
document is open only once it produces audio; a take that captured nothing leaves the current
document untouched.

macOS gates microphone access behind an `NSMicrophoneUsageDescription`, so the shell ships an
`src-tauri/Info.plist` that Tauri merges into the bundled `.app`; without it a bundled build
would be terminated on the first capture attempt. Recording is entirely backend-driven through
the app's own `start_recording`/`stop_recording` IPC commands, so it needs no extra capability
grant (unlike the dialog plugin above).

## Layout

```
SoundForge/
├─ Cargo.toml            # workspace
├─ package.json          # ui/ test harness only (no deps, no build step)
├─ crates/sf-core/       # pure-Rust analysis core (no GUI / no audio hardware; fully unit-tested)
├─ src-tauri/            # Tauri shell (depends on sf-core): IPC commands, audio document state, playback
├─ tests/ui/             # unit tests for ui/lib.js (node --test)
└─ ui/                   # web UI ported from miniforge.html
```

## Development

- Test-Driven Development: write failing tests first, then implement, then integration tests.
- Everything must be green before moving on: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`, `npm test`.
- **English only — including user-facing UI copy.** This supersedes the earlier rule that let
  UI strings stay in the `miniforge.html` prototype's language: the prototype was Russian, the
  port inherited that, and the product is English. `ui/` must contain no non-English copy.
- The UI tests need Node 18+ and nothing else — `npm test` runs `node --test` over `tests/ui/`;
  there are no dependencies to install and no build step.

### Build script

`scripts/build.sh` is the single entry point for the common build/verify tasks
(it runs from any working directory). Run `scripts/build.sh help` for the full
list; the most useful commands are:

| Command | What it does |
|---|---|
| `scripts/build.sh check` | `fmt --check`, `clippy -D warnings`, `test` + the `ui/` tests — the gate that must be green before pushing (mirrors CI) |
| `scripts/build.sh ui` | just the `ui/` tests (`node --test`) |
| `scripts/build.sh release` | optimized release build of the whole workspace (default) |
| `scripts/build.sh app` | bundle the native `.app`/`.dmg` via `cargo tauri build` |
| `scripts/build.sh dev` | run the app in watch mode via `cargo tauri dev` |

---

## Tasks

This task list is the **single source of truth** for the project. Format:
`- [ ] <index> — <description>`.

**Rules for maintaining this list:**
1. **Always tick the checkbox** (`[ ]` → `[x]`) immediately after a task is completed.
2. When an **urgent new task** appears, insert it **right after the last completed
   (checked) task**, then **renumber every task** so indices stay sequential with no gaps.
3. Indices are always contiguous starting at 1; renumber whenever a task is inserted or removed.

- [x] 1 — Cargo workspace scaffold (`Cargo.toml`, `crates/sf-core`)
- [x] 2 — `sf-core::agg` — associative aggregate monoid (`Agg`) with `combine`
- [x] 3 — `sf-core::summary` — summary pyramid + `Analyzer::range` (O(log N) range stats)
- [x] 4 — `sf-core::stats` — `RangeStats` (Peak/RMS/DC/min-max/zero-cross/frequency)
- [x] 5 — `sf-core::summary::waveform` — min/max-per-pixel bins for the waveform view
- [x] 6 — Tauri v2 shell (`src-tauri`) loading `ui/index.html`; IPC ping/pong
- [x] 7 — Logging: `tauri-plugin-log` (stdout + file + webview) + UI→Rust log bridge (`frontend_log`), console/error forwarding; hardened & instrumented record path
- [x] 8 — CI: GitHub Actions (`.github/workflows/ci.yml`) — `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` on `sf-core` (Ubuntu) and the full workspace (Apple-Silicon macOS)
- [x] 9 — Build script (`scripts/build.sh`) — one entry point for `check`/`build`/`release`/`app`/`dev`/`clean`, mirroring the CI "everything green" gate locally
- [x] 10 — `decode` — `symphonia` → on-disk PCM cache opened via `memmap2` (multi-channel)
- [x] 11 — Wire `Analyzer` over the mmap'd PCM; `stats`/`waveform` IPC commands
- [x] 12 — Reap orphaned PCM caches (`cache`) — startup sweep of caches left by an instance that died without running `Drop`
- [x] 13 — Port `miniforge.html` UI to `ui/index.html`; draw waveform + Statistics from IPC
- [x] 14 — Playback (`cpal` output + `rtrb` ring buffer), play selection
- [x] 15 — Recording (`cpal` input, native) into the PCM cache — replaces the browser MediaRecorder path unavailable in WKWebView; needs `NSMicrophoneUsageDescription`
- [x] 16 — Edits + undo (`normalize`/`fade in`/`fade out`/`silence`/`trim`) over the PCM cache
- [x] 17 — WAV export (`hound`) of selection or whole file
- [ ] 18 — Seamless benchmark: 2-hour (~1.2 GB) file, stats update < 5 ms/drag, RAM stable
- [ ] 19 — `cargo tauri build` → signed `.app`/`.dmg` for Apple Silicon
