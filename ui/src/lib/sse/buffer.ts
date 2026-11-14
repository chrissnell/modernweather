import type { Schemas } from '../api/client';

/**
 * The client-side log buffer, and §6.3 gap **G8**'s answer.
 *
 * `LogViewer` does not virtualise -- it renders every entry it is given -- and the two
 * server rings hold 4,096 + 16,384 records, which would be ~20,000 DOM rows. So the
 * buffer is **capped at 2,000 entries, newest wins**, and the view says so with a link to
 * the paged full log. The alternative was a second log component, which is the thing the
 * gap note says not to do.
 */
export const BUFFER_CAP = 2_000;

/** A dropped-records marker, rendered inline in the list rather than as a toast (§5.4). */
export interface GapRow {
  readonly kind: 'gap';
  readonly ts: string;
  readonly stream: string;
  readonly dropped: number;
}

/** The boundary between the backfill and the live records (§5.3). */
export interface CaughtUpRow {
  readonly kind: 'caught_up';
  readonly ts: string;
}

export interface EntryRow {
  readonly kind: 'entry';
  readonly ts: string;
  readonly entry: Schemas['LogEntry'];
}

export type Row = EntryRow | GapRow | CaughtUpRow;

/** Appends with the cap applied, oldest dropped first. Returns a new array. */
export function append(rows: readonly Row[], next: readonly Row[]): Row[] {
  const out = rows.concat(next);
  return out.length > BUFFER_CAP ? out.slice(out.length - BUFFER_CAP) : out;
}

/**
 * §5.4: three gap markers inside a minute is a volume problem, and the operator deserves
 * to be told that rather than left to wonder whether the system is losing data.
 */
export const GAP_BURST = 3;
export const GAP_WINDOW_MS = 60_000;

export function gapBurst(rows: readonly Row[], now: number): boolean {
  let seen = 0;
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i];
    if (row.kind !== 'gap') continue;
    if (now - Date.parse(row.ts) > GAP_WINDOW_MS) break;
    seen++;
    if (seen >= GAP_BURST) return true;
  }
  return false;
}

/** The view filter of §5.5 -- client-side, instant, and destructive of nothing. */
export interface ViewFilter {
  readonly level: 'error' | 'warn' | 'info' | 'debug' | 'trace';
  readonly stream: 'app' | 'access' | 'both';
  readonly station: string | null;
  readonly search: string;
}

const RANK: Record<string, number> = { ERROR: 0, WARN: 1, INFO: 2, DEBUG: 3, TRACE: 4 };

export function matches(row: Row, filter: ViewFilter): boolean {
  // Markers are structural: hiding the gap that explains the hole in the list would be
  // the one filter effect that loses information rather than narrowing it.
  if (row.kind !== 'entry') return true;
  const e = row.entry;
  if (filter.stream !== 'both' && e.stream !== filter.stream) return false;
  const want = RANK[filter.level.toUpperCase()] ?? 2;
  const got = RANK[e.level.toUpperCase()] ?? 2;
  if (got > want) return false;
  if (filter.station && !e.spans.some((s) => s.includes(filter.station!))) return false;
  if (filter.search) {
    const needle = filter.search.toLowerCase();
    const hay = [e.message, e.target, ...e.fields.map((f) => `${f.name}=${f.value}`)]
      .join(' ')
      .toLowerCase();
    if (!hay.includes(needle)) return false;
  }
  return true;
}
