/**
 * Which fields a station's form shows, decided by its kind.
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

export type ConnectionShape = 'serial-or-network' | 'network' | 'inbound' | 'peer';

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
    defaultBaud: 19200
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
    defaultBaud: 115200
  },
  weatherlinklive: {
    connection: 'network',
    wind: true,
    snow: false,
    polled: true,
    sensorMapping: true,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'http_out'
  },
  'ambient-customized': {
    connection: 'inbound',
    wind: true,
    snow: false,
    polled: false,
    sensorMapping: false,
    presentsIdentity: true,
    located: true,
    defaultTransport: 'http_in'
  },
  airgradient: {
    connection: 'inbound',
    wind: false,
    snow: false,
    polled: false,
    sensorMapping: false,
    presentsIdentity: true,
    located: true,
    defaultTransport: 'http_in'
  },
  snowgauge: {
    connection: 'network',
    wind: false,
    snow: true,
    polled: true,
    sensorMapping: false,
    presentsIdentity: false,
    located: true,
    defaultTransport: 'http_out'
  },
  grpcreceiver: {
    connection: 'peer',
    wind: true,
    snow: true,
    polled: false,
    sensorMapping: false,
    presentsIdentity: true,
    located: false,
    defaultTransport: 'grpc_out'
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
      defaultTransport: 'tcp_out'
    }
  );
}

export const knownKinds = Object.keys(SHAPES);

/** Whether a serial device picker belongs on this form at all. */
export function needsSerial(kind: string, transport: string | null | undefined): boolean {
  return shapeFor(kind).connection === 'serial-or-network' && transport === 'serial';
}
