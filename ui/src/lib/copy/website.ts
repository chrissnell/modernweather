import { field, type FieldCopy } from './types';

export const website = {
  index: {
    title: 'My website',
    blurb: 'The public weather site this machine serves.',
    addButton: 'Add a website',
    columns: {
      name: 'Name',
      hostname: 'Address',
      station: 'Which station',
      status: 'Status',
      certificate: 'Certificate'
    },
    empty: {
      heading: 'No website yet',
      body: 'Your readings are being collected. Adding a website is how other people see them.'
    }
  },
  settings: {
    title: 'Settings',
    sections: {
      identity: 'Name and address',
      content: 'What it says',
      features: 'What it shows',
      app: 'Phone app'
    },
    deleteTitle: 'Remove this website?',
    deleteBody: 'The site stops being served. Your readings are untouched.',
    deleteConfirm: 'Remove the website'
  },
  extras: {
    title: 'Extras',
    radarHeading: 'Weather radar map',
    radarBlurb: 'A radar map on your site, supplied by a third party that needs to know it is you.',
    registeredOn: (when: string) => `Registered on ${when}`,
    register: 'Register',
    reregister: 'Register again',
    turnOff: 'Turn off',
    notRegistered: 'Not registered. The radar panel is hidden until it is.',
    registerFailed: 'Registration was refused. The reason is below; nothing was changed.'
  },
  certificate: {
    title: 'Security certificate',
    blurb: 'The certificate that serves your site over https.',
    sniNote:
      'One certificate per address. The right one is chosen from the address the visitor asked for, so several sites can share one port.',
    expiresIn: (days: number) =>
      days < 0
        ? `Expired ${Math.abs(days)} days ago`
        : days === 0
          ? 'Expires today'
          : `Expires in ${days} days`,
    saveWarningNote:
      'A certificate problem is a warning on save, never a refusal. You may be replacing a file that is not there yet.',
    noCertificate: 'No certificate set. The site is served over http only.',
    unavailable:
      'Reading the certificate to show its subject and expiry is not in this version. The paths are saved and used.'
  },
  fields: {
    name: field({
      label: 'Name',
      hint: 'What you call this site here. Visitors never see it.'
    }),
    hostname: field({
      label: 'Address',
      hint: 'The hostname visitors type, without https://. For example weather.example.com.'
    }),
    pageTitle: field({
      label: 'Page title',
      hint: "The heading at the top of the site, and what a browser tab shows."
    }),
    aboutHtml: field({
      label: 'About this station',
      hint: 'A paragraph about your station, shown on the page. HTML is allowed.'
    }),
    primaryStation: field({
      label: 'Which station feeds it',
      hint: 'Whose readings this site shows.'
    }),
    snowEnabled: field({
      label: 'Show snow depth',
      hint: 'Adds a snow panel, if you have a snow sensor.'
    }),
    snowStation: field({
      label: 'Snow sensor',
      hint: 'Which station carries the snow depth sensor.'
    }),
    airQualityEnabled: field({
      label: 'Show air quality',
      hint: 'Adds an air-quality panel, if you have an air-quality sensor.'
    }),
    airQualityStation: field({
      label: 'Air-quality sensor',
      hint: 'Which station carries the air-quality sensor.'
    }),
    appleAppId: field({
      label: 'Apple app ID',
      hint: 'Shows an install banner for the phone app on iPhones. Leave empty for none.'
    }),
    isPortal: field({
      label: 'Portal page',
      hint: 'Lists every station instead of showing one. For a site that fronts several.'
    }),
    isDefaultTls: field({
      label: 'Use for unmatched addresses',
      hint: 'Serve this site when a visitor arrives on an address no other site claims.'
    }),
    tlsCertPath: field({
      label: 'Certificate file',
      hint: 'Full path to the certificate chain file on this machine, in PEM form.'
    }),
    tlsKeyPath: field({
      label: 'Private key file',
      hint: 'Full path to the private key file on this machine, in PEM form.'
    })
  } satisfies Record<string, FieldCopy>
};
