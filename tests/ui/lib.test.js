// Unit tests for the UI's pure logic (`ui/lib.js`).
//
// Everything the waveform/Statistics port needs that is not DOM or IPC lives in `lib.js`
// precisely so it can be tested here, with no webview and no browser. `ui/app.js` is the
// thin DOM+IPC wiring on top and is exercised by the app itself.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  MAX_BINS,
  METER_FLOOR_DB,
  MIN_VIEW_SAMPLES,
  NNBSP,
  binsForView,
  clamp,
  createCoalescer,
  db,
  decayMeter,
  effectiveRange,
  exportFileName,
  fitView,
  fmtCount,
  fmtMeta,
  fmtTime,
  hasSelection,
  isClipping,
  meterActive,
  meterDbLabel,
  meterFraction,
  nextPlaybackAction,
  normalizeSelection,
  panBy,
  playLabel,
  playheadVisible,
  recLabel,
  recMeta,
  sampleToX,
  statsRows,
  viewToSample,
  zoomAt,
} from '../../ui/lib.js';

// ---------- clamp ----------

test('clamp bounds a value to the inclusive range', () => {
  assert.equal(clamp(5, 0, 10), 5);
  assert.equal(clamp(-1, 0, 10), 0);
  assert.equal(clamp(11, 0, 10), 10);
});

// ---------- db ----------

test('db returns -inf marker for silence, matching the prototype', () => {
  // The backend sends LINEAR values on purpose: linear_to_db(0) is -inf, which is not
  // valid JSON. The UI is what turns that into a display string.
  assert.equal(db(0), '-∞');
  assert.equal(db(-0.5), '-∞');
});

test('db converts linear amplitude to dB with a sign prefix above unity', () => {
  assert.equal(db(1), '0.00');
  assert.equal(db(0.5), '-6.02');
  assert.equal(db(2), '+6.02');
});

// ---------- fmtTime ----------

test('fmtTime renders sub-minute values in seconds', () => {
  assert.equal(fmtTime(0), '0.000 s');
  assert.equal(fmtTime(0.5), '0.500 s');
});

test('fmtTime switches to m:ss.mmm past a minute', () => {
  assert.equal(fmtTime(65.25), '1:05.250');
  assert.equal(fmtTime(600), '10:00.000');
});

test('fmtTime renders a dash for non-finite input', () => {
  assert.equal(fmtTime(Infinity), '—');
  assert.equal(fmtTime(NaN), '—');
});

// ---------- fmtCount ----------

test('fmtCount groups thousands deterministically without relying on ICU', () => {
  // Hand-rolled rather than toLocaleString: the grouping must not depend on the Node
  // build's ICU data, or the expected strings drift between dev machines and CI.
  assert.equal(fmtCount(0), '0');
  assert.equal(fmtCount(999), '999');
  assert.equal(fmtCount(1000), `1${NNBSP}000`);
  assert.equal(fmtCount(1234567), `1${NNBSP}234${NNBSP}567`);
});

// ---------- selection ----------

test('normalizeSelection orders its endpoints', () => {
  assert.deepEqual(normalizeSelection(10, 20), { start: 10, end: 20 });
  assert.deepEqual(normalizeSelection(20, 10), { start: 10, end: 20 });
  assert.deepEqual(normalizeSelection(7, 7), { start: 7, end: 7 });
});

test('hasSelection is false for an empty (collapsed) selection', () => {
  assert.equal(hasSelection({ start: 5, end: 5 }), false);
  assert.equal(hasSelection({ start: 5, end: 6 }), true);
});

test('effectiveRange falls back to the whole document when nothing is selected', () => {
  // Mirrors the prototype: with no selection the Statistics panel describes the whole file.
  assert.deepEqual(effectiveRange({ start: 0, end: 0 }, 1000), { start: 0, end: 1000 });
  assert.deepEqual(effectiveRange({ start: 10, end: 20 }, 1000), { start: 10, end: 20 });
});

// ---------- view <-> pixel mapping ----------

test('viewToSample maps pixels across the visible view, not the whole file', () => {
  const view = { start: 1000, end: 2000 };
  assert.equal(viewToSample(0, 100, view), 1000);
  assert.equal(viewToSample(50, 100, view), 1500);
  assert.equal(viewToSample(100, 100, view), 2000);
});

test('viewToSample clamps pixels outside the canvas', () => {
  const view = { start: 1000, end: 2000 };
  assert.equal(viewToSample(-20, 100, view), 1000);
  assert.equal(viewToSample(500, 100, view), 2000);
});

test('sampleToX is the inverse of viewToSample', () => {
  const view = { start: 1000, end: 2000 };
  for (const px of [0, 25, 50, 99]) {
    assert.equal(sampleToX(viewToSample(px, 100, view), 100, view), px);
  }
});

// ---------- bins ----------

test('binsForView never asks for more bins than the view has samples', () => {
  // Regression: zoomed in past one sample per pixel, asking for a bin per pixel makes the
  // backend emit an empty (0,0) bin for every pixel column that contains no sample. Those
  // render as a flat zero line, so the waveform silently disappears at deep zoom. Capping
  // the request at the sample count keeps every bin populated.
  assert.equal(binsForView(950, 32), 32);
  assert.equal(binsForView(950, 1), 1);
});

test('binsForView still asks for one bin per pixel when samples are plentiful', () => {
  assert.equal(binsForView(950, 240000), 950);
  assert.equal(binsForView(1920.9, 240000), 1920);
});

test('binsForView respects MAX_BINS and never returns zero', () => {
  // The backend rejects more than MAX_BINS with an error; the UI must not produce one.
  assert.equal(binsForView(MAX_BINS + 5000, 1e9), MAX_BINS);
  assert.equal(binsForView(0, 0), 1);
  assert.equal(binsForView(-10, 100), 1);
});

// ---------- zoom / pan ----------

test('fitView spans the whole document', () => {
  assert.deepEqual(fitView(48000), { start: 0, end: 48000 });
});

test('zoomAt keeps the anchor point stationary', () => {
  const view = { start: 0, end: 1000 };
  // Zoom to half the length anchored at the middle: the middle sample stays put.
  const z = zoomAt(view, 0.5, 0.5, 1000);
  assert.deepEqual(z, { start: 250, end: 750 });
});

test('zoomAt clamps to the document when zooming out', () => {
  const z = zoomAt({ start: 250, end: 750 }, 0.5, 100, 1000);
  assert.deepEqual(z, { start: 0, end: 1000 });
});

test('zoomAt refuses to zoom in past MIN_VIEW_SAMPLES', () => {
  const z = zoomAt({ start: 0, end: 1000 }, 0.5, 0.0001, 1000);
  assert.equal(z.end - z.start, MIN_VIEW_SAMPLES);
});

test('zoomAt anchored at an edge does not walk outside the document', () => {
  const left = zoomAt({ start: 0, end: 1000 }, 0, 0.5, 1000);
  assert.equal(left.start, 0);
  const right = zoomAt({ start: 0, end: 1000 }, 1, 0.5, 1000);
  assert.equal(right.end, 1000);
});

test('panBy shifts the view and clamps at both ends', () => {
  assert.deepEqual(panBy({ start: 400, end: 600 }, 0.5, 1000), { start: 500, end: 700 });
  assert.deepEqual(panBy({ start: 400, end: 600 }, -100, 1000), { start: 0, end: 200 });
  assert.deepEqual(panBy({ start: 400, end: 600 }, 100, 1000), { start: 800, end: 1000 });
});

// ---------- meta line ----------

test('fmtMeta summarises the document geometry', () => {
  const info = { path: '/tmp/x.wav', channels: 2, sampleRate: 48000, frames: 96000, durationS: 2 };
  const s = fmtMeta('x.wav', info);
  assert.match(s, /x\.wav/);
  assert.match(s, /48000 Hz/);
  assert.match(s, /2 ch/);
  assert.match(s, /2\.000 s/);
});

// ---------- exportFileName ----------

test('exportFileName swaps the source extension for .wav', () => {
  assert.equal(exportFileName('song.mp3', false), 'song.wav');
  assert.equal(exportFileName('take.flac', false), 'take.wav');
});

test('exportFileName marks a selection export', () => {
  assert.equal(exportFileName('song.mp3', true), 'song-selection.wav');
});

test('exportFileName suffixes a .wav source so it cannot propose overwriting it', () => {
  assert.equal(exportFileName('master.wav', false), 'master.wav');
  assert.equal(exportFileName('master.wav', true), 'master-selection.wav');
});

test('exportFileName falls back to a default when there is no name', () => {
  assert.equal(exportFileName('', false), 'audio.wav');
  assert.equal(exportFileName(undefined, false), 'audio.wav');
  assert.equal(exportFileName('.hidden', false), 'audio.wav');
});

test('exportFileName keeps dotted stems intact', () => {
  assert.equal(exportFileName('my.mix.v2.aiff', false), 'my.mix.v2.wav');
});

// ---------- statsRows ----------

const DTO = {
  channel: 0,
  n: 48000,
  startS: 1.5,
  durationS: 1,
  peak: 0.5,
  min: -0.5,
  minPosS: 1.6,
  max: 0.25,
  maxPosS: 1.7,
  rms: 0.25,
  dc: 0.001,
  zeroCrossings: 1200,
  freqHz: 600,
};

test('statsRows converts the linear DTO to dB in the UI', () => {
  const rows = statsRows(DTO);
  const byKey = Object.fromEntries(rows.map((r) => [r.k, r.v]));
  assert.equal(byKey['Peak'], '-6.02'); // db(0.5)
  assert.equal(byKey['RMS'], '-12.04'); // db(0.25)
});

test('statsRows uses absolute values for min/max in dB', () => {
  // dB of a negative sample is meaningless; the magnitude is what is shown.
  const rows = statsRows({ ...DTO, min: -0.5, max: 0.25 });
  const byKey = Object.fromEntries(rows.map((r) => [r.k, r.v]));
  assert.equal(byKey['Min sample'], '-6.02');
  assert.equal(byKey['Max sample'], '-12.04');
});

test('statsRows survives a zeroed (silent / empty) selection without producing NaN', () => {
  // The backend returns zeroed stats rather than an error for an empty selection, so the
  // UI is queried freely mid-drag and must render those cleanly.
  const zero = {
    channel: 0, n: 0, startS: 0, durationS: 0, peak: 0, min: 0, minPosS: 0,
    max: 0, maxPosS: 0, rms: 0, dc: 0, zeroCrossings: 0, freqHz: 0,
  };
  const rows = statsRows(zero);
  for (const r of rows) {
    assert.equal(typeof r.v, 'string');
    assert.doesNotMatch(r.v, /NaN|undefined|Infinity/);
  }
});

test('statsRows highlights the headline figures', () => {
  const rows = statsRows(DTO);
  const hi = rows.filter((r) => r.hi).map((r) => r.k);
  assert.deepEqual(hi, ['Peak', 'RMS', 'Frequency (zero-cross)']);
});

test('statsRows renders every value as a string', () => {
  for (const r of statsRows(DTO)) {
    assert.equal(typeof r.k, 'string');
    assert.equal(typeof r.v, 'string');
    assert.equal(typeof r.u, 'string');
  }
});

// ---------- coalescer ----------

test('createCoalescer runs one call at a time and drops superseded ones', async () => {
  // The drag handler fires far faster than an IPC round-trip. Queuing every mouse-move
  // would build an unbounded backlog and lag the Statistics panel behind the cursor;
  // only the newest selection matters.
  const seen = [];
  let release;
  const gate = new Promise((r) => { release = r; });
  let first = true;

  const send = createCoalescer(async (v) => {
    seen.push(v);
    if (first) { first = false; await gate; }
  });

  send(1); // starts immediately, blocks on the gate
  send(2); // superseded...
  send(3); // ...by this one
  send(4); // ...and this one
  release();
  await new Promise((r) => setTimeout(r, 10));

  assert.deepEqual(seen, [1, 4]);
});

test('createCoalescer keeps running after a failure', async () => {
  // An IPC error (e.g. the document was closed mid-drag) must not wedge the coalescer.
  const seen = [];
  const errors = [];
  const send = createCoalescer(async (v) => {
    seen.push(v);
    if (v === 1) throw new Error('boom');
  }, (e) => errors.push(e));

  send(1);
  await new Promise((r) => setTimeout(r, 5));
  send(2);
  await new Promise((r) => setTimeout(r, 5));

  assert.deepEqual(seen, [1, 2]);
  assert.equal(errors.length, 1);
  assert.match(errors[0].message, /boom/);
});

test('createCoalescer delivers the final value even under a burst', async () => {
  const seen = [];
  const send = createCoalescer(async (v) => {
    seen.push(v);
    await new Promise((r) => setTimeout(r, 1));
  });

  for (let i = 0; i < 100; i++) send(i);
  await new Promise((r) => setTimeout(r, 50));

  assert.equal(seen.at(-1), 99, 'the newest selection must always win');
  assert.ok(seen.length < 100, 'superseded selections must be dropped, not queued');
});

// ---------- playback transport ----------

test('nextPlaybackAction toggles play and pause', () => {
  assert.equal(nextPlaybackAction('stopped'), 'play');
  assert.equal(nextPlaybackAction('playing'), 'pause');
  assert.equal(nextPlaybackAction('paused'), 'resume');
});

test('nextPlaybackAction restarts after playback runs to the end', () => {
  // `finished` is the backend's "reached the end of the selection". The button must offer
  // play again, not a pause of a stream that no longer exists.
  assert.equal(nextPlaybackAction('finished'), 'play');
});

test('nextPlaybackAction falls back to play for an unknown state', () => {
  // A state the UI does not recognise must leave the transport usable rather than wedged.
  assert.equal(nextPlaybackAction(undefined), 'play');
  assert.equal(nextPlaybackAction('wat'), 'play');
});

test('playLabel offers a pause only while actually playing', () => {
  assert.match(playLabel('playing'), /Pause/);
  for (const s of ['stopped', 'paused', 'finished', undefined]) {
    assert.match(playLabel(s), /Play/, `state ${s}`);
  }
});

test('playheadVisible hides the playhead only when stopped', () => {
  assert.equal(playheadVisible('playing'), true);
  assert.equal(playheadVisible('paused'), true, 'a paused playhead still marks the position');
  assert.equal(playheadVisible('finished'), true);
  assert.equal(playheadVisible('stopped'), false);
  assert.equal(playheadVisible(undefined), false);
});

test('the playhead maps to a pixel through the same geometry as the selection', () => {
  // The playhead is drawn on the same overlay as the selection, so a divergence here would
  // show as the playhead sliding out of the highlighted region.
  const view = { start: 1000, end: 2000 };
  assert.equal(sampleToX(1500, 800, view), 400);
  assert.equal(sampleToX(view.start, 800, view), 0);
  assert.equal(sampleToX(view.end, 800, view), 800);
});

test('a play request uses the selection, or the whole file when there is none', () => {
  // The transport plays exactly what the Statistics panel describes.
  assert.deepEqual(effectiveRange({ start: 10, end: 90 }, 500), { start: 10, end: 90 });
  assert.deepEqual(effectiveRange({ start: 42, end: 42 }, 500), { start: 0, end: 500 });
});

// ---------- recording ----------

test('recLabel turns the Record button into a Stop control while recording', () => {
  assert.equal(recLabel(false), '● Record');
  assert.equal(recLabel(true), '■ Stop');
});

test('recMeta shows the elapsed take', () => {
  assert.equal(recMeta({ durationS: 3.25, overruns: 0 }), '● recording — 3.250 s');
  assert.equal(recMeta({ durationS: 65, overruns: 0 }), '● recording — 1:05.000');
});

test('recMeta flags dropped frames only when some were dropped', () => {
  // A clean take must not mention drops; a starved one must, so a glitchy recording is
  // visible rather than silent.
  assert.doesNotMatch(recMeta({ durationS: 1, overruns: 0 }), /dropped/);
  assert.equal(recMeta({ durationS: 1, overruns: 4200 }), `● recording — 1.000 s · 4${NNBSP}200 frames dropped`);
});

// ---------- level meter ----------

test('meterFraction maps silence and the floor to an empty bar', () => {
  assert.equal(meterFraction(0), 0);
  assert.equal(meterFraction(-0.5), 0, 'a negative/garbage peak is empty, not full');
  // -60 dB is the floor (10^(-60/20) = 0.001), so it sits right at the bottom.
  assert.equal(meterFraction(0.001), 0);
});

test('meterFraction fills the bar at and above full scale', () => {
  assert.equal(meterFraction(1), 1, '0 dBFS fills the meter');
  assert.equal(meterFraction(2), 1, 'a clipping peak saturates rather than overflowing');
});

test('meterFraction is a linear dB scale between the floor and full scale', () => {
  // -30 dB is exactly halfway up a -60 dB..0 dB scale.
  const half = meterFraction(10 ** (-30 / 20));
  assert.ok(Math.abs(half - 0.5) < 1e-9, `expected ~0.5, got ${half}`);
  // -6 dB (linear 0.5) sits near the top.
  const near = meterFraction(0.5);
  assert.ok(Math.abs(near - (60 - 6.0206) / 60) < 1e-3, `got ${near}`);
});

test('meterFraction honours a custom floor', () => {
  // With a -40 dB floor, -20 dB is halfway.
  assert.ok(Math.abs(meterFraction(10 ** (-20 / 20), -40) - 0.5) < 1e-9);
});

test('meterDbLabel is the inverse of meterFraction', () => {
  assert.equal(meterDbLabel(0), '-∞');
  assert.equal(meterDbLabel(1), '0.0');
  assert.equal(meterDbLabel(0.5), (METER_FLOOR_DB / 2).toFixed(1)); // -30.0
});

test('meterActive is on while recording or actually playing, off otherwise', () => {
  assert.equal(meterActive('playing', false), true);
  assert.equal(meterActive('stopped', true), true, 'recording drives the meter with no file open');
  assert.equal(meterActive('paused', false), false);
  assert.equal(meterActive('finished', false), false);
  assert.equal(meterActive('stopped', false), false);
});

test('decayMeter rises instantly and falls by at most the step', () => {
  assert.equal(decayMeter(0.2, 0.9, 0.05), 0.9, 'a louder peak snaps up immediately');
  assert.equal(decayMeter(0.9, 0.5, 0.1), 0.8, 'a quieter reading eases down by one step');
  assert.equal(decayMeter(0.3, 0.29, 0.1), 0.29, 'never falls past the target');
  assert.equal(decayMeter(0.5, 0.5, 0.1), 0.5);
});

test('isClipping trips only at or above full scale', () => {
  assert.equal(isClipping(0.999), false);
  assert.equal(isClipping(1), true);
  assert.equal(isClipping(1.4), true);
});
