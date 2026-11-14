import { field, type FieldCopy } from './types';

export const storage = {
  index: {
    title: 'Data storage',
    blurb: 'Where your readings are kept, and who else gets them.',
    database: 'Weather database',
    databaseSummary: 'Where your readings are kept.',
    send: 'Send my readings to another RemoteWeather',
    sendSummary: 'Forward every reading to a peer that keeps them too.',
    receive: 'Receive readings from another RemoteWeather',
    receiveSummary: 'Accept readings from peers and treat them as stations.'
  },
  database: {
    title: 'Weather database',
    blurb: 'Your readings are written here, and your website reads history from it.',
    sections: { connection: 'Connection', state: 'How it is going' },
    oldestReading: 'Oldest reading',
    size: 'Size on disk',
    summaryHistory: 'Summary history',
    summaryHistoryNote:
      'Hourly and daily summaries are built from your readings so a year of history draws quickly.',
    noDatabase: {
      heading: 'No weather database yet',
      body: 'Live readings still work and your phone app still works. A database is what gives you history.'
    },
    unreachable: {
      heading: "Can't reach the weather database",
      body: 'Readings are being held and will be written when it comes back. Your website and the phone app are unaffected.',
      held: (n: number) => `${n} readings waiting to be written.`
    },
    connectivityNote:
      'Saving a connection the database refuses is still a legitimate save — it may be down, or about to be set up. The save succeeds and the status above says what happened.',
    credentialsNeeded:
      'The user and password are needed to change any of this. RemoteWeather never sends a stored password back to this page, so there is nothing a blank box could keep.',
    unavailable:
      'Database size and oldest reading are not in this version. The status above is what the running system reports.'
  },
  send: {
    title: 'Send my readings to another RemoteWeather',
    blurb: 'Every reading is forwarded to a peer, which keeps its own copy.',
    sections: { connection: 'Where to send', state: 'How it is going' },
    unsent: 'Unsent readings',
    unsentWaiting: (n: number, size: string) => `${n} readings waiting to be sent (${size}).`,
    unsentNone: 'Nothing is waiting. Every reading has been delivered.',
    lastDelivery: 'Last successful delivery',
    /** N-11: W7's `spool_time_remaining_seconds`, in the §0.4 register. */
    timeRemaining: (human: string) =>
      `At the current rate there is room for about ${human} more before the oldest unsent readings are given up on.`,
    timeRemainingUnknown:
      'There is not enough recent traffic to say how long the waiting readings will fit for.',
    /** N-11: W7 §3.5's `ceiling < max_lag x rate` advisory. */
    ceilingAdvisory: (ceiling: string, lag: string, needed: string) =>
      `Unsent readings are allowed to use ${ceiling} and are given up on after ${lag}. At the current rate that needs ${needed}, so a peer that goes away for the full period will lose the oldest readings before the time limit does.`,
    ceilingAdvisoryOk: (ceiling: string, lag: string) =>
      `Unsent readings are allowed to use ${ceiling}, which at the current rate covers the full ${lag} a peer may be away for.`,
    /** N-11: labelled peer-reported wherever it appears (W7 D-40). */
    peerReported: 'Peer-reported',
    peerReportedDepth: (n: number) => `${n} readings, as reported by the peer itself`,
    peerReportedNote:
      'This number comes from the peer, not from this machine. If the two disagree, the one above is what this machine knows it still holds.',
    peerReportedNone: 'The peer has not reported a figure.'
  },
  receive: {
    title: 'Receive readings from another RemoteWeather',
    blurb: 'Peers send readings here, and they appear under My weather stations.',
    sections: { listener: 'How peers reach us', peers: 'Peers', enrolment: 'Enrolment' },
    peerColumns: { name: 'Peer', stations: 'Stations', lastSeen: 'Last seen', depth: 'Unsent' },
    noPeers: {
      heading: 'No peers yet',
      body: 'Nothing is sending readings to this machine over a peer link.'
    },
    /** N-11: outstanding enrolment tokens -- leaving enrolment open is the mistake. */
    enrolmentOpen: (n: number) =>
      n === 1
        ? 'One enrolment code is still outstanding. Anyone holding it can enrol a new peer.'
        : `${n} enrolment codes are still outstanding. Anyone holding one can enrol a new peer.`,
    enrolmentNone: 'No enrolment codes are outstanding. Nothing new can enrol right now.',
    enrolmentColumns: { code: 'Code', issued: 'Issued', expires: 'Expires', uses: 'Uses left' },
    enrolmentLog: 'Enrolment history',
    enrolmentLogColumns: { when: 'When', peer: 'Peer', outcome: 'Outcome', source: 'From' },
    enrolmentLogEmpty: 'Nothing has enrolled yet.',
    revoke: 'Revoke',
    revokeTitle: 'Revoke this enrolment code?',
    revokeBody: 'Anyone still holding it will be refused. Peers that already enrolled are unaffected.',
    unavailable:
      'Issuing and revoking enrolment codes from this screen is not in this version. What is outstanding is shown above.'
  },
  fields: {
    host: field({ label: 'Host', hint: 'The machine the database runs on.' }),
    port: field({ label: 'Port', hint: 'The port it listens on. PostgreSQL uses 5432.' }),
    database: field({ label: 'Database', hint: 'The name of the database to write into.' }),
    user: field({ label: 'User', hint: 'The database user to connect as.' }),
    password: field({ label: 'Password', hint: "That user's password." }),
    sslMode: field({
      // §0.5's 90-character cap again: W5's worked example for this field is 105, so the
      // second sentence moves into the disclosure rather than being truncated.
      label: 'SSL mode',
      hint: 'How to encrypt the connection to the database.',
      more: "Use 'require' unless the database is on this same machine, in which case 'disable' is honest and faster. 'verify-full' additionally checks the database's certificate, which needs a certificate authority both ends agree on."
    }),
    timezone: field({
      label: 'Time zone',
      hint: 'The time zone the database reports timestamps in. UTC is the safe answer.'
    }),
    url: field({
      label: 'Connection',
      hint: 'The whole connection string, if you would rather paste one.'
    }),
    peerAddress: field({
      label: 'Peer address',
      hint: 'host:port of the other RemoteWeather that should receive your readings.'
    }),
    peerTls: field({
      label: 'Encrypt the link',
      hint: 'Leave on unless the peer is on the same machine.'
    }),
    listenPort: field({
      label: 'Listening port',
      hint: 'The port peers connect to. The phone app uses this port too.'
    }),
    spoolCeiling: field({
      label: 'Room for unsent readings',
      hint: 'How much disk unsent readings may use before the oldest are given up on.'
    }),
    spoolMaxLag: field({
      label: 'Give up after',
      hint: 'How long a peer may be away before the readings waiting for it are abandoned.'
    })
  } satisfies Record<string, FieldCopy>
};
