import { describe, expect, it } from 'vitest';
import { NOT_YET_AFTER_MS, phase, type Awaiting } from '../../src/lib/live/applied';
import type { Schemas } from '../../src/lib/api/client';

/**
 * §2.2's derived fact, which the whole of §2 rests on: **a change is live when every
 * affected component's `applied_generation` is at least the generation the change
 * committed at.**
 *
 * Worth its own test because getting it wrong fails in the direction that matters --
 * reporting "live" for a component that was never restarted is precisely the lie §2.1
 * removes.
 */
function component(
  id: string,
  applied: number,
  state = 'running'
): Schemas['ComponentView'] {
  return {
    id,
    display: { name: id, section: 'share', href: '/share' },
    state,
    badge: { label: 'Running', tone: 'success' },
    sentence: 'Running.',
    waiting_on: [],
    degraded: [],
    since: '2026-11-14T00:00:00Z',
    applied_generation: applied,
    pending: false,
    schedule: { kind: 'every' },
    counters: {
      runs_total: 1,
      failed_total: 0,
      skipped_total: 0,
      deadline_exceeded_total: 0,
      misfired_total: 0,
      restart_total: 0,
      lagged_total: 0,
      stop_timeout_total: 0
    },
    notes: []
  };
}

const saved: Awaiting = { generation: 42, affects: ['wu:backyard'], savedAt: 1_000 };

describe('whether a save is live', () => {
  it('is nothing at all before a save', () => {
    expect(phase(null, [], 2_000).kind).toBe('none');
  });

  it('is catching up while the affected component is behind', () => {
    const result = phase(saved, [component('wu:backyard', 41)], 2_000);
    expect(result).toEqual({ kind: 'catching_up', remaining: 1 });
  });

  it('is live once every affected component has caught up', () => {
    expect(phase(saved, [component('wu:backyard', 42)], 2_000).kind).toBe('live');
    expect(phase(saved, [component('wu:backyard', 43)], 2_000).kind).toBe('live');
  });

  it('ignores a component the save did not affect', () => {
    // The whole point of `applied_generation` being per component: an unchanged component
    // is deliberately not restarted and keeps its old generation for ever, so a global
    // comparison would never report live again.
    const result = phase(
      saved,
      [component('wu:backyard', 42), component('aprs:backyard', 7)],
      2_000
    );
    expect(result.kind).toBe('live');
  });

  it('becomes "not running yet" after ten seconds, not before', () => {
    const components = [component('wu:backyard', 41)];
    expect(phase(saved, components, saved.savedAt + NOT_YET_AFTER_MS - 1).kind).toBe(
      'catching_up'
    );
    expect(phase(saved, components, saved.savedAt + NOT_YET_AFTER_MS).kind).toBe('not_yet');
  });

  it('reports a failure ahead of anything still catching up', () => {
    // §2.4 ⑤ is the headline even while others are mid-restart, because it is the one
    // that needs a decision from the operator.
    const result = phase(
      { ...saved, affects: ['wu:backyard', 'pws:backyard'] },
      [component('wu:backyard', 42, 'failed'), component('pws:backyard', 41)],
      2_000
    );
    expect(result.kind).toBe('failed');
  });

  it('does not call a component that has not restarted yet a failure', () => {
    // A component still on the old generation that happens to be in a failed state is
    // reporting the OLD configuration's failure, and saying "your change broke it" would
    // be the wrong sentence.
    const result = phase(saved, [component('wu:backyard', 41, 'failed')], 2_000);
    expect(result.kind).toBe('catching_up');
  });
});
