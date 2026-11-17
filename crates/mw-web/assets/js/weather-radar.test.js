const { test } = require('node:test');
const assert = require('node:assert');
const {
  DBZ_BANDS,
  DBZ_COLORS,
  PTYPE_NONE,
  PTYPE_RAIN,
  PTYPE_SNOW,
  PTYPE_SLEET,
  PTYPE_FREEZING_RAIN,
  PTYPE_MIXED,
  PTYPE_BANDS,
  PTYPE_COLORS,
  PTYPE_LABELS,
  buildDbzFillColor,
  buildPtypeFillColor,
  buildRadarFillColor,
  buildLegendRows,
  zoomForWidthMiles,
  parseManifestFrames,
} = require('./weather-radar.js');

test('zoomForWidthMiles ~200mi at 40N lands around z7-8', () => {
  const z = zoomForWidthMiles(200, 40.5, 800);
  assert.ok(z > 6.5 && z < 8.5, `z=${z}`);
});

test('zoomForWidthMiles clamps to [3,11]', () => {
  assert.strictEqual(zoomForWidthMiles(20000, 0, 800), 3);
  assert.strictEqual(zoomForWidthMiles(0.001, 0, 800), 11);
});

test('buildDbzFillColor is a step expression keyed on dbz', () => {
  const e = buildDbzFillColor();
  assert.strictEqual(e[0], 'step');
  assert.deepStrictEqual(e[1], ['get', 'dbz']);
  assert.strictEqual(e[2], DBZ_COLORS[DBZ_BANDS[0]]);   // base = lowest band color
  assert.strictEqual(e[e.length - 2], 75);              // last stop input
  assert.strictEqual(e[e.length - 1], DBZ_COLORS[75]);  // last stop color
});

test('the dbz step has one stop per band, strictly ascending', () => {
  const e = buildDbzFillColor();
  const stops = [];
  for (let i = 3; i < e.length; i += 2) {
    assert.strictEqual(e[i + 1], DBZ_COLORS[e[i]], `stop ${e[i]} maps to its palette color`);
    stops.push(e[i]);
  }
  assert.deepStrictEqual(stops, DBZ_BANDS.slice(1));
});

test('parseManifestFrames reverses newest-first to oldest-first', () => {
  const frames = parseManifestFrames({
    schema_version: 1,
    frames: [{ ts: 200, iso: 'b' }, { ts: 100, iso: 'a' }],
  });
  assert.deepStrictEqual(frames.map(f => f.ts), [100, 200]);
});

test('parseManifestFrames returns [] for a bad manifest', () => {
  assert.deepStrictEqual(parseManifestFrames({ schema_version: 2 }), []);
  assert.deepStrictEqual(parseManifestFrames(null), []);
});

// ---- surface precipitation type palette ------------------------------------
// The palette is graywolf's (`web/src/lib/map/sources/radar-source.js`); these
// are its contracts, carried over so this copy cannot drift out from under the
// generator or lose its accessibility properties to an eyeballed edit.

test('ptype codes match the generator contract and are never renumbered', () => {
  // Mirrors graywolf radar-contour/src/ptype.rs. Changing these silently
  // remaps colors on every tile already in the archive.
  assert.strictEqual(PTYPE_NONE, 0);
  assert.strictEqual(PTYPE_RAIN, 1);
  assert.strictEqual(PTYPE_SNOW, 2);
  assert.strictEqual(PTYPE_SLEET, 3);
  assert.strictEqual(PTYPE_FREEZING_RAIN, 4);
  assert.strictEqual(PTYPE_MIXED, 5);
});

const WINTER_TYPES = [PTYPE_SNOW, PTYPE_SLEET, PTYPE_FREEZING_RAIN, PTYPE_MIXED];
const HUED_TYPES = [PTYPE_SNOW, PTYPE_SLEET, PTYPE_FREEZING_RAIN];

test('every band of every ramp has a 6-digit hex color', () => {
  for (const dbz of DBZ_BANDS) {
    assert.match(DBZ_COLORS[dbz], /^#[0-9a-f]{6}$/, `liquid band ${dbz}`);
  }
  for (const ptype of WINTER_TYPES) {
    for (const dbz of PTYPE_BANDS) {
      assert.match(PTYPE_COLORS[ptype][dbz], /^#[0-9a-f]{6}$/, `ptype ${ptype} band ${dbz}`);
    }
  }
});

test('every type has a label for the legend', () => {
  for (const ptype of [PTYPE_NONE, PTYPE_RAIN, ...WINTER_TYPES]) {
    assert.strictEqual(typeof PTYPE_LABELS[ptype], 'string');
    assert.ok(PTYPE_LABELS[ptype].length > 0);
  }
});

// Small color helpers, so the palette contract is checked numerically rather
// than by eye. Luminance must linearize first: a weighted sum of gamma-encoded
// channels is not luminance and understates differences in the darks.
const rgbOf = (hex) =>
  [0, 1, 2].map((i) => parseInt(hex.slice(1 + i * 2, 3 + i * 2), 16) / 255);
const toLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const linOf = (hex) => rgbOf(hex).map(toLinear);
const lumOf = (hex) => {
  const [r, g, b] = linOf(hex);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const chromaOf = (hex) => {
  const c = rgbOf(hex);
  return Math.max(...c) - Math.min(...c);
};
const hueOf = (hex) => {
  const [r, g, b] = rgbOf(hex);
  const max = Math.max(r, g, b);
  const d = max - Math.min(r, g, b);
  if (d === 0) return 0;
  let h;
  if (max === r) h = ((g - b) / d) % 6;
  else if (max === g) h = (b - r) / d + 2;
  else h = (r - g) / d + 4;
  return (h * 60 + 360) % 360;
};

test('snow keeps the blue family and liquid gives it up', () => {
  // The whole point of the repaint: at the weak reflectivities where winter
  // precip lives, snow must be bluer than rain or the two are one color.
  const blueness = (hex) =>
    parseInt(hex.slice(5, 7), 16) - parseInt(hex.slice(1, 3), 16);
  for (const dbz of [10, 15, 20, 25, 30]) {
    assert.ok(blueness(PTYPE_COLORS[PTYPE_SNOW][dbz]) > 40,
      `snow at ${dbz} dBZ should read blue, got ${PTYPE_COLORS[PTYPE_SNOW][dbz]}`);
    assert.ok(blueness(DBZ_COLORS[dbz]) < 10,
      `liquid at ${dbz} dBZ must not be blue, got ${DBZ_COLORS[dbz]}`);
  }
  for (const dbz of [5, 10, 15, 20, 25, 30]) {
    const h = hueOf(DBZ_COLORS[dbz]);
    assert.ok(h > 70 && h < 170, `liquid at ${dbz} dBZ should be green, got ${DBZ_COLORS[dbz]}`);
  }
});

test('the liquid ramp has no duplicate colors and tops out near-white', () => {
  // Two bands shared #ffffff before this palette pass (65 and 70 dBZ), and 75
  // jumped back to #00ff00 -- colliding with the weak-echo greens 70 dB below.
  assert.strictEqual(new Set(Object.values(DBZ_COLORS)).size, DBZ_BANDS.length,
    'duplicate color(s) in the liquid ramp');
  const top = DBZ_COLORS[DBZ_BANDS[DBZ_BANDS.length - 1]];
  assert.ok(chromaOf(top) < 0.1, `top band ${top} should be near-neutral`);
  assert.ok(lumOf(top) > 0.9, `top band ${top} should be the brightest`);
});

test('every winter ramp darkens monotonically with intensity', () => {
  // Intensity has to read within a type, not only between types.
  for (const ptype of WINTER_TYPES) {
    let prev = Infinity;
    for (const dbz of PTYPE_BANDS) {
      const l = lumOf(PTYPE_COLORS[ptype][dbz]);
      assert.ok(l < prev, `ptype ${ptype} must darken at ${dbz} dBZ (${l} vs ${prev})`);
      prev = l;
    }
  }
});

test('mixed is the only desaturated ramp, so it never reads as a hue', () => {
  for (const dbz of PTYPE_BANDS) {
    assert.ok(chromaOf(PTYPE_COLORS[PTYPE_MIXED][dbz]) < 0.12,
      `mixed at ${dbz} dBZ must stay neutral`);
  }
  // Only over the bands that publish: below the generator's 20 dBZ floor the
  // hued ramps approach white, where chroma necessarily falls away.
  for (const dbz of PTYPE_BANDS.filter((d) => d >= 20)) {
    for (const ptype of HUED_TYPES) {
      assert.ok(chromaOf(PTYPE_COLORS[ptype][dbz]) > 0.15,
        `ptype ${ptype} at ${dbz} dBZ needs enough chroma to read as a hue`);
    }
  }
});

// ---- color-vision math -----------------------------------------------------
// The palette's correctness claim is "a reader can tell these apart", readers
// with red-green color blindness included. That is not eyeballable, so it is
// computed: sRGB -> linear -> LMS, collapse the lost channel (Vienot et al.
// 1999), back to sRGB, then compare in CIE L*a*b* with CIE76. deltaE under ~5
// is "the same color" to a human.
const matMul = (m, v) => m.map((row) => row[0] * v[0] + row[1] * v[1] + row[2] * v[2]);
const RGB_TO_LMS = [
  [0.31399022, 0.63951294, 0.04649755],
  [0.15537241, 0.75789446, 0.08670142],
  [0.01775239, 0.10944209, 0.87256922],
];
const LMS_TO_RGB = [
  [5.47221206, -4.6419601, 0.16963708],
  [-1.1252419, 2.29317094, -0.1678952],
  [0.02980165, -0.19318073, 1.16364789],
];
// Deuteranopia drops the M cone; protanopia drops L. Each is reconstructed as a
// linear combination of the two the viewer retains.
const DEUTERANOPIA = [[1, 0, 0], [0.9513092, 0, 0.04866992], [0, 0, 1]];
const PROTANOPIA = [[0, 1.05118294, -0.05116099], [0, 1, 0], [0, 0, 1]];
// Returns LINEAR rgb, clamped to the displayable gamut, because `labOf` takes
// linear input -- handing it gamma-encoded values silently compresses every
// deltaE and the whole check goes soft.
const simulate = (hex, collapse) => {
  const lms = matMul(RGB_TO_LMS, linOf(hex));
  return matMul(LMS_TO_RGB, matMul(collapse, lms)).map((c) => Math.max(0, Math.min(1, c)));
};
const labOf = (lin) => {
  const [r, g, b] = lin;
  const xyz = [
    (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047,
    0.2126 * r + 0.7152 * g + 0.0722 * b,
    (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883,
  ];
  const f = (t) => (t > 0.008856 ? Math.cbrt(t) : 7.787 * t + 16 / 116);
  const [fx, fy, fz] = xyz.map(f);
  return [116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz)];
};
const deltaE = (linA, linB) => {
  const [a, b] = [labOf(linA), labOf(linB)];
  return Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);
};
const VISION = [
  ['normal vision', (hex) => linOf(hex)],
  ['deuteranopia', (hex) => simulate(hex, DEUTERANOPIA)],
  ['protanopia', (hex) => simulate(hex, PROTANOPIA)],
];

test('no two precipitation types look alike, including to dichromats', () => {
  // Hue separation alone is structurally blind to the failure it was meant to
  // prevent: hue is precisely the channel these readers lose. Snow vs freezing
  // rain is also the pair that matters most -- one is a benign inconvenience,
  // the other brings down power lines.
  //
  // Restricted to the bands that publish (the generator runs a 20 dBZ floor);
  // below it the ramps are allowed to converge toward white.
  const published = PTYPE_BANDS.filter((d) => d >= 20);
  for (const [vision, project] of VISION) {
    for (const dbz of published) {
      for (let i = 0; i < WINTER_TYPES.length; i++) {
        for (let j = i + 1; j < WINTER_TYPES.length; j++) {
          const a = PTYPE_COLORS[WINTER_TYPES[i]][dbz];
          const b = PTYPE_COLORS[WINTER_TYPES[j]][dbz];
          const d = deltaE(project(a), project(b));
          assert.ok(d > 15,
            `ptype ${WINTER_TYPES[i]} vs ${WINTER_TYPES[j]} at ${dbz} dBZ under ${vision}: `
            + `deltaE ${d.toFixed(1)} (${a} vs ${b}) -- too close to tell apart`);
        }
      }
    }
  }
});

// Winter vs liquid, which the sweep above does not reach: it compares winter
// types to each other at equal dBZ, and a reader looking at one frame sees
// whatever intensities the weather happens to be producing, including snow in
// one state and a convective core in another.
//
// Three of the 240 published combinations collapse, all of them against the
// 65 dBZ liquid band -- the conventional magenta at the extreme end of the NWS
// ramp, which under simulated red-green dichromacy lands on the same
// desaturated blue as mid-intensity snow. Declared here rather than repainted,
// because these hexes are graywolf's and the two repositories have to agree
// about what a color means; the fix belongs upstream in `radar-source.js`.
//
// Checked in BOTH directions, the way `ci/conformance`'s excuse list is: a pair
// below the bar that is not declared fails, and a declared pair that has risen
// above it fails too, so the list cannot outlive the collision it describes.
const CROSS_PHASE_COLLISIONS = [
  ['deuteranopia', PTYPE_SNOW, 25, 65],
  ['deuteranopia', PTYPE_SNOW, 30, 65],
  ['protanopia', PTYPE_SNOW, 40, 65],
];

test('winter types stay distinct from liquid, except where it is declared', () => {
  const published = (bands) => bands.filter((d) => d >= 20);
  const declared = new Set(CROSS_PHASE_COLLISIONS.map((c) => c.join('|')));
  const seen = new Set();
  for (const [vision, project] of VISION) {
    for (const ptype of WINTER_TYPES) {
      for (const wdbz of published(PTYPE_BANDS)) {
        for (const ldbz of published(DBZ_BANDS)) {
          const a = PTYPE_COLORS[ptype][wdbz];
          const b = DBZ_COLORS[ldbz];
          const d = deltaE(project(a), project(b));
          const key = [vision, ptype, wdbz, ldbz].join('|');
          if (d > 15) continue;
          seen.add(key);
          assert.ok(declared.has(key),
            `ptype ${ptype} at ${wdbz} dBZ vs liquid at ${ldbz} dBZ under ${vision}: `
            + `deltaE ${d.toFixed(1)} (${a} vs ${b}) -- a new collision, not one of the `
            + 'three CROSS_PHASE_COLLISIONS declares');
        }
      }
    }
  }
  for (const key of declared) {
    assert.ok(seen.has(key),
      `CROSS_PHASE_COLLISIONS declares ${key}, which now separates cleanly -- `
      + 'drop the declaration so the list keeps describing the palette');
  }
});

test('the two pairs that collapse under dichromacy are split in lightness', () => {
  // The structural reason the test above passes: snow/freezing rain are both
  // blue and sleet/mixed both go near-neutral, so within each pair lightness is
  // the only channel left. Asserted directly so a future edit that re-equalizes
  // those lightnesses fails here with a reason, not opaquely in the deltaE sweep.
  for (const dbz of PTYPE_BANDS) {
    const gap = (a, b) => lumOf(PTYPE_COLORS[a][dbz]) - lumOf(PTYPE_COLORS[b][dbz]);
    assert.ok(gap(PTYPE_SNOW, PTYPE_FREEZING_RAIN) > 0.1,
      `snow must stay clearly lighter than freezing rain at ${dbz} dBZ`);
    assert.ok(gap(PTYPE_SLEET, PTYPE_MIXED) > 0.1,
      `sleet must stay clearly lighter than mixed at ${dbz} dBZ`);
  }
});

test('buildPtypeFillColor ramps on dbz within a type and holds above 40', () => {
  const e = buildPtypeFillColor(PTYPE_SNOW);
  assert.strictEqual(e[0], 'step');
  assert.deepStrictEqual(e[1], ['get', 'dbz']);
  assert.strictEqual(e[2], PTYPE_COLORS[PTYPE_SNOW][PTYPE_BANDS[0]]);
  const stops = [];
  for (let i = 3; i < e.length; i += 2) stops.push(e[i]);
  // No stop above the last winter band, so stronger echo keeps the darkest color.
  assert.deepStrictEqual(stops, PTYPE_BANDS.slice(1));
  assert.strictEqual(e[e.length - 1], PTYPE_COLORS[PTYPE_SNOW][40]);
});

test('buildPtypeFillColor falls back to the liquid ramp for an unknown type', () => {
  assert.deepStrictEqual(buildPtypeFillColor(PTYPE_RAIN), buildDbzFillColor());
  assert.deepStrictEqual(buildPtypeFillColor(PTYPE_NONE), buildDbzFillColor());
  assert.deepStrictEqual(buildPtypeFillColor(99), buildDbzFillColor());
});

test('buildRadarFillColor matches on ptype with liquid as the default branch', () => {
  const e = buildRadarFillColor();
  assert.strictEqual(e[0], 'match');
  // Must stay an unwrapped `get`: a to-number/number cast throws on a missing
  // attribute and MapLibre paints every pre-ptype tile black.
  assert.deepStrictEqual(e[1], ['get', 'ptype']);
  // Trailing element is the fallback, which must be the liquid ramp so ptype 0
  // (unknown) and 1 (rain) both render as plain precipitation.
  assert.deepStrictEqual(e[e.length - 1], buildDbzFillColor());
  for (const ptype of WINTER_TYPES) {
    const at = e.indexOf(ptype, 2);
    assert.ok(at > 0, `ptype ${ptype} needs a match branch`);
    assert.deepStrictEqual(e[at + 1], buildPtypeFillColor(ptype));
  }
  // Rain is NOT its own branch -- it shares the default, so an old tile with no
  // ptype attribute at all cannot fall through to nothing.
  assert.strictEqual(e.indexOf(PTYPE_RAIN, 2), -1);
});

// ---- legend ----------------------------------------------------------------

test('buildLegendRows covers every drawn palette, light to heavy', () => {
  const rows = buildLegendRows();
  assert.deepStrictEqual(rows.map((r) => r.ptype),
    [PTYPE_RAIN, PTYPE_SNOW, PTYPE_SLEET, PTYPE_FREEZING_RAIN, PTYPE_MIXED]);
  for (const row of rows) {
    assert.strictEqual(row.label, PTYPE_LABELS[row.ptype]);
    const bands = row.ptype === PTYPE_RAIN ? DBZ_BANDS : PTYPE_BANDS;
    assert.deepStrictEqual(row.swatches.map((s) => s.dbz), bands);
  }
});

test('every legend swatch is the color the fill expression paints', () => {
  // The legend is only worth having if it cannot drift from the map, so each
  // swatch is checked against the step expression for its own type.
  for (const row of buildLegendRows()) {
    const expr = row.ptype === PTYPE_RAIN
      ? buildDbzFillColor()
      : buildPtypeFillColor(row.ptype);
    for (const { dbz, color } of row.swatches) {
      // expr = ['step', input, base, stop1, c1, ...]; the base is the lowest band.
      const at = expr.indexOf(dbz, 3);
      const painted = at > 0 && at % 2 === 1 ? expr[at + 1] : expr[2];
      assert.strictEqual(color, painted, `ptype ${row.ptype} at ${dbz} dBZ`);
    }
  }
});
