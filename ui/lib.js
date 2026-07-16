// Pure UI logic: formatting, view/pixel geometry and request scheduling.
//
// This module deliberately touches neither the DOM nor the Tauri IPC bridge, so all of it
// is unit-testable under plain Node (see `tests/ui/lib.test.js`). `app.js` holds the DOM
// and IPC wiring that cannot be tested without a webview.

/// Upper bound on waveform bins per request. Must match `MAX_BINS` in
/// `src-tauri/src/audio.rs`: the backend rejects a larger request with an error.
export const MAX_BINS = 8192;

/// Narrowest view the user can zoom to, in samples. At this point individual samples are
/// several pixels apart; zooming further only magnifies the interpolation.
export const MIN_VIEW_SAMPLES = 32;

/// Narrow no-break space, used as the thousands separator by `fmtCount`.
export const NNBSP = ' ';

export function clamp(v, lo, hi) {
  return Math.max(lo, Math.min(hi, v));
}

/// Linear amplitude to a dB display string.
///
/// The backend sends linear values on purpose — `linear_to_db(0.0)` is `-inf`, which is not
/// representable in JSON — so this conversion is the UI's job, as in the `miniforge.html`
/// prototype.
export function db(x) {
  if (x <= 0) return '-∞';
  const d = 20 * Math.log10(x);
  return (d > 0 ? '+' : '') + d.toFixed(2);
}

/// Seconds to `s.mmm s`, or `m:ss.mmm` past a minute.
export function fmtTime(sec) {
  if (!isFinite(sec)) return '—';
  const m = Math.floor(sec / 60);
  const s = sec - m * 60;
  return (m > 0 ? m + ':' : '') + (m > 0 ? s.toFixed(3).padStart(6, '0') : s.toFixed(3)) + (m > 0 ? '' : ' s');
}

/// Group thousands with a narrow no-break space.
///
/// Hand-rolled rather than `toLocaleString`, so the output does not depend on the host's
/// ICU data — that would make the tests drift between machines and CI.
export function fmtCount(n) {
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, NNBSP);
}

/// Order two selection endpoints. A drag can run right-to-left.
export function normalizeSelection(a, b) {
  return a <= b ? { start: a, end: b } : { start: b, end: a };
}

export function hasSelection(sel) {
  return sel.end > sel.start;
}

/// The range the Statistics panel describes: the selection, or the whole document when
/// nothing is selected (the prototype's behaviour).
export function effectiveRange(sel, frames) {
  return hasSelection(sel) ? { start: sel.start, end: sel.end } : { start: 0, end: frames };
}

/// The whole document.
export function fitView(frames) {
  return { start: 0, end: frames };
}

/// Canvas pixel -> sample index, relative to the visible view.
export function viewToSample(px, width, view) {
  const frac = clamp(px / width, 0, 1);
  return Math.round(view.start + frac * (view.end - view.start));
}

/// Sample index -> canvas pixel. Inverse of `viewToSample`.
export function sampleToX(sample, width, view) {
  const len = view.end - view.start;
  if (len <= 0) return 0;
  return ((sample - view.start) / len) * width;
}

/// Bins to request for a view of `viewLen` samples across `width` pixels.
///
/// One bin per pixel, but never more bins than the view has samples. The backend fills a
/// bin that contains no sample with `(0, 0)`, so once the view is zoomed in past one sample
/// per pixel most bins come back empty and the envelope collapses onto the zero line. The
/// prototype had the same edge and handled it by holding the nearest sample; capping the
/// request keeps every bin populated instead, and the renderer widens each bin to suit.
export function binsForView(width, viewLen, maxBins = MAX_BINS) {
  return clamp(Math.min(Math.floor(width), viewLen), 1, maxBins);
}

/// Zoom the view by `factor` (< 1 zooms in) about `frac`, the anchor's position across the
/// view as a 0..1 fraction. The anchor sample stays under the cursor.
export function zoomAt(view, frac, factor, frames) {
  if (frames <= 0) return { start: 0, end: 0 };
  const len = view.end - view.start;
  const anchor = view.start + frac * len;
  const newLen = clamp(len * factor, Math.min(MIN_VIEW_SAMPLES, frames), frames);
  const start = clamp(anchor - frac * newLen, 0, frames - newLen);
  const s = Math.round(start);
  return { start: s, end: Math.min(frames, s + Math.round(newLen)) };
}

/// Pan the view by `deltaFrac` of its own width, clamped to the document.
export function panBy(view, deltaFrac, frames) {
  const len = view.end - view.start;
  const start = Math.round(clamp(view.start + deltaFrac * len, 0, frames - len));
  return { start, end: start + len };
}

/// The header's one-line document summary.
export function fmtMeta(name, info) {
  return `${name} · ${info.sampleRate} Hz · ${info.channels} ch · ${info.durationS.toFixed(3)} s`;
}

/// Build the Statistics panel's row model from a `StatsDto`.
///
/// This is where the backend's linear values become dB. Rows mirror the `miniforge.html`
/// prototype; `hi` marks the headline figures.
export function statsRows(dto) {
  const rows = [
    ['Selection length', fmtTime(dto.durationS), '', false],
    ['Samples', fmtCount(dto.n), '', false],
    ['Start position', fmtTime(dto.startS), '', false],
    ['Peak', db(dto.peak), 'dB', true],
    ['Max sample', db(Math.abs(dto.max)), 'dB', false],
    ['Max position', fmtTime(dto.maxPosS), '', false],
    ['Min sample', db(Math.abs(dto.min)), 'dB', false],
    ['Min position', fmtTime(dto.minPosS), '', false],
    ['RMS', db(dto.rms), 'dB', true],
    ['Mean (DC offset)', dto.dc.toFixed(6), '', false],
    ['DC offset', db(Math.abs(dto.dc)), 'dB', false],
    ['Zero crossings', fmtCount(dto.zeroCrossings), '', false],
    ['Frequency (zero-cross)', dto.freqHz.toFixed(1), 'Hz', true],
  ];
  return rows.map(([k, v, u, hi]) => ({ k, v, u, hi }));
}

/// What the transport button does next, given the backend's playback state.
///
/// `finished` (ran to the end of the selection) and `stopped` both restart from the
/// selection, which is why the backend distinguishes them but this does not.
export function nextPlaybackAction(state) {
  if (state === 'playing') return 'pause';
  if (state === 'paused') return 'resume';
  return 'play';
}

/// The transport button's label. Only an active stream offers a pause.
export function playLabel(state) {
  return state === 'playing' ? '⏸ Pause' : '▶ Play';
}

/// Whether the playhead should be drawn. A stopped transport has no position to show; a
/// paused or finished one does.
export function playheadVisible(state) {
  return state === 'playing' || state === 'paused' || state === 'finished';
}

/// Run `fn` with at most one call in flight, always finishing with the newest arguments.
///
/// A selection drag fires mouse-moves far faster than an IPC round-trip completes. Awaiting
/// each one in turn would build an unbounded backlog and leave the Statistics panel lagging
/// behind the cursor; only the newest selection is worth computing, so superseded calls are
/// dropped rather than queued. `onError` receives any rejection so that one failure (e.g.
/// the document closing mid-drag) cannot wedge the pump.
export function createCoalescer(fn, onError = () => {}) {
  let inFlight = false;
  let pending = null;

  async function pump() {
    if (inFlight || pending === null) return;
    const args = pending;
    pending = null;
    inFlight = true;
    try {
      await fn(...args);
    } catch (e) {
      onError(e);
    } finally {
      inFlight = false;
      pump();
    }
  }

  return (...args) => {
    pending = args;
    pump();
  };
}
