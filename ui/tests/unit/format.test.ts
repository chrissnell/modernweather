import { describe, expect, it } from 'vitest';
import { ago, bytes, compass, duration, measure, shortAgo } from '../../src/lib/format';

/**
 * §6.5's first explicit prohibition, as a test: **no date library at all.** The current UI
 * vendors a deprecated copy of moment.js; this module is `Intl` and arithmetic, and these
 * are the cases where arithmetic is easy to get subtly wrong.
 */
describe('relative time', () => {
  const now = Date.parse('2026-11-14T12:00:00Z');

  it('reads in the operator’s words', () => {
    expect(ago('2026-11-14T11:59:58Z', now)).toMatch(/2 seconds ago/);
    expect(ago('2026-11-14T11:56:00Z', now)).toMatch(/4 minutes ago/);
    expect(ago('2026-11-14T09:00:00Z', now)).toMatch(/3 hours ago/);
  });

  it('counts forward too, which is what the Backoff countdown needs', () => {
    // §5.6's one permitted client-side timer renders a server-supplied **absolute**
    // instant, so a future timestamp has to read as a future one.
    expect(ago('2026-11-14T12:00:12Z', now)).toMatch(/in 12 seconds/);
  });

  it('says "never" rather than showing an epoch', () => {
    expect(ago(null, now)).toBe('never');
    expect(shortAgo(undefined, now)).toBe('—');
  });

  it('compacts for a table cell', () => {
    expect(shortAgo('2026-11-14T11:59:58Z', now)).toBe('2s');
    expect(shortAgo('2026-11-14T11:56:00Z', now)).toBe('4m');
    expect(shortAgo('2026-11-14T09:00:00Z', now)).toBe('3h');
    expect(shortAgo('2026-11-11T12:00:00Z', now)).toBe('3d');
  });
});

describe('durations', () => {
  it('reads as words, with two units at most', () => {
    expect(duration(600)).toBe('10 minutes');
    expect(duration(3_900)).toBe('1 hour 5 minutes');
    expect(duration(604_800)).toBe('7 days');
    expect(duration(1)).toBe('1 second');
    expect(duration(0.4)).toBe('less than a second');
    expect(duration(null)).toBe('—');
  });
});

describe('sizes', () => {
  it('is decimal, because a quota an operator set is a decimal number', () => {
    expect(bytes(512)).toBe('512 B');
    expect(bytes(1_500)).toBe('1.5 kB');
    expect(bytes(1_200_000)).toBe('1.2 MB');
    expect(bytes(5_000_000_000)).toBe('5.0 GB');
    expect(bytes(null)).toBe('—');
  });
});

describe('the compass', () => {
  it('is sixteen points, which is how people say it', () => {
    expect(compass(0)).toBe('N');
    expect(compass(247.5)).toBe('WSW');
    expect(compass(360)).toBe('N');
    // Negative and over-360 both normalise rather than producing a blank.
    expect(compass(-22.5)).toBe('NNW');
    expect(compass(382.5)).toBe('NNE');
    expect(compass(null)).toBe('—');
  });
});

describe('measurements', () => {
  it('never shows zero in place of "no value"', () => {
    // A station that reports no solar sensor must not read `0.0 W/m²`, which is a
    // measurement rather than an absence.
    expect(measure(null, 'W/m²', 0)).toBe('—');
    expect(measure(undefined, '°F')).toBe('—');
    expect(measure(0, 'W/m²', 0)).toBe('0 W/m²');
    expect(measure(54.32, '°F')).toBe('54.3 °F');
  });
});
