import { auth as authCopy } from '../copy';
import type { Schemas } from '../api/client';

/**
 * §0.7's checklist, derived rather than stored.
 *
 * Deliberately **not** a persisted wizard position. §0.7's shape is "a resumable
 * checklist, not a modal wizard": each step's completion is a fact about the
 * configuration, so closing the browser loses nothing and a step that was completed and
 * then undone correctly reappears. A stored cursor would drift from the thing it claims
 * to describe, which is the defect §2 exists to remove from the rest of the product.
 *
 * "Done" is steps 1-3 (§0.7's last paragraph). The optional ones stay available and are
 * not nagged about.
 */
export interface Step {
  readonly key: 'account' | 'station' | 'readings' | 'storage' | 'website' | 'share';
  readonly label: string;
  readonly href: string;
  readonly done: boolean;
  readonly optional: boolean;
}

export interface Inputs {
  readonly claimed: boolean;
  readonly stations: readonly Schemas['StationView'][];
  readonly readingStationKeys: readonly string[];
  readonly hasStorage: boolean;
  readonly websites: readonly Schemas['WebsiteView'][];
  readonly services: readonly Schemas['ServiceView'][];
}

export function steps(i: Inputs): Step[] {
  const collecting = i.stations.filter((s) => s.collecting);
  return [
    {
      key: 'account',
      label: authCopy.firstRun.steps.account,
      href: '/system/administrator',
      done: i.claimed,
      optional: false
    },
    {
      key: 'station',
      label: authCopy.firstRun.steps.station,
      href: '/stations/new',
      done: i.stations.length > 0,
      optional: false
    },
    {
      key: 'readings',
      label: authCopy.firstRun.steps.readings,
      href: collecting.length > 0 ? `/stations/${collecting[0].key}/status` : '/stations',
      done: i.readingStationKeys.length > 0,
      optional: false
    },
    {
      key: 'storage',
      label: authCopy.firstRun.steps.storage,
      href: '/data-storage/database',
      done: i.hasStorage,
      optional: true
    },
    {
      key: 'website',
      label: authCopy.firstRun.steps.website,
      href: '/website',
      done: i.websites.length > 0,
      optional: true
    },
    {
      key: 'share',
      label: authCopy.firstRun.steps.share,
      href: '/share',
      done: i.services.some((s) => s.enabled),
      optional: true
    }
  ];
}

/** The first step that is not done -- "<- you are here". */
export function current(list: readonly Step[]): Step | null {
  return list.find((s) => !s.done) ?? null;
}

/** §0.7: first-run is over when steps 1-3 are complete. */
export function complete(list: readonly Step[]): boolean {
  return list.filter((s) => !s.optional).every((s) => s.done);
}
