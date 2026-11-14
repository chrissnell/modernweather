import { describe, expect, it } from 'vitest';
import {
  BUFFER_CAP,
  GAP_BURST,
  append,
  gapBurst,
  matches,
  type Row,
  type ViewFilter
} from '../../src/lib/sse/buffer';
import type { Schemas } from '../../src/lib/api/client';

function entry(
  level: string,
  message: string,
  stream = 'app',
  spans: string[] = []
): Row {
  const e: Schemas['LogEntry'] = {
    ts: '2026-11-14T00:00:00Z',
    level,
    stream,
    target: 'mw_station::davis',
    spans,
    message,
    causes: [],
    fields: []
  };
  return { kind: 'entry', ts: e.ts, entry: e };
}

const base: ViewFilter = { level: 'info', stream: 'both', station: null, search: '' };

describe('the client buffer', () => {
  it('is capped, newest wins', () => {
    // §6.3 gap G8: `LogViewer` renders every entry it is given, and the two server rings
    // hold 4,096 + 16,384 records -- about twenty thousand DOM rows.
    let rows: Row[] = [];
    for (let i = 0; i < BUFFER_CAP + 50; i++) {
      rows = append(rows, [entry('INFO', `line ${i}`)]);
    }
    expect(rows).toHaveLength(BUFFER_CAP);
    const last = rows[rows.length - 1];
    expect(last.kind === 'entry' && last.entry.message).toBe(`line ${BUFFER_CAP + 49}`);
  });
});

describe('the view filter', () => {
  it('hides entries below the chosen level and keeps the ones above', () => {
    expect(matches(entry('ERROR', 'x'), { ...base, level: 'warn' })).toBe(true);
    expect(matches(entry('INFO', 'x'), { ...base, level: 'warn' })).toBe(false);
    expect(matches(entry('DEBUG', 'x'), { ...base, level: 'debug' })).toBe(true);
  });

  it('narrows by stream', () => {
    expect(matches(entry('INFO', 'x', 'access'), { ...base, stream: 'app' })).toBe(false);
    expect(matches(entry('INFO', 'x', 'access'), { ...base, stream: 'access' })).toBe(true);
  });

  it('narrows by station through the span chain', () => {
    const row = entry('INFO', 'reading accepted', 'app', ['station=backyard']);
    expect(matches(row, { ...base, station: 'backyard' })).toBe(true);
    expect(matches(row, { ...base, station: 'frontyard' })).toBe(false);
  });

  it('never hides a marker', () => {
    // A gap explains the hole in the list. Filtering it out is the one filter effect that
    // would lose information rather than narrow it.
    const gap: Row = { kind: 'gap', ts: '2026-11-14T00:00:00Z', stream: 'access', dropped: 412 };
    expect(matches(gap, { ...base, level: 'error', stream: 'app' })).toBe(true);
    const caughtUp: Row = { kind: 'caught_up', ts: '2026-11-14T00:00:00Z' };
    expect(matches(caughtUp, { ...base, level: 'error' })).toBe(true);
  });
});

describe('§5.4’s repeated-gap explanation', () => {
  it('appears at three gaps inside a minute and not at two', () => {
    const now = Date.parse('2026-11-14T00:01:00Z');
    const recent = (seconds: number): Row => ({
      kind: 'gap',
      ts: new Date(now - seconds * 1000).toISOString(),
      stream: 'access',
      dropped: 10
    });
    expect(gapBurst([recent(1), recent(2)], now)).toBe(false);
    expect(gapBurst([recent(1), recent(2), recent(3)], now)).toBe(true);
    // Outside the window, so it is three gaps over an afternoon rather than a burst.
    expect(gapBurst([recent(1), recent(2), recent(600)], now)).toBe(false);
    expect(GAP_BURST).toBe(3);
  });
});
