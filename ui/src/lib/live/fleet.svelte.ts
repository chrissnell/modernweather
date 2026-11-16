import { api, asFailure, unwrap, type ApiFailure } from '../api/client';
import type { Schemas } from '../api/client';

/**
 * **N-11**'s numbers (docs/reconciliation.md C-W7-29).
 *
 * One read for the three things W7 specified after W5 had finished -- the unsent-readings
 * figures, the peers with their peer-reported depth, and what enrolment is open -- because
 * the two pages that render them show all three and two reads would be two moments.
 *
 * A `501` here is not an error: it means this node keeps no unsent readings and has no
 * peers, which is a configuration rather than a fault, and both pages have an empty state
 * that says so.
 */
class FleetStore {
  value = $state<Schemas['FleetView'] | null>(null);
  loading = $state(false);
  /** True when the node simply has nothing to report. */
  absent = $state(false);
  error = $state<ApiFailure | null>(null);

  get spool(): Schemas['SpoolView'] | null {
    return this.value?.spool ?? null;
  }

  get peers(): Schemas['PeerView'][] {
    return this.value?.peers ?? [];
  }

  get enrolment(): Schemas['EnrolmentView'] | null {
    return this.value?.enrolment ?? null;
  }

  async load(): Promise<void> {
    this.loading = true;
    try {
      this.value = unwrap(await api.GET('/api/v1/fleet'));
      this.absent = false;
      this.error = null;
    } catch (e) {
      const f = asFailure(e);
      this.absent = f.status === 501;
      this.error = this.absent ? null : f;
      if (this.absent) this.value = null;
    } finally {
      this.loading = false;
    }
  }
}

export const fleet = new FleetStore();
