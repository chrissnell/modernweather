import { api, asFailure, unwrap, type ApiFailure } from '../api/client';
import type { Schemas } from '../api/client';

/**
 * The configuration, read once and shared.
 *
 * `GET /api/v1/config` returns the whole document with every secret omitted (D-67), and
 * every other read in §1 is a projection of it -- so one read backs the station list, the
 * website list, the share grid and the first-run checklist, and they cannot disagree with
 * each other about what is configured. Reloaded after any save, which is also when the
 * generation moves.
 */
class Config {
  value = $state<Schemas['ConfigView'] | null>(null);
  loading = $state(false);
  error = $state<ApiFailure | null>(null);

  get stations(): Schemas['StationView'][] {
    return this.value?.stations ?? [];
  }

  get websites(): Schemas['WebsiteView'][] {
    return this.value?.websites ?? [];
  }

  get services(): Schemas['ServiceView'][] {
    return this.value?.services ?? [];
  }

  get storage(): Schemas['StorageView'] | null {
    return this.value?.storage ?? null;
  }

  get forecasts(): Schemas['ForecastsView'] | null {
    return this.value?.forecasts ?? null;
  }

  get listener(): Schemas['ListenerView'] | null {
    return this.value?.listener ?? null;
  }

  get node(): Schemas['NodeView'] | null {
    return this.value?.node ?? null;
  }

  get generation(): number {
    return this.value?.generation ?? 0;
  }

  station(key: string): Schemas['StationView'] | null {
    return this.stations.find((s) => s.key === key) ?? null;
  }

  website(id: number): Schemas['WebsiteView'] | null {
    return this.websites.find((w) => w.id === id) ?? null;
  }

  service(name: string): Schemas['ServiceView'] | null {
    return this.services.find((s) => s.service === name) ?? null;
  }

  async load(): Promise<void> {
    this.loading = true;
    try {
      this.value = unwrap(await api.GET('/api/v1/config'));
      this.error = null;
    } catch (e) {
      this.error = asFailure(e);
    } finally {
      this.loading = false;
    }
  }

  /** Called after every save, which is the one moment the shared copy is known stale. */
  async refresh(): Promise<void> {
    await this.load();
  }
}

export const config = new Config();

/** §2.5's rollup, which the SSE `components` channel keeps current between reads. */
class Summary {
  value = $state<Schemas['SummaryView'] | null>(null);
  error = $state<ApiFailure | null>(null);

  async load(): Promise<void> {
    try {
      this.value = unwrap(await api.GET('/api/v1/runtime/summary'));
      this.error = null;
    } catch (e) {
      this.error = asFailure(e);
    }
  }
}

export const summary = new Summary();
