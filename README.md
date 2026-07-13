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
| UI | Existing `miniforge.html` design (vanilla JS + Canvas) ported to call Rust via IPC |
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

## Layout

```
SoundForge/
├─ Cargo.toml            # workspace
├─ crates/sf-core/       # pure-Rust analysis core (no GUI / no audio hardware; fully unit-tested)
├─ src-tauri/            # Tauri binary (depends on sf-core)  — not yet scaffolded
└─ ui/                   # web UI ported from miniforge.html  — not yet created
```

## Development

- Test-Driven Development: write failing tests first, then implement, then integration tests.
- Everything must be green before moving on: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- **All documentation and docstrings are in English only.**

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
- [ ] 9 — `decode` — `symphonia` → on-disk PCM cache opened via `memmap2` (multi-channel)
- [ ] 10 — Wire `Analyzer` over the mmap'd PCM; `stats`/`waveform` IPC commands
- [ ] 11 — Port `miniforge.html` UI to `ui/index.html`; draw waveform + Statistics from IPC
- [ ] 12 — Playback (`cpal` output + `rtrb` ring buffer), play selection
- [ ] 13 — Recording (`cpal` input, native) into the PCM cache — replaces the browser MediaRecorder path unavailable in WKWebView; needs `NSMicrophoneUsageDescription`
- [ ] 14 — Edits + undo (`normalize`/`fade in`/`fade out`/`silence`/`trim`) over the PCM cache
- [ ] 15 — WAV export (`hound`) of selection or whole file
- [ ] 16 — Seamless benchmark: 2-hour (~1.2 GB) file, stats update < 5 ms/drag, RAM stable
- [ ] 17 — `cargo tauri build` → signed `.app`/`.dmg` for Apple Silicon
