// Live weather radar for the station website. Reuses the Graywolf maps service
// (https://maps.nw5w.com): Americana basemap + per-frame NEXRAD vector contours.
//
// The pure helpers below are dual-exported for node --test; the MapLibre glue
// runs only in the browser.

// ---- Palette ---------------------------------------------------------------
// The band values, the ptype codes and every hex below are copied verbatim from
// graywolf's `web/src/lib/map/sources/radar-source.js`, which is where the
// contour generator's own palette lives. Both ends have to agree about what a
// color means, so a change belongs there first and is mirrored here.

// Liquid precipitation, keyed by each band's dBZ lower bound. This is the
// classic NWS green/yellow/red progression, not RainViewer's "Universal Blue":
// Universal Blue spends its six weakest bands on cyan and blue, which is the
// exact 10-30 dBZ window winter precipitation lives in, and snow is blue on
// every radar product a reader recognizes. Blue cannot mean both, so liquid
// gave it up (see PTYPE_COLORS).
const DBZ_BANDS = [5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75];
const DBZ_COLORS = {
  5: '#cdfccd', 10: '#9bf79b', 15: '#5fe85f', 20: '#22cc22', 25: '#11a011',
  30: '#0a7a0a', 35: '#ffee00', 40: '#ffaa00', 45: '#ff4400', 50: '#e60000',
  55: '#c10000', 60: '#9a0000', 65: '#f800fd', 70: '#9854c6', 75: '#ffffff',
};

// Surface precipitation type, carried on every contour's `ptype` attribute.
// Cross-repo contract with the generator (`radar-contour/src/ptype.rs`): never
// renumber, the codes are baked into every tile already in the archive. 0 means
// the generator had no type information and renders on the liquid ramp.
const PTYPE_NONE = 0;
const PTYPE_RAIN = 1;
const PTYPE_SNOW = 2;
const PTYPE_SLEET = 3;
const PTYPE_FREEZING_RAIN = 4;
const PTYPE_MIXED = 5;

// Frozen and mixed precipitation. Hues follow NWS hazard convention -- snow
// blue, sleet pink, freezing rain violet, mixed neutral.
//
// The ramps stop at 40 dBZ on purpose: frozen precipitation is a weak scatterer,
// and a `step` with no stop above 40 holds the darkest color for anything
// stronger instead of running off the end of the ramp.
//
// Each type also owns a lightness range, not just a hue. Red-green dichromats
// keep lightness and lose hue, and two pairs collapse under that loss: the two
// blues (snow, freezing rain) and the two near-neutrals (sleet, mixed). Each
// pair is split in lightness -- snow light / freezing rain dark, sleet light /
// mixed dark -- so the consequence to know about is that lightness now encodes
// type as well as intensity. Intensity still reads monotonically *within* a
// type, which is the comparison a reader actually makes. `weather-radar.test.js`
// pins this with simulated dichromacy so it cannot be undone by eye.
const PTYPE_BANDS = [5, 10, 15, 20, 25, 30, 35, 40];
const PTYPE_COLORS = {
  [PTYPE_SNOW]: {
    5: '#e4f0fe', 10: '#cde3fe', 15: '#b1d5fd', 20: '#97c6fc',
    25: '#7ab6fc', 30: '#5ea7fb', 35: '#3f96fa', 40: '#2085f9',
  },
  [PTYPE_SLEET]: {
    5: '#fedcec', 10: '#fec9e1', 15: '#fdb4d6', 20: '#fc9dc9',
    25: '#fc84bc', 30: '#fb6bae', 35: '#fa4d9e', 40: '#f9117d',
  },
  [PTYPE_FREEZING_RAIN]: {
    5: '#b179fc', 10: '#a767fb', 15: '#9c54fb', 20: '#9040fa',
    25: '#8226f9', 30: '#6e06f5', 35: '#6006d7', 40: '#5305ba',
  },
  [PTYPE_MIXED]: {
    5: '#988881', 10: '#8d7c74', 15: '#81716a', 20: '#74665f',
    25: '#675a54', 30: '#5c504b', 35: '#4f4540', 40: '#433a36',
  },
};

const PTYPE_LABELS = {
  [PTYPE_NONE]: 'Precipitation',
  [PTYPE_RAIN]: 'Rain',
  [PTYPE_SNOW]: 'Snow',
  [PTYPE_SLEET]: 'Sleet',
  [PTYPE_FREEZING_RAIN]: 'Freezing rain',
  [PTYPE_MIXED]: 'Mixed',
};

// MapLibre `step` expression over a polygon's `dbz`. A polygon in the lowest
// band falls below the first stop and takes the base output, so stops begin at
// the second band.
function buildStep(bands, colors) {
  const expr = ['step', ['get', 'dbz'], colors[bands[0]]];
  for (let i = 1; i < bands.length; i++) expr.push(bands[i], colors[bands[i]]);
  return expr;
}

// The liquid intensity ramp. Also what `ptype` 0 and 1 resolve to.
function buildDbzFillColor() {
  return buildStep(DBZ_BANDS, DBZ_COLORS);
}

// The intensity ramp for one frozen/mixed type; liquid for anything else.
function buildPtypeFillColor(ptype) {
  const colors = PTYPE_COLORS[ptype];
  return colors ? buildStep(PTYPE_BANDS, colors) : buildDbzFillColor();
}

// Switch palette on precipitation type, then ramp on intensity within it. Rain
// and "no type information" share the default branch, so a tile generated
// before `ptype` existed renders exactly as it always did.
//
// `['get', 'ptype']` must stay unwrapped: a `to-number`/`number` cast throws on
// a missing attribute, and MapLibre would paint every pre-`ptype` tile black.
function buildRadarFillColor() {
  return [
    'match',
    ['get', 'ptype'],
    PTYPE_SNOW, buildPtypeFillColor(PTYPE_SNOW),
    PTYPE_SLEET, buildPtypeFillColor(PTYPE_SLEET),
    PTYPE_FREEZING_RAIN, buildPtypeFillColor(PTYPE_FREEZING_RAIN),
    PTYPE_MIXED, buildPtypeFillColor(PTYPE_MIXED),
    buildDbzFillColor(),
  ];
}

// Legend model, light -> heavy, derived from the palette so the key cannot
// drift from what the map paints. Color encodes two dimensions now (type x
// intensity) instead of one, so the key is load-bearing rather than polish.
// The liquid row is labelled "Rain" because that is what a reader calls it;
// untyped echo renders on the same ramp.
function buildLegendRows() {
  const row = (ptype, bands, colors) => ({
    ptype,
    label: PTYPE_LABELS[ptype],
    swatches: bands.map((dbz) => ({ dbz, color: colors[dbz] })),
  });
  return [
    row(PTYPE_RAIN, DBZ_BANDS, DBZ_COLORS),
    row(PTYPE_SNOW, PTYPE_BANDS, PTYPE_COLORS[PTYPE_SNOW]),
    row(PTYPE_SLEET, PTYPE_BANDS, PTYPE_COLORS[PTYPE_SLEET]),
    row(PTYPE_FREEZING_RAIN, PTYPE_BANDS, PTYPE_COLORS[PTYPE_FREEZING_RAIN]),
    row(PTYPE_MIXED, PTYPE_BANDS, PTYPE_COLORS[PTYPE_MIXED]),
  ];
}

// Zoom so a `widthPx`-wide map spans `miles` at latitude `latDeg`.
// Web-Mercator: meters/px = 156543.03 * cos(lat) / 2^z.
function zoomForWidthMiles(miles, latDeg, widthPx) {
  const meters = miles * 1609.344;
  const lat = (latDeg * Math.PI) / 180;
  const z = Math.log2((156543.03 * Math.cos(lat) * widthPx) / meters);
  return Math.max(3, Math.min(z, 11));
}

// Radar manifest -> oldest-first [{ts, iso}]. Manifest `frames` is newest-first.
function parseManifestFrames(json) {
  if (!json || typeof json !== 'object') return [];
  if (json.schema_version !== 1 || !Array.isArray(json.frames)) return [];
  const frames = [];
  for (const f of json.frames) {
    if (!f || typeof f.ts !== 'number' || typeof f.iso !== 'string') continue;
    frames.push({ ts: f.ts, iso: f.iso });
  }
  frames.reverse();
  return frames;
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = {
    DBZ_BANDS, DBZ_COLORS,
    PTYPE_NONE, PTYPE_RAIN, PTYPE_SNOW, PTYPE_SLEET, PTYPE_FREEZING_RAIN, PTYPE_MIXED,
    PTYPE_BANDS, PTYPE_COLORS, PTYPE_LABELS,
    buildDbzFillColor, buildPtypeFillColor, buildRadarFillColor, buildLegendRows,
    zoomForWidthMiles, parseManifestFrames,
  };
}

// ---- Browser glue (skipped under node --test) -----------------------------
if (typeof window !== 'undefined' && typeof document !== 'undefined') {
  const TILE_HOST = 'maps.nw5w.com';
  const STYLE_URL = `https://${TILE_HOST}/style/americana-roboto/style.json`;
  const MANIFEST_URL = `https://${TILE_HOST}/radar/manifest.json`;
  const FRAME_MS = 500;        // per-frame dwell
  const HOLD_MS = 1500;        // longer hold on the newest frame
  const POLL_MS = 60000;       // manifest refresh
  const DESKTOP_MAX_FRAMES = 36;
  const MOBILE_MAX_FRAMES = 12;

  // Mount the palette key under the radar controls. Built from `buildLegendRows`
  // rather than from markup so the key and the fill expression cannot disagree;
  // it lives here instead of in the template because `weather-station.html` is
  // byte-pinned against the Go recording (`crates/mw-web/tests/byte_diff.rs`).
  function mountLegend(section, open) {
    if (!section || document.getElementById('radar-legend')) return;
    const details = document.createElement('details');
    details.id = 'radar-legend';
    details.className = 'radar-legend';
    details.open = open;
    const summary = document.createElement('summary');
    summary.textContent = 'Legend';
    details.appendChild(summary);

    // Labels and ramps are laid out by one grid spanning every row, so the five
    // scales and the axis line up on a column the widest label defines.
    const grid = document.createElement('div');
    grid.className = 'radar-legend-grid';

    for (const row of buildLegendRows()) {
      const line = document.createElement('div');
      line.className = 'radar-legend-row';
      const label = document.createElement('span');
      label.className = 'radar-legend-label';
      label.textContent = row.label;
      const ramp = document.createElement('span');
      ramp.className = 'radar-legend-ramp';
      // Every ramp is a `step`, which holds its last color for anything above
      // its last stop, so each scale's top swatch means "that dBZ and above".
      const top = row.swatches[row.swatches.length - 1].dbz;
      ramp.setAttribute('role', 'img');
      ramp.setAttribute('aria-label',
        `${row.label}: light to heavy, ${row.swatches[0].dbz} to ${top} dBZ and above`);
      for (const s of row.swatches) {
        const swatch = document.createElement('i');
        swatch.style.backgroundColor = s.color;
        swatch.title = `${s.dbz} dBZ`;
        ramp.appendChild(swatch);
      }
      line.append(label, ramp);
      grid.appendChild(line);
    }

    const axis = document.createElement('div');
    axis.className = 'radar-legend-axis';
    const light = document.createElement('span');
    light.textContent = 'Light';
    const heavy = document.createElement('span');
    heavy.textContent = 'Heavy';
    axis.append(light, heavy);
    grid.appendChild(axis);
    details.appendChild(grid);
    section.appendChild(details);
  }

  function initRadar() {
    const cfgEl = document.getElementById('radar-config');
    const mapEl = document.getElementById('radar-map');
    if (!cfgEl || !mapEl || !window.maplibregl) return;
    const cfg = JSON.parse(cfgEl.textContent);
    const isTouch = window.matchMedia('(pointer: coarse)').matches;
    const maxFrames = isTouch ? MOBILE_MAX_FRAMES : DESKTOP_MAX_FRAMES;
    // Open on desktop, collapsed on touch where it would crowd the map.
    mountLegend(document.getElementById('radar-section'), !isTouch);

    const tokenParam = (url) => {
      const sep = url.includes('?') ? '&' : '?';
      return `${url}${sep}t=${encodeURIComponent(cfg.token)}`;
    };

    const map = new maplibregl.Map({
      container: 'radar-map',
      style: STYLE_URL,
      center: [cfg.lon, cfg.lat],
      zoom: zoomForWidthMiles(cfg.widthMiles || 200, cfg.lat, mapEl.clientWidth || 800),
      attributionControl: { compact: true },
      cooperativeGestures: isTouch,
      transformRequest: (url) => {
        if (url.startsWith(`https://${TILE_HOST}/`) && !url.startsWith(`https://${TILE_HOST}/style/`)) {
          return { url: tokenParam(url) };
        }
        return { url };
      },
    });
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');
    map.addControl(new maplibregl.ScaleControl({ unit: 'imperial' }), 'bottom-left');

    // Keep ~200mi width across resizes/orientation.
    let resizeTimer = null;
    window.addEventListener('resize', () => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        map.setZoom(zoomForWidthMiles(cfg.widthMiles || 200, cfg.lat, mapEl.clientWidth || 800));
      }, 200);
    });

    map.on('load', () => {
      mapEl.classList.remove('is-loading');
      new maplibregl.Marker().setLngLat([cfg.lon, cfg.lat]).addTo(map);
      startRadar(map, tokenParam, maxFrames, !isTouch);
    });
    // Single-tile fetch errors are benign; only log.
    map.on('error', (e) => console.warn('[radar]', e && e.error ? e.error.message : e));
  }

  function startRadar(map, tokenParam, maxFrames, autoplay) {
    const sourceId = (ts) => `radar-${ts}`;
    const layerId = (ts) => `radar-fill-${ts}`;
    const mounted = new Set();
    let frames = [];        // oldest-first [{ts, iso}]
    let index = 0;
    let curTs = null;
    let playing = false;
    let timer = null;
    const fillColor = buildRadarFillColor();

    const firstSymbolId = () => (map.getStyle().layers.find((l) => l.type === 'symbol') || {}).id;

    function ensureFrame(ts, visible) {
      if (!map.getSource(sourceId(ts))) {
        map.addSource(sourceId(ts), {
          type: 'vector', minzoom: 3, maxzoom: 8,
          tiles: [tokenParam(`https://maps.nw5w.com/radar/${ts}/{z}/{x}/{y}.pbf`)],
          attribution: 'NEXRAD via NWS / Iowa State Mesonet',
        });
      }
      if (!map.getLayer(layerId(ts))) {
        map.addLayer({
          id: layerId(ts), type: 'fill', source: sourceId(ts), 'source-layer': 'radar',
          paint: { 'fill-color': fillColor, 'fill-antialias': false, 'fill-opacity': visible ? 0.75 : 0 },
        }, firstSymbolId());
      }
      mounted.add(ts);
    }

    function removeFrame(ts) {
      if (map.getLayer(layerId(ts))) map.removeLayer(layerId(ts));
      if (map.getSource(sourceId(ts))) map.removeSource(sourceId(ts));
      mounted.delete(ts);
    }

    function showFrame(ts) {
      if (ts === curTs) return;
      const prev = curTs;
      curTs = ts;
      ensureFrame(ts, true);
      if (prev != null && map.getLayer(layerId(prev))) map.setPaintProperty(layerId(prev), 'fill-opacity', 0);
      if (map.getLayer(layerId(ts))) map.setPaintProperty(layerId(ts), 'fill-opacity', 0.75);
      updateUI();
    }

    function scheduleNext() {
      clearTimeout(timer);
      if (!playing || frames.length <= 1) return;
      const atNewest = index >= frames.length - 1;
      timer = setTimeout(() => {
        index = (index + 1) % frames.length;
        showFrame(frames[index].ts);
        scheduleNext();
      }, atNewest ? HOLD_MS : FRAME_MS);
    }

    function play() { if (frames.length > 1) { playing = true; scheduleNext(); updateUI(); } }
    function pause() { playing = false; clearTimeout(timer); updateUI(); }
    function reset() { pause(); index = frames.length - 1; if (frames[index]) showFrame(frames[index].ts); }
    function seek(i) { pause(); index = Math.max(0, Math.min(i, frames.length - 1)); if (frames[index]) showFrame(frames[index].ts); }

    async function poll() {
      try {
        const res = await fetch(tokenParam(MANIFEST_URL), { cache: 'no-store' });
        if (!res.ok) return;
        let next = parseManifestFrames(await res.json());
        if (next.length > maxFrames) next = next.slice(next.length - maxFrames);
        if (!next.length) return;
        const keep = new Set(next.map((f) => f.ts));
        for (const ts of [...mounted]) if (!keep.has(ts) && ts !== curTs) removeFrame(ts);
        next.forEach((f) => ensureFrame(f.ts, false));
        frames = next;
        if (curTs == null || !keep.has(curTs)) { index = frames.length - 1; showFrame(frames[index].ts); }
        else { index = frames.findIndex((f) => f.ts === curTs); }
        updateUI();
      } catch (e) { console.warn('[radar] manifest poll failed', e); }
    }

    // ---- controls ----
    const btnPlay = document.getElementById('radar-play');
    const btnReset = document.getElementById('radar-reset');
    const slider = document.getElementById('radar-scrubber');
    const tsLabel = document.getElementById('radar-timestamp');
    if (btnPlay) btnPlay.addEventListener('click', () => (playing ? pause() : play()));
    if (btnReset) btnReset.addEventListener('click', reset);
    if (slider) slider.addEventListener('input', (e) => seek(Number(e.target.value)));

    function updateUI() {
      if (btnPlay) {
        btnPlay.textContent = playing ? 'Pause' : 'Play';
        btnPlay.setAttribute('aria-pressed', String(playing));
      }
      if (slider) { slider.max = String(Math.max(0, frames.length - 1)); slider.value = String(index); }
      if (tsLabel && frames[index]) {
        tsLabel.textContent = new Date(frames[index].ts * 1000)
          .toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
      }
    }

    poll().then(() => { if (autoplay) play(); });
    setInterval(poll, POLL_MS);
  }

  // Lazy init: build the map only when the section scrolls into view.
  document.addEventListener('DOMContentLoaded', () => {
    const section = document.getElementById('radar-section');
    if (!section) return;
    if (!('IntersectionObserver' in window)) { initRadar(); return; }
    const io = new IntersectionObserver((entries, obs) => {
      if (entries.some((e) => e.isIntersecting)) { obs.disconnect(); initRadar(); }
    }, { rootMargin: '200px' });
    io.observe(section);
  });
}
