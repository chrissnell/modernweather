import { field, type FieldCopy } from './types';

/** The §0.7 step-2 chooser: what kind, and the one line that tells them which to pick. */
export interface StationKindCopy {
  readonly kind: string;
  readonly label: string;
  readonly how: string;
}

/**
 * The kinds, in the order §0.7 lists them.
 *
 * `kind` is `stations.type`, and the **six** values its `CHECK` constraint admits -- in
 * the frozen contract's spelling, which is what makes `weatherlink-live` hyphenated and
 * `ambient-customized` and `campbellscientific` not. A seventh spelling here is a 422 on
 * every create, and the schema's own comment says why the names cannot be tidied: writing
 * `campbell` would make M7-4's configuration migration a silent contract break.
 *
 * §0.7's chooser has a seventh row, "Another RemoteWeather", and it is **not** a station
 * type: a received station is an `ingest` row plus whatever a peer registers, which is why
 * the schema's `CHECK` excludes `grpcreceiver` by name. The row is still offered, because
 * it is what the operator is looking for -- it just goes to the page that configures the
 * pairing, which is where §0.6 puts it.
 */
export const stationKinds: readonly StationKindCopy[] = [
  { kind: 'davis', label: 'Davis Vantage Pro or Vue', how: 'Connects by cable or network' },
  { kind: 'weatherlink-live', label: 'Davis WeatherLink Live', how: 'Connects over your network' },
  {
    kind: 'campbellscientific',
    label: 'Campbell Scientific logger',
    how: 'Connects by cable or network'
  },
  { kind: 'ambient-customized', label: 'Ambient Weather station', how: 'Sends readings to us' },
  {
    kind: 'airgradient',
    label: 'AirGradient air-quality sensor',
    how: 'Connects over your network'
  },
  { kind: 'snowgauge', label: 'Snow depth sensor', how: 'Connects over your network' }
];

/**
 * §0.7's seventh row, which is a link rather than a type.
 *
 * Offered in the chooser because "another RemoteWeather" is a thing an operator has and
 * looks for here. It is configured as a pairing, not as a station, so the row navigates
 * instead of selecting.
 */
export const receivedFromPeer = {
  label: 'Another RemoteWeather',
  how: 'Receives readings from a peer',
  href: '/data-storage/receive',
  note: 'A peer link is set up under Data storage. The stations it sends appear here on their own.'
} as const;

export const stations = {
  list: {
    title: 'My weather stations',
    blurb: 'The hardware that sends readings to this machine.',
    addButton: 'Add a station',
    columns: {
      name: 'Name',
      kind: 'Kind',
      collecting: 'Collecting readings',
      status: 'Status',
      lastReading: 'Last reading'
    },
    receivedFrom: (peer: string) => `Received from ${peer}`,
    receivedNote: 'Sent to us by another RemoteWeather. Edit it where it is configured.',
    empty: {
      heading: 'No weather stations yet',
      body: 'Nothing is sending readings to this machine. Adding a station is the first step.',
      action: 'Add a station'
    }
  },
  add: {
    title: 'Add a station',
    blurb: 'Pick what you have. Only the settings that kind needs are shown.',
    chooserTitle: 'What kind of weather station do you have?',
    found: (address: string) => `We found one on your network at ${address}`,
    useThis: 'Use this',
    discoveryUnavailable:
      'Automatic discovery is not in this version. Type the address below instead.',
    noSerialPorts:
      'No serial ports found. If your console is plugged in by USB, check the cable — or choose network instead.',
    chooseNetworkInstead: 'Choose network instead',
    testFirst: 'Test connection',
    testPassed: 'The station answered.',
    testFailedPrefix: "Couldn't reach it:",
    saveAnyway: 'Save',
    saveAnywayHint:
      "A failed test doesn't stop you saving. A station that isn't plugged in yet is a perfectly good reason to save now and plug it in later.",
    testUnavailable:
      'Testing a connection before saving is not in this version. Save it and the status page will say whether readings arrive.'
  },
  settings: {
    title: 'Settings',
    sections: {
      identity: 'Name and kind',
      connection: 'How it connects',
      location: 'Where it is',
      calibration: 'Calibration',
      sharing: 'Shared with'
    },
    sharedWithNone: 'Not shared with any weather service yet.',
    deleteTitle: 'Remove this station?',
    deleteBody:
      'Readings already saved are kept. Nothing new will arrive from it, and anything that depended on it stops.',
    deleteConfirm: 'Remove the station'
  },
  status: {
    title: 'Status',
    livePanelHeading: 'Live readings',
    noReadingsYet: 'No readings yet.',
    noReadingsDiagnostic:
      'Nothing has arrived since this page opened. Check the connection, then look at the activity log.',
    checkActivity: 'Check the activity log',
    readingsArriving: 'Readings are arriving.',
    lastReading: (ago: string) => `last reading: ${ago}`,
    counters: 'Counters',
    rejected: (n: number, reason: string) => `${n} readings rejected: ${reason}`
  },
  diagnostics: {
    title: 'Diagnostics',
    blurb: 'Check that this station answers, and read what it is reporting right now.',
    readValues: 'Read current values',
    perStationLog: 'Show only this station in the activity log',
    unavailable:
      'On-demand connection checks are not in this version. The status page and the activity log are what to read instead.'
  },
  fields: {
    name: field({
      label: 'Name',
      hint: 'What you call this station. It appears on your website and in the phone app.'
    }),
    kind: field({
      label: 'Kind',
      hint: 'What hardware this is. Changing it changes which settings apply.'
    }),
    collecting: field({
      label: 'Collecting readings',
      hint: 'Turn off to stop reading from this station without removing it.',
      more: 'Turning it off is a saved setting, not a pause: it survives a restart, and the station stays configured so you can turn it back on. Readings already saved are untouched.'
    }),
    host: field({
      label: 'Address',
      hint: 'The hostname or IP address of the station on your network.'
    }),
    port: field({
      label: 'Port',
      hint: 'The TCP port it listens on. Davis consoles over a network bridge usually use 22222.'
    }),
    device: field({
      label: 'Serial port',
      hint: 'Which cable it is plugged into. Pick the one that names your adapter.',
      more: 'Port names like /dev/ttyUSB0 are handed out in the order things are plugged in, so two adapters can swap places across a reboot. Where a stable name exists it is offered first, and it is the one to choose.'
    }),
    baud: field({
      label: 'Baud rate',
      hint: 'How fast the station talks over the cable. Davis consoles use 19200.'
    }),
    pollInterval: field({
      label: 'How often to read',
      hint: 'Seconds between reads. Davis consoles answer comfortably every 2 seconds.'
    }),
    latitude: field({
      label: 'Latitude',
      hint: 'Decimal degrees, north positive. Used for sunrise, sunset and the clear-sky model.',
      more: 'This is not decoration. Sunrise and sunset on your website come from it, and so does the maximum solar radiation a clear sky could produce at your location — so a latitude that is a degree out is a wrong answer that still looks like weather.'
    }),
    longitude: field({
      label: 'Longitude',
      hint: 'Decimal degrees, east positive — so North America is negative.',
      more: 'A wrong longitude moves sunrise and sunset by four minutes per degree. It is the single most common wrong value in a hand-written configuration, because the minus sign is easy to drop.'
    }),
    altitude: field({
      label: 'Altitude',
      hint: 'Metres above sea level. Used to turn station pressure into sea-level pressure.',
      more: "It also feeds the clear-sky model through air pressure, so leaving it at a default makes the maximum-solar figure wrong by a few percent even when everything else is right."
    }),
    timezone: field({
      label: 'Time zone',
      hint: "Used to decide when 'today' starts for daily rain and snow totals."
    }),
    windDirCorrection: field({
      label: 'Wind direction correction',
      hint: "Degrees to add if your wind vane isn't pointing true north. Leave at 0 if you aligned it.",
      more: 'Nothing downstream can tell that a vane is misaligned: every direction simply reads wrong by the same amount, consistently, forever. If you know the offset, this is where it goes.'
    }),
    snowBaseDistance: field({
      label: 'Base snow distance',
      // §0.5 caps a hint at 90 characters and W5's own worked example for this field is
      // 98, so it is said in two lines instead of one over-long one -- which is what the
      // `more` disclosure is for.
      hint: 'Millimetres from your snow sensor down to bare ground.',
      more: 'Measure it when there is no snow under the sensor. Snow depth is this distance minus whatever the sensor currently reads, so a wrong base makes every depth wrong by the same amount.'
    }),
    aprsCallsign: field({
      label: 'Callsign',
      hint: 'The amateur-radio callsign this station reports under, if it has one.'
    }),
    sensorMapping: field({
      label: 'Which sensor is which',
      hint: 'Which transmitter on the WeatherLink Live carries which sensors.'
    }),
    presentedKey: field({
      label: 'Station identifier',
      hint: 'The name this station announces itself with when it sends readings to us.'
    }),
    protocolSecret: field({
      label: 'Shared secret',
      hint: 'The password the station sends with its readings, if it uses one.'
    })
  } satisfies Record<string, FieldCopy>
};
