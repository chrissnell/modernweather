import { field, type FieldCopy } from './types';

export const system = {
  index: {
    title: 'System',
    blurb: 'This machine, what it is doing, and who can sign in.',
    sections: [
      { href: '/system/activity', label: 'Activity', summary: 'What has been happening.' },
      {
        href: '/system/diagnostics',
        label: 'Diagnostics',
        summary: 'Checks you can run, and everything that is running.'
      },
      { href: '/system/machine', label: 'This machine', summary: 'Version, uptime and hardware.' },
      {
        href: '/system/administrator',
        label: 'Administrator',
        summary: 'Your password and where you are signed in.'
      },
      {
        href: '/system/advanced',
        label: 'Advanced',
        summary: 'Admin address, raw configuration, extra detail in the log.'
      }
    ]
  },
  activity: {
    title: 'Activity',
    blurb: 'Everything this machine has logged, newest last.',
    columns: { ts: 'Time', level: 'Level', target: 'Where', message: 'What happened' },
    filters: {
      level: 'Least important to show',
      stream: 'Which log',
      station: 'Station',
      search: 'Search'
    },
    streams: { app: 'The system', access: 'Web requests', both: 'Both' },
    levels: {
      error: 'Errors only',
      warn: 'Warnings and errors',
      info: 'Normal',
      debug: 'Detailed',
      trace: 'Everything'
    },
    paused: 'Paused',
    autoscroll: 'Follow',
    download: 'Download',
    empty: { heading: 'Nothing logged yet', body: 'Entries appear here as they happen.' },
    caughtUp: 'live',
    gap: (n: number) => `${n} entries dropped (the log filled up faster than it could be shown)`,
    gapRepeated:
      'The log is producing entries faster than this view can keep up. Narrowing the filter will help.',
    bufferCap: (n: number) =>
      `Showing the newest ${n} entries. Older entries are available in the full log.`,
    fullLog: 'Open the full log',
    moreDetail: {
      heading: 'Show more detail',
      forLabel: 'For',
      detailLabel: 'Detail',
      durationLabel: 'For how long',
      turnOn: 'Turn on',
      durations: {
        '5m': '5 minutes',
        '10m': '10 minutes',
        '30m': '30 minutes',
        '1h': '1 hour',
        forever: 'Until I turn it off'
      },
      standing: (subject: string) => `Showing extra detail for ${subject}.`,
      stop: 'Stop',
      captureNote:
        'This changes what gets recorded, not just what is shown. Turning it up fills the log faster; turning it down can lose detail you are hunting for.',
      viewNote: 'The filters above only change what this page shows. Nothing is lost.'
    }
  },
  diagnostics: {
    title: 'Diagnostics',
    blurb: 'Checks you can run on demand, and everything that is running right now.',
    checksHeading: 'Checks',
    checksUnavailable:
      'On-demand checks are not in this version. Everything below is live, and the activity log has the detail.',
    runningHeading: 'Everything that is running',
    columns: { what: 'What', status: 'Status', schedule: 'Schedule', lastRun: 'Last run' },
    runNow: 'Run now',
    restart: 'Restart',
    restartTitle: 'Restart this?',
    restartBody: 'It stops and starts again with the configuration that is saved now.',
    noStopNote:
      'There is no stop button here on purpose. Turning something off is a setting on its own page, so it survives a restart and is visible where it belongs.'
  },
  machine: {
    title: 'This machine',
    blurb: 'What this copy of RemoteWeather is and where it is running.',
    rows: {
      version: 'Version',
      target: 'Built for',
      hostname: 'Host name',
      os: 'Operating system',
      started: 'Started',
      uptime: 'Running for',
      pid: 'Process id',
      serialPorts: 'Serial ports present',
      listening: 'Listening on'
    },
    noSerialPorts: 'None found.',
    unavailable: 'Memory and disk figures are not in this version.'
  },
  administrator: {
    title: 'Administrator',
    blurb: 'Your sign-in, and every browser that is currently signed in.',
    changePassword: 'Change password',
    passwordChanged:
      'Password changed. Every other browser has been signed out; this one is still signed in.',
    sessionsHeading: 'Where you are signed in',
    sessionColumns: { created: 'Signed in', lastSeen: 'Last used', from: 'From', agent: 'Browser' },
    thisSession: 'This browser',
    revoke: 'Sign out',
    signOutEverywhere: 'Sign out everywhere',
    signOutEverywhereTitle: 'Sign out of every browser?',
    signOutEverywhereBody: 'Every browser including this one has to sign in again.',
    recoveryNote:
      'Locked out? Run `modernweather reset-password` on this machine. It needs shell access, which is the point.',
    noTokensNote:
      'There are no API tokens. The sign-in cookie is not a credential anything else can use, which is the defect this replaces.'
  },
  advanced: {
    title: 'Advanced',
    blurb: 'For when you know what you are looking for.',
    sections: {
      admin: 'Admin address',
      cors: 'Allowed origins',
      logLevels: 'Standing detail overrides',
      raw: 'Raw configuration',
      ids: 'Internal names',
      metrics: 'Metrics'
    },
    portChangeWarning: (url: string) =>
      `Changing the address moves this screen. After saving, this page is at ${url} and this browser will not follow automatically.`,
    portChangeConfirm: 'Change the address',
    corsNote:
      'An added origin takes effect at the next start, not immediately. The list is read once when the process starts.',
    corsEmpty: 'Empty, which means no other site may call this API. That is the right default.',
    rawNote: 'Secrets are left out. What is shown is what you could re-type.',
    idsNote:
      'These are the names used inside the system and in the log. They are shown here and nowhere else.',
    metricsNote: 'Numbers for a monitoring system, in the Prometheus text format.',
    openMetrics: 'Open the metrics page'
  },
  fields: {
    currentPassword: field({
      label: 'Current password',
      hint: 'The password you are signing in with now.'
    }),
    newPassword: field({
      label: 'New password',
      hint: 'At least 12 characters. A short phrase you can type beats a short jumble you cannot.',
      more: 'Length is what matters. Very common passwords of any length are refused, because a list of the ten thousand most used ones is checked before the password is accepted.'
    }),
    newPasswordAgain: field({
      label: 'New password again',
      hint: 'Type it a second time, so a typo does not lock you out.'
    }),
    sessionLifetime: field({
      label: 'Stay signed in for',
      hint: 'How long a browser stays signed in without being used.'
    }),
    managementListenAddr: field({
      label: 'Admin address',
      hint: 'Which address this admin screen listens on. 127.0.0.1 means this machine only.'
    }),
    managementPort: field({
      label: 'Admin port',
      hint: 'Which port this admin screen listens on.'
    }),
    managementTlsCert: field({
      label: 'Admin certificate file',
      hint: 'Full path to a PEM certificate to serve this admin screen over https.'
    }),
    managementTlsKey: field({
      label: 'Admin private key file',
      hint: 'Full path to the matching PEM private key.'
    }),
    httpPort: field({ label: 'Website port', hint: 'The port your public website is served on.' }),
    httpsPort: field({
      label: 'Website secure port',
      hint: 'The port your public website is served on over https.'
    }),
    listenAddr: field({
      label: 'Website address',
      hint: 'Which address the public website listens on. 0.0.0.0 means every network.'
    }),
    grpcPort: field({
      // W5 §0.5 offers this exact sentence as the worked example of a hint written for
      // someone who has just bought a weather station, and it is 122 characters against
      // its own 90-character cap. Both halves are kept: the first line is the hint and
      // the advice is the disclosure, which is the shape §0.5 specifies anyway.
      label: 'App & integration port',
      hint: 'The port the RemoteWeather phone app connects to.',
      more: 'Leave it alone unless something else on this machine already uses 50051. Peers that send readings here use this port too, so changing it means changing it on them as well.'
    }),
    corsOrigin: field({
      label: 'Allowed origin',
      hint: 'A full address, such as https://admin.example.com. One per line.'
    }),
    logDirective: field({
      label: 'Detail override',
      hint: 'Which parts of the system to record extra detail for, in the usual filter syntax.'
    })
  } satisfies Record<string, FieldCopy>
};
