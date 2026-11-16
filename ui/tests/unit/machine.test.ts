import { describe, expect, it } from 'vitest';
import {
  BACKOFF_CAP_MS,
  delayFor,
  initial,
  isStale,
  reduce,
  secondsUntil,
  type StreamState
} from '../../src/lib/sse/machine';

/**
 * §6.7 names the SSE reconnect/backfill state machine as one of the four things worth a
 * unit test, and the reason is the failure mode: a bug here is **silent**. The page keeps
 * rendering and the data is simply older than it looks, which is the exact defect §2
 * exists to remove from the rest of the product.
 */
describe('the reconnect schedule', () => {
  it('is 1, 2, 4, 8 then capped at 15 seconds', () => {
    // `random` pinned to the midpoint, so the assertion is on the step rather than on a
    // range -- a backoff whose test accepts a range is a backoff whose off-by-one nobody
    // notices.
    const mid = () => 0.5;
    expect(delayFor(0, mid)).toBe(1_000);
    expect(delayFor(1, mid)).toBe(2_000);
    expect(delayFor(2, mid)).toBe(4_000);
    expect(delayFor(3, mid)).toBe(8_000);
    expect(delayFor(4, mid)).toBe(BACKOFF_CAP_MS);
    expect(delayFor(99, mid)).toBe(BACKOFF_CAP_MS);
  });

  it('jitters by no more than ±20%', () => {
    expect(delayFor(2, () => 0)).toBe(3_200);
    expect(delayFor(2, () => 1)).toBe(4_800);
  });

  it('is capped low, because this is a LAN tool a person is watching', () => {
    // The decision, as a test: a 60-second cap would mean a UI that looks dead for a
    // minute after a daemon restart -- which is exactly when the operator is looking.
    expect(BACKOFF_CAP_MS).toBeLessThanOrEqual(15_000);
  });
});

describe('the backfill boundary', () => {
  it('is backfilling until the server says caught_up', () => {
    let state = reduce(initial, { kind: 'open' });
    expect(state.phase).toBe('backfilling');
    expect(state.caughtUp).toBe(false);
    state = reduce(state, { kind: 'caught_up' });
    expect(state.phase).toBe('live');
    expect(state.caughtUp).toBe(true);
  });

  it('does not treat backfilling as stale', () => {
    // The records arriving during a backfill are the ring's current contents, so dimming
    // the panel would be a lie in the other direction.
    const state = reduce(initial, { kind: 'open' });
    expect(isStale(state)).toBe(false);
  });

  it('dims the page while reconnecting and while disconnected', () => {
    const errored = reduce(initial, { kind: 'error', at: 0, delayMs: 1_000 });
    expect(isStale(errored)).toBe(true);
    expect(isStale(reduce(errored, { kind: 'give_up' }))).toBe(true);
  });
});

describe('the resume point', () => {
  it('advances only on an event that carried an id', () => {
    let state = reduce(initial, { kind: 'open' });
    state = reduce(state, { kind: 'event', id: '7' });
    expect(state.lastEventId).toBe('7');
    // A backfill frame carries no id, so it must not move the resume point: resuming from
    // a replayed snapshot would resume from a position that never existed in the stream.
    state = reduce(state, { kind: 'event', id: null });
    expect(state.lastEventId).toBe('7');
  });

  it('survives a stop, so a reconnect does not refetch the whole ring', () => {
    let state = reduce(initial, { kind: 'open' });
    state = reduce(state, { kind: 'event', id: '42' });
    state = reduce(state, { kind: 'stop' });
    expect(state.lastEventId).toBe('42');
    expect(state.phase).toBe('connecting');
  });
});

describe('the attempt counter', () => {
  it('resets on a successful open rather than on a successful event', () => {
    let state: StreamState = initial;
    state = reduce(state, { kind: 'error', at: 0, delayMs: 1_000 });
    state = reduce(state, { kind: 'error', at: 1_000, delayMs: 2_000 });
    expect(state.attempt).toBe(2);
    state = reduce(state, { kind: 'open' });
    expect(state.attempt).toBe(0);
  });
});

describe('the countdown', () => {
  it('rounds up, so it never shows zero while still waiting', () => {
    expect(secondsUntil(3_400, 1_000)).toBe(3);
    expect(secondsUntil(1_001, 1_000)).toBe(1);
    expect(secondsUntil(null, 1_000)).toBe(0);
    expect(secondsUntil(500, 1_000)).toBe(0);
  });
});
