/**
 * Which fields a station's form shows, decided by its kind.
 *
 * **The authority is `crates/mw-station/src/plan.rs`'s own table**, which maps
 * `(type, transport_kind)` to the components it plans, and anything outside it is
 * `PlanError` -- "station `x` is a `y` with a `z` transport, which that driver has no mode
 * for". It is reproduced here because a form that offers a transport the driver has no
 * mode for is a Save that cannot succeed:
 *
 * | `type` | `transport_kind` |
 * |---|---|
 * | `weatherlink-live` | `udp_in`, or `http_out` polling `/v1/current_conditions` |
 * | `davis` | `serial` or `tcp_out` |
 * | `campbellscientific` | `serial` or `tcp_out` |
 * | `airgradient` | `http_out`, polling `/measures/current` |
 * | `ambient-customized` | `http_in` |
 * | `snowgauge` | `grpc_out` |
 *
 * §7.3's worst offender: today's form shows **every** field of `DeviceData` -- 50-plus
 * columns spanning Davis serial settings, PWS credentials, APRS KISS TNC paths and
 * WeatherLink sensor mappings -- for every station type, so an AirGradient sensor's form
 * asks for a radio path. §0.7 inverts it: type first, then only that type's fields.
 *
 * A table rather than a chain of `{#if kind === ...}` blocks in the template, because a
 * table is testable and because "which fields does a snow gauge have" is then a question
 * with one answer in one place.
 */

export type ConnectionShape = 'serial-or-network' | 'network' | 'inbound';

export interface KindShape {
  /** How this hardware is reached, which decides the connection fields. */
  readonly connection: ConnectionShape;
  /** Whether the station reports a wind vane, so the correction field is meaningful. */
  readonly wind: boolean;
  /** Whether it reports snow depth, which needs the base distance. */
  readonly snow: boolean;
  /** Whether a poll interval applies (a polled unit rather than a stream). */
  readonly polled: boolean;
  /** Whether "which sensor is which" applies (WeatherLink Live only -- §1.6). */
  readonly sensorMapping: boolean;
  /** Whether the station announces its own identifier rather than being fixed. */
  readonly presentsIdentity: boolean;
  /** Whether a location is meaningful -- it feeds sunrise and the clear-sky model. */
  readonly located: boolean;
  /** The transport `kind` a create defaults to. */
  readonly defaultTransport: string;
  /**
   * Whether this hardware's outbound transport streams rather than polls.
   *
   * **Not optional, and not cosmetic.** `http_out` and `grpc_out` are refused without
   * `transport.streaming` -- W2a §4a.3's trigger-implication table reads it at plan time
   * to decide whether the component is `OnReading` or `Every` -- so a form that leaves it
   * unset can never save a WeatherLink Live or a snow gauge at all.
   */
  readonly streams: boolean;
  /** The baud rate this hardware actually uses, offered as the default. */
  readonly defaultBaud?: number;
}

const SHAPES: Record<string, KindShape> = {
  davis: {
    connection: 'serial-or-network',
    wind: true,
    snow: false,
    polled: false,
    sensorMapping: false,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'serial',
    defaultBaud: 19200,
    streams: false
  },
  campbellscientific: {
    connection: 'serial-or-network',
    wind: true,
    snow: false,
    polled: false,
    sensorMapping: false,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'tcp_out',
    defaultBaud: 115200,
    streams: false
  },
  'weatherlink-live': {
    connection: 'network',
    wind: true,
    snow: false,
    polled: true,
    sensorMapping: true,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'http_out',
    // A WeatherLink Live is polled over http: the unit answers a request, it does not
    // push. `streaming: false` is therefore a fact about the hardware rather than a
    // default, and the plan reads it to make this an `Every` component.
    streams: false
  },
  'ambient-customized': {
    connection: 'inbound',
    wind: true,
    snow: false,
    polled: false,
    sensorMapping: false,
    presentsIdentity: true,
    located: true,
    defaultTransport: 'http_in',
    streams: false
  },
  airgradient: {
    // `http_out`, not inbound: this node polls `/measures/current`. An outbound transport
    // therefore means a `fixed` identity, which the schema requires of it.
    connection: 'network',
    wind: false,
    snow: false,
    polled: true,
    sensorMapping: false,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'http_out',
    streams: false
  },
  snowgauge: {
    // `grpc_out`, which `mw-grpc` plans -- `mw-station` validates the row and emits
    // nothing, because that crate may not name `tonic`. It is the one kind whose transport
    // carries `streaming`, and `mw_station::plan` requires it **set**: "a snow gauge
    // streams readings, so the flag has to be set; `transport_streaming = 0` would declare
    // one request per occurrence and no stream". Not polled, therefore, and not a choice.
    connection: 'network',
    wind: false,
    snow: true,
    polled: false,
    sensorMapping: false,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'grpc_out',
    streams: true
  }
};

/**
 * The shape for a kind, or a conservative default.
 *
 * An unknown kind shows the location and the network connection rather than nothing: a
 * configuration written by a future version should still be **editable** by this one,
 * which is §4.4's "every resource is an object with named fields" applied to a form.
 */
export function shapeFor(kind: string): KindShape {
  return (
    SHAPES[kind] ?? {
      connection: 'network',
      wind: true,
      snow: false,
      polled: false,
      sensorMapping: false,
      presentsIdentity: false,
      located: true,
      defaultTransport: 'tcp_out',
      streams: false
    }
  );
}

export const knownKinds = Object.keys(SHAPES);

/** Whether a serial device picker belongs on this form at all. */
export function needsSerial(kind: string, transport: string | null | undefined): boolean {
  return shapeFor(kind).connection === 'serial-or-network' && transport === 'serial';
}
