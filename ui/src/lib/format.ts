/**
 * Dates, durations and sizes, with **no date library at all** -- §6.5's first explicit
 * prohibition, and a defect in the current UI, which vendors a deprecated copy of
 * moment.js.
 *
 * `Intl.DateTimeFormat` and `Intl.RelativeTimeFormat` are built into every browser that
 * can run Svelte 5, handle time zones properly, and cost zero bytes. Everything below is
 * a thin wrapper over them, in one module, so that "we need a date library" has to be
 * argued against a file rather than asserted in a pull request.
 */

const time = new Intl.DateTimeFormat(undefined, { timeStyle: 'medium' });
const dateTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'long' });

/** `21:14:02` -- the log view's column, in the operator's own locale and zone. */
export function clock(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '—' : time.format(d);
}

/** `29 Oct 2026, 21:14` -- for anything that is not a log row. */
export function stamp(iso: string | null | undefined): string {
  if (!iso) return 'Never';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '—' : dateTime.format(d);
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ['year', 31_536_000],
  ['month', 2_592_000],
  ['week', 604_800],
  ['day', 86_400],
  ['hour', 3_600],
  ['minute', 60],
  ['second', 1]
];

/**
 * `2 seconds ago` / `in 12 seconds`, from an absolute instant.
 *
 * **Absolute input on purpose.** §5.6's one permitted client-side timer is the `Backoff`
 * countdown, and the thing it counts down to is a server-supplied absolute timestamp --
 * so a clock that disagrees with the server's shows a slightly wrong number rather than a
 * number that never arrives.
 */
export function ago(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return 'never';
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '—';
  const deltaSeconds = (t - now) / 1000;
  const magnitude = Math.abs(deltaSeconds);
  for (const [unit, seconds] of UNITS) {
    if (magnitude >= seconds || unit === 'second') {
      return relative.format(Math.round(deltaSeconds / seconds), unit);
    }
  }
  return 'now';
}

/** `2s` / `4m` / `3h` -- the compact form, for a table cell that has no room for prose. */
export function shortAgo(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return '—';
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '—';
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3_600) return `${Math.round(s / 60)}m`;
  if (s < 86_400) return `${Math.round(s / 3_600)}h`;
  return `${Math.round(s / 86_400)}d`;
}

/** `10 minutes` / `1 hour 5 minutes` -- a duration an operator reads, not an ISO 8601 one. */
export function duration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return '—';
  if (seconds < 1) return 'less than a second';
  const parts: string[] = [];
  const units: [string, number][] = [
    ['day', 86_400],
    ['hour', 3_600],
    ['minute', 60],
    ['second', 1]
  ];
  let left = Math.round(seconds);
  for (const [name, size] of units) {
    const n = Math.floor(left / size);
    if (n > 0) {
      parts.push(`${n} ${name}${n === 1 ? '' : 's'}`);
      left -= n * size;
    }
    if (parts.length === 2) break;
  }
  return parts.join(' ');
}

/** `1.2 MB` -- decimal, because a disk quota an operator set is a decimal number. */
export function bytes(n: number | null | undefined): string {
  if (n === null || n === undefined) return '—';
  if (n < 1000) return `${n} B`;
  const units = ['kB', 'MB', 'GB', 'TB'];
  let v = n / 1000;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** `4 mph from the WSW` -- the sixteen-point compass, which is how people say it. */
const POINTS = [
  'N', 'NNE', 'NE', 'ENE', 'E', 'ESE', 'SE', 'SSE',
  'S', 'SSW', 'SW', 'WSW', 'W', 'WNW', 'NW', 'NNW'
];

export function compass(degrees: number | null | undefined): string {
  if (degrees === null || degrees === undefined) return '—';
  const i = Math.round((((degrees % 360) + 360) % 360) / 22.5) % 16;
  return POINTS[i];
}

/** A measurement with its unit, or an em dash. Never `0` standing in for "no value". */
export function measure(
  value: number | null | undefined,
  unit: string,
  decimals = 1
): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '—';
  return `${value.toFixed(decimals)} ${unit}`;
}
