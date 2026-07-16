// DOM + IPC wiring for the SoundForge UI.
//
// The document lives in Rust: `open_file` decodes it into a memory-mapped PCM cache and
// builds a summary pyramid per channel, and this UI only ever asks for what it needs to
// paint — `waveform` for the envelope of the visible range, `stats` for the selection. No
// samples are ever held here, which is what lets a multi-hour file behave like a short one.
//
// All pure logic (formatting, view geometry, request coalescing) lives in `lib.js` so it
// can be unit-tested without a webview; this file is the part that needs a real DOM.

import {
  binsForView,
  createCoalescer,
  effectiveRange,
  fitView,
  fmtMeta,
  fmtTime,
  hasSelection,
  nextPlaybackAction,
  normalizeSelection,
  panBy,
  playLabel,
  playheadVisible,
  sampleToX,
  statsRows,
  viewToSample,
  zoomAt,
} from './lib.js';

// ---------- IPC bridge ----------

const TAURI = window.__TAURI__;

/// Invoke a Rust command. Rejects when running outside Tauri (e.g. this file opened
/// directly in a browser), so callers surface a real message instead of hanging.
function invoke(cmd, args) {
  if (!(TAURI && TAURI.core && typeof TAURI.core.invoke === 'function')) {
    return Promise.reject(new Error('IPC bridge unavailable (not running inside Tauri)'));
  }
  return TAURI.core.invoke(cmd, args);
}

/// Fire-and-forget log to the backend; never throws.
function sflog(level, ...args) {
  const msg = args
    .map((a) => {
      if (a instanceof Error) return a.stack || `${a.name}: ${a.message}`;
      if (a && typeof a === 'object') {
        try {
          return JSON.stringify(a);
        } catch {
          return String(a);
        }
      }
      return String(a);
    })
    .join(' ');
  try {
    invoke('frontend_log', { level, message: msg }).catch(() => {});
  } catch {
    /* bridge missing: nothing to do */
  }
}

// Mirror console.* to the backend logger while keeping normal console behaviour, so that
// webview-only failures land in the same log file as native ones.
for (const k of ['log', 'info', 'warn', 'error', 'debug']) {
  const orig = console[k] ? console[k].bind(console) : () => {};
  console[k] = (...a) => {
    orig(...a);
    sflog(k === 'log' ? 'info' : k, ...a);
  };
}
window.addEventListener('error', (e) => {
  sflog('error', 'window.onerror:', e.message, 'at', `${e.filename || '?'}:${e.lineno || '?'}:${e.colno || '?'}`, e.error || '');
});
window.addEventListener('unhandledrejection', (e) => {
  sflog('error', 'unhandledrejection:', e.reason || '(no reason)');
});

// ---------- elements ----------

const $ = (id) => document.getElementById(id);
const wrap = $('wavewrap');
const waveCanvas = $('wave');
const overlayCanvas = $('overlay');
const waveCtx = waveCanvas.getContext('2d');
const overlayCtx = overlayCanvas.getContext('2d');

// ---------- state ----------

/// The open document's geometry (`AudioInfo`), or null when nothing is open.
let info = null;
/// Visible range, in samples: {start, end}. Zoom/scroll move this.
let view = { start: 0, end: 0 };
/// Selection, in samples. start === end means "no selection".
let sel = { start: 0, end: 0 };
/// Channel the Statistics panel describes.
let statsCh = 0;
/// Cached `WaveformDto` per channel for the current view, so a selection drag can repaint
/// without re-querying the backend. See `redrawEnvelope`.
let envelopes = [];
/// Canvas size in CSS pixels.
let size = { w: 0, h: 0 };
let dragging = false;
/// Last `PlaybackDto` from the backend: what the transport is doing and where the playhead
/// is. The backend owns this — the UI never guesses the position from a timer, because only
/// the audio callback knows what has actually reached the device.
let playback = { state: 'stopped', positionFrames: 0, underruns: 0 };

// ---------- error banner ----------

function showError(msg) {
  const bar = $('errBar');
  bar.textContent = msg;
  bar.classList.add('on');
  sflog('error', msg);
}
function clearError() {
  $('errBar').classList.remove('on');
}

// ---------- canvas sizing ----------

function fitCanvases() {
  // Measure the canvas, not `wrap`: the canvases are `inset:0` inside a 1px border, so the
  // wrapper's border-box is 2px wider than the canvas actually is. Sizing the backing store
  // from the wrapper would both resample the drawing and skew every pixel<->sample mapping.
  const r = waveCanvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  for (const [c, ctx] of [
    [waveCanvas, waveCtx],
    [overlayCanvas, overlayCtx],
  ]) {
    c.width = Math.max(1, Math.round(r.width * dpr));
    c.height = Math.max(1, Math.round(r.height * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }
  size = { w: r.width, h: r.height };
}

// The palette is a static `:root` block, but getComputedStyle can force a style recalc and
// these are read several times per frame on the drag path — so resolve each var once.
const paletteCache = new Map();
function css(v) {
  let hit = paletteCache.get(v);
  if (hit === undefined) {
    hit = getComputedStyle(document.documentElement).getPropertyValue(v).trim();
    paletteCache.set(v, hit);
  }
  return hit;
}

// ---------- envelope (slow path: zoom / scroll / resize only) ----------

// Fetching the envelope costs one O(log N) range query per bin — a few milliseconds for a
// full-width redraw. That is fine for its real triggers, but it must never run per drag
// frame: a stats query is ~1.2 us, so the Statistics panel can follow the cursor while the
// envelope stays put. `refreshEnvelope` is therefore only called when the *view* changes,
// and the drag path repaints from `envelopes` instead.
const refreshEnvelope = createCoalescer(async () => {
  if (!info) return;
  const bins = binsForView(size.w, view.end - view.start);
  const { start, end } = view;
  // The channels are independent: fetch them together rather than paying one serial
  // round-trip each, which a 6-channel document would feel on every wheel tick.
  envelopes = await Promise.all(
    Array.from({ length: info.channels }, (_, ch) => invoke('waveform', { ch, start, end, bins })),
  );
  drawEnvelope();
}, (e) => showError(`Не удалось построить волну: ${e.message || e}`));

function drawEnvelope() {
  waveCtx.clearRect(0, 0, size.w, size.h);
  if (!info || envelopes.length === 0) return;

  const laneH = size.h / info.channels;
  waveCtx.lineWidth = 1;

  for (let ch = 0; ch < info.channels; ch++) {
    const env = envelopes[ch];
    if (!env) continue;
    const mid = laneH * ch + laneH / 2;
    const amp = (laneH / 2) * 0.95;

    // Lane separator above every lane but the first.
    if (ch > 0) {
      waveCtx.strokeStyle = css('--line');
      waveCtx.beginPath();
      waveCtx.moveTo(0, laneH * ch);
      waveCtx.lineTo(size.w, laneH * ch);
      waveCtx.stroke();
    }

    // Zero line.
    waveCtx.strokeStyle = css('--line');
    waveCtx.beginPath();
    waveCtx.moveTo(0, mid);
    waveCtx.lineTo(size.w, mid);
    waveCtx.stroke();

    // Envelope: one vertical min..max segment per bin. Position the bins by the range the
    // DTO was actually computed for rather than assuming it fills the canvas — during a
    // zoom the cached envelope is briefly one view behind, and mapping it through the
    // current view keeps it in the right place (just coarser) until the refetch lands.
    const xStart = sampleToX(env.start, size.w, view);
    const scale = (sampleToX(env.end, size.w, view) - xStart) / env.bins;
    waveCtx.strokeStyle = css('--amber');
    // A bin spanning several pixels would otherwise draw as a comb of hairlines.
    waveCtx.lineWidth = Math.max(1, scale);
    waveCtx.beginPath();
    for (let i = 0; i < env.bins; i++) {
      const x = xStart + (i + 0.5) * scale;
      // A bin whose min and max coincide would be a zero-length line and paint nothing;
      // nudge it so a flat or single-sample stretch still shows up.
      const yMax = mid - env.max[i] * amp;
      const yMin = mid - env.min[i] * amp;
      waveCtx.moveTo(x, yMax);
      waveCtx.lineTo(x, yMin === yMax ? yMin + 0.5 : yMin);
    }
    waveCtx.stroke();
    waveCtx.lineWidth = 1;
  }
}

// ---------- overlay (fast path: every drag frame) ----------

function drawOverlay() {
  overlayCtx.clearRect(0, 0, size.w, size.h);
  if (!info) return;

  if (hasSelection(sel)) {
    const x1 = sampleToX(sel.start, size.w, view);
    const x2 = sampleToX(sel.end, size.w, view);

    overlayCtx.fillStyle = css('--sel');
    overlayCtx.fillRect(x1, 0, x2 - x1, size.h);

    overlayCtx.strokeStyle = css('--amber');
    overlayCtx.lineWidth = 1;
    overlayCtx.beginPath();
    for (const x of [x1, x2]) {
      overlayCtx.moveTo(Math.round(x) + 0.5, 0);
      overlayCtx.lineTo(Math.round(x) + 0.5, size.h);
    }
    overlayCtx.stroke();
  }

  drawPlayhead();
}

/// The playhead, on the overlay rather than the envelope: it moves every animation frame,
/// and the envelope must not be repainted at that rate (see `refreshEnvelope`).
function drawPlayhead() {
  if (!playheadVisible(playback.state)) return;
  const x = sampleToX(playback.positionFrames, size.w, view);
  // Zoomed in elsewhere, the playhead is simply off-screen.
  if (x < 0 || x > size.w) return;

  overlayCtx.strokeStyle = css('--green');
  overlayCtx.lineWidth = 1;
  overlayCtx.beginPath();
  overlayCtx.moveTo(Math.round(x) + 0.5, 0);
  overlayCtx.lineTo(Math.round(x) + 0.5, size.h);
  overlayCtx.stroke();
}

// ---------- statistics (fast path: every drag frame) ----------

const requestStats = createCoalescer(async (ch, start, end) => {
  const dto = await invoke('stats', { ch, start, end });
  renderStats(dto);
}, (e) => showError(`Не удалось посчитать статистику: ${e.message || e}`));

function renderStats(dto) {
  $('statsBody').innerHTML = statsRows(dto)
    .map(
      (r) =>
        `<div class="row${r.hi ? ' hi' : ''}"><span class="k">${r.k}</span>` +
        `<span class="v">${r.v}${r.u ? `<span class="u">${r.u}</span>` : ''}</span></div>`,
    )
    .join('');
}

/// Push the current selection to the backend and refresh the readouts. Cheap by design:
/// this runs on every mouse-move of a drag.
function updateStats() {
  if (!info) return;
  const r = effectiveRange(sel, info.frames);
  requestStats(statsCh, r.start, r.end);

  const sr = info.sampleRate;
  const startS = r.start / sr;
  const endS = r.end / sr;
  const durS = (r.end - r.start) / sr;

  $('scopeNote').textContent = hasSelection(sel)
    ? `Область: выделение ${fmtTime(startS)} – ${fmtTime(endS)}`
    : 'Область: весь файл (выдели участок мышкой для локальной статистики)';
  $('timeReadout').textContent = hasSelection(sel)
    ? `${fmtTime(startS)} → ${fmtTime(endS)}  (${fmtTime(durS)})`
    : `весь файл · ${fmtTime(durS)}`;
}

// ---------- playback ----------

/// Adopt a `PlaybackDto` from the backend and reflect it on screen.
function applyPlayback(dto) {
  const wasClean = playback.underruns === 0;
  playback = dto;
  $('playBtn').textContent = playLabel(dto.state);
  // Dropouts are inaudible in a log file but obvious in the ear; record the first one so a
  // "playback sounds glitchy" report has evidence behind it.
  if (wasClean && dto.underruns > 0) {
    sflog('warn', `playback underrun: ${dto.underruns} starved callback(s)`);
  }
  drawOverlay();
}

/// Follow the playhead until playback stops.
///
/// The position is polled rather than pushed: it is a couple of atomic loads on the backend,
/// and tying it to `requestAnimationFrame` means the UI asks exactly as often as it can draw
/// — no faster, and not at all when the window is hidden. The loop ends by itself as soon as
/// the state leaves `playing`, so a finished or paused transport costs nothing.
let polling = false;
async function followPlayhead() {
  if (polling) return;
  polling = true;
  try {
    while (playback.state === 'playing') {
      await new Promise((r) => requestAnimationFrame(r));
      applyPlayback(await invoke('playback_status'));
    }
  } catch (e) {
    sflog('warn', 'playback_status failed:', e);
  } finally {
    polling = false;
  }
}

/// The transport button: play the selection (or the whole file), or pause/resume a stream
/// that is already running.
async function transport() {
  if (!info) return;
  try {
    const action = nextPlaybackAction(playback.state);
    if (action === 'pause') {
      applyPlayback(await invoke('pause_playback'));
    } else if (action === 'resume') {
      applyPlayback(await invoke('resume_playback'));
      followPlayhead();
    } else {
      // Play exactly what the Statistics panel describes.
      const r = effectiveRange(sel, info.frames);
      applyPlayback(await invoke('play', { start: r.start, end: r.end }));
      followPlayhead();
    }
  } catch (e) {
    showError(`Не удалось воспроизвести: ${e.message || e}`);
  }
}

/// Stop playback and forget the playhead. Used when the document goes away.
async function stopPlayback() {
  try {
    applyPlayback(await invoke('stop_playback'));
  } catch (e) {
    sflog('warn', 'stop_playback failed:', e);
    playback = { state: 'stopped', positionFrames: 0, underruns: 0 };
    $('playBtn').textContent = playLabel('stopped');
  }
}

// ---------- open / close ----------

async function openPath(path) {
  clearError();
  const prevMeta = $('fileMeta').textContent;
  $('fileMeta').textContent = 'открываю…';

  let opened;
  try {
    // The only O(n) step: decode + build the pyramids. Everything after this is O(log N).
    opened = await invoke('open_file', { path });
  } catch (e) {
    // `AudioState::open` decodes before it swaps the document, so a failed open leaves the
    // previous one intact on the backend. Restore the previous display rather than
    // half-clearing the UI: dropping `info` here would leave a stale waveform on screen,
    // controls enabled but inert, and the two sides disagreeing about what is open.
    $('fileMeta').textContent = prevMeta;
    showError(`Не удалось открыть файл: ${e.message || e}`);
    return;
  }
  info = opened;

  const name = path.split('/').pop() || path;
  $('fileMeta').textContent = fmtMeta(name, info);
  sflog('info', `opened ${path}: ${info.channels} ch, ${info.sampleRate} Hz, ${info.frames} frames`);

  view = fitView(info.frames);
  sel = { start: 0, end: 0 };
  statsCh = 0;
  envelopes = [];
  // `open_file` stops playback of the outgoing document on the backend; mirror that here so
  // the button and the playhead do not describe a file that is no longer open.
  playback = { state: 'stopped', positionFrames: 0, underruns: 0 };
  $('playBtn').textContent = playLabel(playback.state);

  buildChannelSelector();
  setDocumentControlsEnabled(true);
  $('hint').style.display = 'none';

  fitCanvases();
  refreshEnvelope();
  drawOverlay();
  updateStats();
}

function buildChannelSelector() {
  const s = $('chSel');
  s.innerHTML = '';
  for (let ch = 0; ch < info.channels; ch++) {
    const o = document.createElement('option');
    o.value = String(ch);
    o.textContent = info.channels === 1 ? 'моно' : `канал ${ch + 1}`;
    s.appendChild(o);
  }
  s.value = '0';
  s.disabled = info.channels < 2;
}

function setDocumentControlsEnabled(on) {
  for (const id of ['playBtn', 'selAllBtn', 'clrSelBtn', 'fitBtn', 'zoomSelBtn', 'closeBtn']) {
    $(id).disabled = !on;
  }
}

async function closeFile() {
  // Before `close_file`, so the transport is already down when the document goes: playback
  // holds its own handle on the samples and would otherwise keep playing a closed file.
  await stopPlayback();
  await invoke('close_file').catch((e) => sflog('warn', 'close_file failed:', e));
  info = null;
  envelopes = [];
  sel = { start: 0, end: 0 };
  setDocumentControlsEnabled(false);
  const chSel = $('chSel');
  chSel.innerHTML = '';
  chSel.disabled = true;
  $('fileMeta').textContent = 'нет файла';
  $('statsBody').innerHTML = '<div class="row"><span class="k">Загрузи звук, чтобы увидеть цифры</span></div>';
  $('scopeNote').textContent = 'Область: —';
  $('timeReadout').textContent = '—';
  $('hint').style.display = '';
  fitCanvases();
  waveCtx.clearRect(0, 0, size.w, size.h);
  overlayCtx.clearRect(0, 0, size.w, size.h);
}

/// Native file picker. The backend needs a real filesystem path, which an `<input type=file>`
/// cannot provide — so this goes through the dialog plugin. It is called by its raw command
/// name because the project has no JS bundler to import the plugin's npm package.
async function pickFile() {
  try {
    const path = await invoke('plugin:dialog|open', {
      options: {
        title: 'Открыть аудиофайл',
        multiple: false,
        directory: false,
        filters: [
          {
            name: 'Аудио',
            extensions: ['wav', 'wave', 'flac', 'mp3', 'm4a', 'aac', 'ogg', 'oga', 'opus', 'aiff', 'aif', 'caf', 'alac', 'mkv', 'webm'],
          },
        ],
      },
    });
    if (path) await openPath(path);
  } catch (e) {
    showError(`Не удалось открыть диалог выбора файла: ${e.message || e}`);
  }
}

// ---------- view changes ----------

function setView(v) {
  view = v;
  drawOverlay();
  refreshEnvelope();
}

function zoomToSelection() {
  if (!info || !hasSelection(sel)) return;
  setView({ start: sel.start, end: sel.end });
}

// ---------- selection interaction ----------

function selectAll() {
  if (!info) return;
  sel = { start: 0, end: info.frames };
  drawOverlay();
  updateStats();
}

function clearSel() {
  sel = { start: 0, end: 0 };
  drawOverlay();
  updateStats();
}

let anchorSample = 0;

overlayCanvas.addEventListener('mousedown', (e) => {
  if (!info) return;
  dragging = true;
  anchorSample = viewToSample(e.offsetX, size.w, view);
  sel = { start: anchorSample, end: anchorSample };
  drawOverlay();
  updateStats();
});

window.addEventListener('mousemove', (e) => {
  if (!dragging || !info) return;
  const r = overlayCanvas.getBoundingClientRect();
  const cur = viewToSample(e.clientX - r.left, size.w, view);
  sel = normalizeSelection(anchorSample, cur);
  // Only the overlay and the numbers move: the envelope is untouched, so this stays at
  // 60 fps regardless of how long the selection or the file is.
  drawOverlay();
  updateStats();
});

window.addEventListener('mouseup', () => {
  dragging = false;
});

// Wheel: zoom about the cursor; shift+wheel pans.
wrap.addEventListener(
  'wheel',
  (e) => {
    if (!info) return;
    e.preventDefault();
    const r = overlayCanvas.getBoundingClientRect();
    if (e.shiftKey) {
      setView(panBy(view, (e.deltaY || e.deltaX) / size.w, info.frames));
    } else {
      const frac = (e.clientX - r.left) / size.w;
      setView(zoomAt(view, frac, Math.exp(e.deltaY * 0.002), info.frames));
    }
  },
  { passive: false },
);

// ---------- wiring ----------

$('openBtn').addEventListener('click', pickFile);
$('closeBtn').addEventListener('click', closeFile);
$('playBtn').addEventListener('click', transport);
$('selAllBtn').addEventListener('click', selectAll);
$('clrSelBtn').addEventListener('click', clearSel);
$('fitBtn').addEventListener('click', () => info && setView(fitView(info.frames)));
$('zoomSelBtn').addEventListener('click', zoomToSelection);
$('chSel').addEventListener('change', (e) => {
  statsCh = Number(e.target.value);
  updateStats();
});

window.addEventListener('keydown', (e) => {
  if (!info) return;
  if ((e.metaKey || e.ctrlKey) && e.key === 'a') {
    e.preventDefault();
    selectAll();
  }
  if (e.key === 'Escape') clearSel();
  if (e.key === ' ') {
    // Otherwise the browser scrolls, and a focused button would fire its click too — which
    // would toggle the transport twice.
    e.preventDefault();
    transport();
  }
});

let resizeTimer = null;
window.addEventListener('resize', () => {
  if (!info) {
    fitCanvases();
    return;
  }
  // Debounce: a resize drag would otherwise re-query the envelope on every frame.
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    fitCanvases();
    drawOverlay();
    refreshEnvelope();
  }, 80);
});

// Native drag & drop. Tauri intercepts file drops at the window level, so the HTML5
// drop events never fire in the webview and these events are the only way to get a path.
if (TAURI && TAURI.event && typeof TAURI.event.listen === 'function') {
  const border = (on) => {
    wrap.style.borderColor = on ? css('--amber') : css('--line');
  };
  TAURI.event.listen('tauri://drag-enter', () => border(true));
  TAURI.event.listen('tauri://drag-over', () => border(true));
  TAURI.event.listen('tauri://drag-leave', () => border(false));
  TAURI.event.listen('tauri://drag-drop', (e) => {
    border(false);
    const p = e.payload && e.payload.paths && e.payload.paths[0];
    if (p) openPath(p);
  });
}

fitCanvases();
sflog('info', `UI booted. tauri=${!!TAURI} dpr=${window.devicePixelRatio || 1}`);
