import { describe, expect, it } from 'vitest';
import {
  common,
  destinations,
  nav,
  needsAttention,
  spins,
  stationKinds,
  receivedFromPeer,
  stations,
  storage,
  system,
  variantFor
} from '../../src/lib/copy';
import { shapeFor, knownKinds } from '../../src/lib/live/stationform';
import { allPages, inventory } from '../../src/lib/pages';

/**
 * §6.7's "vocabulary mapping", tested as the two properties that actually matter: the
 * **absence** of internal terms, and the presence of a hint on every field.
 *
 * `cargo xtask gate-ui-copy` is the real enforcement -- it sees every template too -- and
 * this is the half that runs in a second on a developer's machine, which is where a
 * mistake is cheapest to find.
 */

/** §0.1's list, as the test's own copy, so a change to the gate shows up here too. */
const BANNED = [
  'component',
  'capability',
  'trigger',
  'sink',
  'envelope',
  'spool',
  'provision',
  'supervisor',
  'engine',
  'device',
  'controller',
  'storage backend'
];

function offenders(text: string): string[] {
  const lowered = text.toLowerCase();
  return BANNED.filter((term) => new RegExp(`\\b${term}s?\\b`).test(lowered));
}

describe('the user-facing vocabulary', () => {
  it('names no internal term in the navigation', () => {
    for (const item of nav) {
      expect(offenders(`${item.label} ${item.summary}`)).toEqual([]);
    }
  });

  it('names no internal term in the shared labels', () => {
    // `common` is where a word slips in: it is read by every page and reviewed as part of
    // none of them. "Follow my device" lived here, and `device` is banned precisely
    // because the current UI alternates it with "station" inside one screen.
    for (const [name, value] of Object.entries(common)) {
      if (typeof value !== 'string') continue;
      expect(offenders(value), `common.${name}: "${value}"`).toEqual([]);
    }
  });

  it('names no internal term in a station kind', () => {
    for (const kind of stationKinds) {
      expect(offenders(`${kind.label} ${kind.how}`)).toEqual([]);
    }
  });

  it('names no internal term in a destination', () => {
    for (const d of destinations) {
      expect(offenders(`${d.label} ${d.summary}`)).toEqual([]);
    }
  });

  it('keeps the words that are the operator’s own', () => {
    // §0.3: "station" and "website" are not jargon in the other direction, and APRS/CWOP
    // keeps its own words because the audience has a callsign.
    const labels = nav.map((n) => n.label).join(' ');
    expect(labels).toContain('station');
    expect(labels).toContain('website');
    expect(destinations.map((d) => d.label)).toContain('APRS / CWOP');
  });
});

describe('§0.5’s hint standard', () => {
  const groups = [
    ['stations', stations.fields],
    ['storage', storage.fields],
    ['system', system.fields]
  ] as const;

  it('gives every field a hint of no more than ninety characters', () => {
    for (const [group, fields] of groups) {
      for (const [name, field] of Object.entries(fields)) {
        expect(field.hint, `${group}.${name}`).toBeTruthy();
        expect(field.hint.length, `${group}.${name}: "${field.hint}"`).toBeLessThanOrEqual(
          90
        );
      }
    }
  });

  it('names no internal term in a label or a hint', () => {
    for (const [group, fields] of groups) {
      for (const [name, field] of Object.entries(fields)) {
        const more = 'more' in field ? field.more : '';
        expect(offenders(`${field.label} ${field.hint} ${more}`), `${group}.${name}`).toEqual(
          []
        );
      }
    }
  });

  it('keeps the four fields M5-2 made non-negotiable', () => {
    // M5-2 measured the Go configuration's −94° against this side's 0 and it made
    // `winddir` differ on 100% of readings with nothing downstream able to tell. The
    // coordinates feed the clear-sky model and sunrise/sunset. All four are on the form.
    for (const key of ['windDirCorrection', 'latitude', 'longitude', 'altitude'] as const) {
      expect(stations.fields[key].hint).toBeTruthy();
    }
  });
});

describe('§0.4’s badge tones', () => {
  it('maps a tone to the chonky variant of the same name', () => {
    expect(variantFor('success')).toBe('success');
    expect(variantFor('warning')).toBe('warning');
    expect(variantFor('danger')).toBe('danger');
    expect(variantFor('info')).toBe('info');
    // chonky calls it `default`; §0.4 calls it neutral.
    expect(variantFor('neutral')).toBe('default');
    expect(variantFor('something-new')).toBe('default');
  });

  it('spins only the two states that have no honest colour', () => {
    // §6.3 gap G3: `Starting` and `Stopping` get `info` plus a spinner until a `pending`
    // variant exists upstream.
    expect(spins('starting')).toBe(true);
    expect(spins('stopping')).toBe(true);
    expect(spins('running')).toBe(false);
    expect(spins('failed')).toBe(false);
  });

  it('calls only failure and retry attention', () => {
    expect(needsAttention('failed')).toBe(true);
    expect(needsAttention('backoff')).toBe(true);
    // A waiting component has not failed; §2.5 shows the root cause instead.
    expect(needsAttention('waiting')).toBe(false);
    expect(needsAttention('stopped')).toBe(false);
  });
});

describe('§7.3’s station form', () => {
  it('shows only the fields a kind actually has', () => {
    // The worst entry in §7.3: today's form shows every field of `DeviceData` for every
    // station type, so an air-quality sensor is asked for a radio path.
    const air = shapeFor('airgradient');
    expect(air.wind).toBe(false);
    expect(air.snow).toBe(false);
    expect(air.sensorMapping).toBe(false);

    const davis = shapeFor('davis');
    expect(davis.connection).toBe('serial-or-network');
    expect(davis.defaultBaud).toBe(19200);

    const gauge = shapeFor('snowgauge');
    expect(gauge.snow).toBe(true);
    expect(gauge.wind).toBe(false);
  });

  it('keeps an unknown kind editable rather than blank', () => {
    // §4.4: a configuration written by a future version must still be editable by this
    // one, which is the same conservatism applied to a form.
    const unknown = shapeFor('something-new');
    expect(unknown.located).toBe(true);
    expect(unknown.connection).toBe('network');
  });

  it('covers every kind the chooser offers', () => {
    for (const kind of stationKinds) {
      expect(knownKinds, kind.kind).toContain(kind.kind);
    }
  });
});

describe('§7.1’s page inventory', () => {
  it('has twenty-seven pages, which is the table’s own count', () => {
    // The plan says "twenty-six"; §7.1's table has twenty-seven rows. A counting
    // correction rather than a finding of substance -- the same kind W5 made about its own
    // prompt's "46 routes" -- and stated so that "every page is built" cannot be satisfied
    // while one is quietly missed.
    expect(inventory).toHaveLength(27);
  });

  it('keeps the two unauthenticated pages out of the palette', () => {
    const palette = allPages.map((p) => p.route);
    expect(palette).toContain('/');
    // `/setup` and `/sign-in` are reachable only when they are the *only* thing
    // reachable, so offering them in the palette would be offering a dead end.
    expect(palette).not.toContain('/setup');
    expect(palette).not.toContain('/sign-in');
    // Every parameterised route is absent too: "Station — Settings" is not a destination
    // until there is a station.
    expect(palette.filter((r) => r.includes('['))).toEqual([]);
  });
});

describe('the values the server actually admits', () => {
  /**
   * These are **enums the schema enforces and the API does not describe**: the OpenAPI
   * document types them as `string`, so §4.3's two drift mechanisms cannot see a wrong
   * spelling -- mechanism 1 compares the schema to the handlers and mechanism 2 compares
   * the UI to the schema, and the value is in neither. A wrong one is a 422 on every save
   * of that page, and nothing fails until somebody opens it.
   *
   * So the spellings are pinned here, next to the two places that own them:
   *
   *   stations.type       `STATION_KINDS` in crates/mw-api-mgmt/src/change.rs, and the
   *                       `CHECK (transport_kind IS NULL OR type IN (...))` constraint in
   *                       migrations/config/20261102230000_config_schema_initial.sql
   *   aprs_transport      the `CHECK (aprs_transport IS NULL OR aprs_transport IN (...))`
   *                       constraint in the same migration
   *
   * The frozen contract is why they cannot be tidied: `campbellscientific` and
   * `ambient-customized` are what docs/legacy-api-contract.md §3.1 freezes, and the
   * migration's own comment says writing `campbell` would make M7-4's configuration
   * migration a silent contract break.
   */
  it('offers only station types the schema admits', () => {
    expect(stationKinds.map((k) => k.kind).sort()).toEqual([
      'airgradient',
      'ambient-customized',
      'campbellscientific',
      'davis',
      'snowgauge',
      'weatherlink-live'
    ]);
  });

  it('does not offer a received station as a type', () => {
    // `grpcreceiver` is an `ingest` row plus whatever a peer registers, and the schema's
    // `CHECK` excludes it by name. §0.7's seventh chooser row is a link to the page that
    // configures the pairing, which is `receivedFromPeer`.
    expect(stationKinds.map((k) => k.kind)).not.toContain('grpcreceiver');
    expect(receivedFromPeer.href).toBe('/data-storage/receive');
  });

  it('gives every offered type a shape, and every shape a streaming answer', () => {
    for (const kind of stationKinds) {
      const shape = shapeFor(kind.kind);
      expect(knownKinds, kind.kind).toContain(kind.kind);
      // `http_out` and `grpc_out` are refused without `transport.streaming`, so a shape
      // that left it undefined would be a type that can never be saved.
      expect(typeof shape.streams, kind.kind).toBe('boolean');
    }
  });
});
