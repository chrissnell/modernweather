import type { Schemas } from '../api/client';

/**
 * The `GET /api/v1/events` wire format, as one place both ends agree on.
 *
 * §5.1 types every frame with the SSE `event:` field rather than putting a discriminator
 * inside the payload, so a client that only wants one channel can ignore the rest without
 * parsing it.
 */

/** §5.3: component state backfills as a **full snapshot**, then deltas. */
export interface ComponentSnapshot {
  readonly kind: 'snapshot';
  readonly generation: number;
  readonly components: Schemas['ComponentView'][];
}

export interface ComponentDelta {
  readonly kind: 'delta';
  readonly generation: number;
  readonly component: Schemas['ComponentView'];
}

export interface ComponentRemoved {
  readonly kind: 'removed';
  readonly generation: number;
  readonly id: string;
}

export type ComponentFrame = ComponentSnapshot | ComponentDelta | ComponentRemoved;

/** One station's latest reading, as the station tiles and §0.7 step 3 render it. */
export interface ReadingFrame {
  readonly station_key: string;
  readonly station_name: string;
  readonly ts: string;
  readonly temperature_f?: number | null;
  readonly humidity_pct?: number | null;
  readonly barometer_inhg?: number | null;
  readonly wind_speed_mph?: number | null;
  readonly wind_dir_degrees?: number | null;
  readonly solar_wm2?: number | null;
  readonly rain_day_in?: number | null;
  readonly snow_depth_in?: number | null;
}

/** W3 §5.7's synthetic overflow record, rendered inline in the list (§5.4). */
export interface GapFrame {
  readonly kind: 'gap';
  readonly stream: string;
  readonly dropped: number;
  readonly ts: string;
}

/**
 * §5.3's explicit boundary between the backfill and the live records.
 *
 * Its own `event:` type rather than one of §5.1's five. Folding it into `log` would make
 * it a log record, which it is not -- it is a statement about the connection -- and the
 * log view has to draw "— live —" at exactly this point rather than guess.
 */
export interface CaughtUpFrame {
  readonly kind: 'caught_up';
  readonly ts: string;
}

export const CHANNELS = ['components', 'logs', 'readings'] as const;
export type Channel = (typeof CHANNELS)[number];
