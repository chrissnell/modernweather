import { field, type FieldCopy } from './types';

/**
 * §0.2: the four destinations, named by what they are to the operator. "APRS / CWOP"
 * keeps its own words on purpose (§0.3) -- a person configuring it has a callsign and a
 * passcode, and "share with amateur radio" as a service name would be worse.
 */
export interface DestinationCopy {
  /** The `service` value the API uses. */
  readonly service: string;
  readonly label: string;
  readonly href: string;
  readonly summary: string;
}

export const destinations: readonly DestinationCopy[] = [
  {
    service: 'wunderground',
    label: 'Weather Underground',
    href: '/share/weather-underground',
    summary: 'Send your readings to wunderground.com.'
  },
  {
    service: 'pwsweather',
    label: 'PWS Weather',
    href: '/share/pws-weather',
    summary: 'Send your readings to pwsweather.com.'
  },
  {
    service: 'aprs',
    label: 'APRS / CWOP',
    href: '/share/aprs',
    summary: 'Beacon your readings over APRS-IS or a radio.'
  }
];

export const share = {
  index: {
    title: 'Share my data',
    blurb: 'Where your readings go, besides your own website.',
    columns: {
      destination: 'Where',
      station: 'Which station',
      enabled: 'On',
      status: 'Status',
      lastSent: 'Last sent'
    },
    empty: {
      heading: 'Not sharing with anyone yet',
      body: 'Pick a service below to start sending your readings to it.'
    },
    forecastsLabel: 'Forecasts & alerts',
    forecastsHref: '/share/forecasts',
    forecastsSummary: 'Fetch a forecast for your website.'
  },
  upload: {
    globalSection: 'Account',
    perStationSection: 'Which stations to send',
    statusSection: 'How it is going',
    lastUpload: 'Last upload',
    lastError: 'Last error',
    noStations: 'No stations are being sent to this service.',
    rejectedNote:
      "A rejected upload is not a bad setting here — the service may be down, or the station ID may not be registered with them yet. The status above says what they answered."
  },
  forecasts: {
    title: 'Forecasts & alerts',
    blurb: 'A forecast for your website, fetched from a weather data provider.',
    vendorNote: 'Supplied by Aeris Weather. An account with them is needed.',
    lastForecast: 'Forecast',
    lastAlert: 'Alerts',
    credentialTestNote:
      'Saving is not a credentials check. If the account details are wrong the fetch fails and says so here, which is what you want when the provider is simply down.'
  },
  fields: {
    enabled: field({
      label: 'Send my readings',
      hint: 'Turn off to stop sending without losing the settings.'
    }),
    stationId: field({
      label: 'Station ID',
      hint: 'The ID the weather service issued for this station.'
    }),
    apiKey: field({
      label: 'Password',
      hint: 'The upload password or API key the weather service issued.'
    }),
    apiEndpoint: field({
      label: 'Upload address',
      hint: "Leave empty unless the service has told you to send somewhere else."
    }),
    interval: field({
      label: 'Upload interval',
      hint: 'How often to send a reading. Weather Underground asks for no more than one every 60 seconds.'
    }),
    callsign: field({
      label: 'Callsign',
      hint: 'Your amateur-radio or CWOP callsign, as issued.'
    }),
    passcode: field({
      label: 'Passcode',
      hint: "The numeric passcode for your callsign. If you don't have one, your station can still receive but not transmit."
    }),
    symbolTable: field({
      label: 'Symbol table',
      hint: "Which APRS symbol set to use. '/' is the primary table."
    }),
    symbolCode: field({
      label: 'Symbol',
      hint: "The APRS symbol shown on the map. '_' is a weather station."
    }),
    comment: field({
      label: 'Comment',
      hint: 'A short line sent with every beacon. Your website address is a good use for it.'
    }),
    aprsTransport: field({
      label: 'Send over',
      hint: 'APRS-IS sends over the internet. A TNC sends over the radio.'
    }),
    aprsServer: field({
      label: 'APRS-IS server',
      hint: 'The APRS-IS server to connect to. Leave as it is unless you have a reason.'
    }),
    kissSerialDevice: field({
      label: 'TNC serial port',
      hint: 'Which cable the TNC is plugged into.'
    }),
    kissSerialBaud: field({
      label: 'Baud rate',
      hint: 'How fast the TNC talks over the cable. Most KISS TNCs use 9600.'
    }),
    kissTcpAddress: field({
      label: 'TNC network address',
      hint: 'host:port of a TNC that is reached over the network rather than a cable.'
    }),
    kissPath: field({
      label: 'Digipeater path',
      hint: 'Which digipeaters to route through. WIDE1-1 is the usual answer.'
    }),
    kissDestination: field({
      label: 'Destination',
      hint: 'The AX.25 destination callsign for the beacon. APRS is the usual answer.'
    }),
    clientId: field({
      label: 'Client ID',
      hint: 'The client ID from your forecast provider account.'
    }),
    clientSecret: field({
      label: 'Client secret',
      hint: 'The client secret from your forecast provider account.'
    }),
    refreshInterval: field({
      label: 'Refresh interval',
      hint: 'How often to fetch a new forecast. Hourly is plenty.'
    })
  } satisfies Record<string, FieldCopy>
};
