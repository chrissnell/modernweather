import type { Schemas } from '../api/client';

/**
 * §2.2's one derived fact: **a change is live when every affected component's
 * `applied_generation` is at least the generation the change committed at.**
 *
 * Its own module with its own test because it is the mechanism the whole of §2 rests on,
 * and because getting it wrong fails in the direction that matters -- reporting "live"
 * for a component that was never restarted is precisely the lie §2.1 removes.
 *
 * The server computes `pending` as well (§2.3), and these two must agree. They are not
 * redundant: the server's answer is scoped to the newest change, and this one is scoped
 * to **a specific save the operator just made**, which is what the banner is about.
 */
export interface Awaiting {
  /** The generation the save committed at. */
  readonly generation: number;
  /** The ids the save affects -- `Applied.affects`, the union of `start` and `restart`. */
  readonly affects: readonly string[];
  /** When the save happened, for §2.4's 10-second "not live yet" threshold. */
  readonly savedAt: number;
}

export type ApplyPhase =
  | { kind: 'none' }
  | { kind: 'catching_up'; remaining: number }
  | { kind: 'live' }
  | { kind: 'not_yet'; blocked: Schemas['ComponentView'][] }
  | { kind: 'failed'; failed: Schemas['ComponentView'][] };

/** How long §2.4 waits before "Saved. 1 item is restarting…" becomes "not running yet". */
export const NOT_YET_AFTER_MS = 10_000;

/**
 * Which of §2.4's five states a save is in.
 *
 * Ordered by what the operator most needs to know, which is not the order the states
 * occur in: a component that restarted and then **failed** (⑤) is the headline even if
 * others are still catching up, because it is the one that needs a decision.
 */
export function phase(
  awaiting: Awaiting | null,
  components: readonly Schemas['ComponentView'][],
  now: number
): ApplyPhase {
  if (!awaiting) return { kind: 'none' };
  const affected = components.filter((c) => awaiting.affects.includes(c.id));

  // A component the save stopped is gone from the list and never reports a generation
  // again, so `affects` carrying only start+restart is what stops this waiting forever.
  const failed = affected.filter(
    (c) => c.applied_generation >= awaiting.generation && c.state === 'failed'
  );
  if (failed.length > 0) return { kind: 'failed', failed };

  const behind = affected.filter((c) => c.applied_generation < awaiting.generation);
  if (behind.length === 0) return { kind: 'live' };

  if (now - awaiting.savedAt < NOT_YET_AFTER_MS) {
    return { kind: 'catching_up', remaining: behind.length };
  }
  return { kind: 'not_yet', blocked: behind };
}
