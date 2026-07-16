// Unit tests for the UI's pure logic (`ui/lib.js`).
//
// Everything the waveform/Statistics port needs that is not DOM or IPC lives in `lib.js`
// precisely so it can be tested here, with no webview and no browser. `ui/app.js` is the
// thin DOM+IPC wiring on top and is exercised by the app itself.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  MAX_BINS,
  MIN_VIEW_SAMPLES,
  NNBSP,
  binsForView,
  clamp,
  createCoalescer,
  db,
  effectiveRange,
  fitView,
  fmtCount,
  fmtMeta,
  fmtTime,
  hasSelection,
  normalizeSelection,
  panBy,
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
  assert.equal(byKey['Пик (Peak)'], '-6.02'); // db(0.5)
  assert.equal(byKey['RMS'], '-12.04'); // db(0.25)
});

test('statsRows uses absolute values for min/max in dB', () => {
  // dB of a negative sample is meaningless; the magnitude is what is shown.
  const rows = statsRows({ ...DTO, min: -0.5, max: 0.25 });
  const byKey = Object.fromEntries(rows.map((r) => [r.k, r.v]));
  assert.equal(byKey['Мин. сэмпл'], '-6.02');
  assert.equal(byKey['Макс. сэмпл'], '-12.04');
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
  assert.deepEqual(hi, ['Пик (Peak)', 'RMS', 'Частота (zero-cross)']);
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
