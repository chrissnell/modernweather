/**
 * §5.2's reconnect and §5.3's backfill, as a pure state machine.
 *
 * Pure because this is one of the four things §6.7 names as worth a unit test: a bug in
 * reconnect or in the backfill boundary is **silent**. The page keeps rendering, the data
 * is simply older than it looks -- which is the exact failure §2 exists to remove from
 * the rest of the product, so leaving it in the transport would be funny rather than
 * acceptable. Nothing here touches `EventSource`, `window` or the clock; the driver in
 * `./stream.svelte.ts` does, and it is thin enough to read in one go.
 */

/** What the status indicator shows (§5.2). */
export type Phase = 'connecting' | 'live' | 'backfilling' | 'reconnecting' | 'disconnected';

export interface StreamState {
  readonly phase: Phase;
  /** Consecutive failed attempts. Reset by a successful open, not by a successful event. */
  readonly attempt: number;
  /** When the next attempt is due, as epoch milliseconds, while `reconnecting`. */
  readonly retryAt: number | null;
  /** The newest event id seen, which is what a reconnect resumes from. */
  readonly lastEventId: string | null;
  /** True once the server has said `caught_up` for the current connection. */
  readonly caughtUp: boolean;
  /** Set when the operator has given up and pressed nothing yet. */
  readonly givenUp: boolean;
}

export const initial: StreamState = {
  phase: 'connecting',
  attempt: 0,
  retryAt: null,
  lastEventId: null,
  caughtUp: false,
  givenUp: false
};

export type StreamEvent =
  | { kind: 'open' }
  | { kind: 'event'; id: string | null }
  | { kind: 'caught_up' }
  | { kind: 'error'; at: number; delayMs: number }
  | { kind: 'retry_now' }
  | { kind: 'give_up' }
  | { kind: 'stop' };

/**
 * `1s, 2s, 4s, 8s, 15s` with +/-20% jitter, and **capped low on purpose**: this is a LAN
 * tool watched by a person, so a 60-second cap would mean a UI that looks dead for a
 * minute after a daemon restart -- which is exactly the moment the operator is watching.
 *
 * `random` is a parameter so the test can pin it. A backoff whose test has to accept a
 * range is a backoff whose off-by-one nobody notices.
 */
export const BACKOFF_STEPS_MS = [1_000, 2_000, 4_000, 8_000, 15_000] as const;
export const BACKOFF_CAP_MS = 15_000;

export function delayFor(attempt: number, random: () => number = Math.random): number {
  const step = BACKOFF_STEPS_MS[Math.min(attempt, BACKOFF_STEPS_MS.length - 1)] ?? BACKOFF_CAP_MS;
  // +/-20%: 0.8 .. 1.2
  const jitter = 0.8 + random() * 0.4;
  return Math.round(step * jitter);
}

export function reduce(state: StreamState, event: StreamEvent): StreamState {
  switch (event.kind) {
    case 'open':
      // `backfilling` rather than `live` until the server says `caught_up`, so the log
      // view can draw the "— live —" rule in the right place instead of guessing (§5.3).
      return { ...state, phase: 'backfilling', attempt: 0, retryAt: null, caughtUp: false, givenUp: false };
    case 'caught_up':
      return { ...state, phase: 'live', caughtUp: true };
    case 'event':
      return event.id ? { ...state, lastEventId: event.id } : state;
    case 'error':
      return {
        ...state,
        phase: 'reconnecting',
        attempt: state.attempt + 1,
        retryAt: event.at + event.delayMs,
        caughtUp: false
      };
    case 'retry_now':
      return { ...state, phase: 'connecting', retryAt: null, givenUp: false };
    case 'give_up':
      return { ...state, phase: 'disconnected', retryAt: null, givenUp: true };
    case 'stop':
      return { ...initial, lastEventId: state.lastEventId };
  }
}

/** Seconds until the next attempt, for the "reconnecting (3s)" indicator. */
export function secondsUntil(retryAt: number | null, now: number): number {
  if (retryAt === null) return 0;
  return Math.max(0, Math.ceil((retryAt - now) / 1000));
}

/**
 * Whether a panel fed by this stream should dim and show "last updated {time}".
 *
 * §5.2: the UI never silently shows stale data as though it were live. `backfilling` is
 * **not** stale -- the records arriving are the current ring.
 */
export function isStale(state: StreamState): boolean {
  return state.phase === 'reconnecting' || state.phase === 'disconnected';
}
