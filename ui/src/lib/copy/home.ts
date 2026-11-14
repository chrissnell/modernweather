export const home = {
  title: 'Home',
  blurb: 'What your weather station is doing right now.',
  attention: {
    heading: 'Something needs attention',
    because: (n: number) =>
      n === 1 ? 'Because of this, 1 thing is waiting:' : `Because of this, ${n} things are waiting:`,
    details: 'Details'
  },
  allWell: {
    heading: 'Everything is running',
    body: 'Nothing needs attention.'
  },
  counts: {
    running: 'Running',
    waiting: 'Waiting',
    failed: 'Stopped',
    backoff: 'Retrying',
    // **Not "Turned off".** The server's `off` bucket is "stopped, finished, or not
    // started", and §0.4 gives those different words on purpose: a `Finished` one-shot is
    // badged **Done** and only a `Stopped` one is **Off**, "because the only way an
    // operator reaches `Stopped` deliberately is by turning something off". Labelling the
    // tile "Turned off" therefore reported a successful start-up warm-up as something the
    // operator had switched off -- which is the §2 lie in miniature, on the one screen
    // that exists to answer "is this working?".
    off: 'Not running'
  },
  stations: {
    heading: 'Your stations',
    noneYet: 'No stations yet.',
    addOne: 'Add a station',
    ago: (human: string) => `${human} ago`,
    noReading: 'no readings yet'
  },
  /** §2.4's five banner states, in order. */
  apply: {
    savedCatchingUp: (n: number) =>
      n === 1 ? 'Saved. 1 item is restarting…' : `Saved. ${n} items are restarting…`,
    savedAndLive: 'Saved and live.',
    savedNotYet: 'Saved, but not running yet.',
    savedNotYetSuffix: 'Your change will take effect automatically.',
    whatsWrong: "What's wrong?",
    savedButFailed: "Saved, but it won't start:",
    undo: 'Undo save',
    undone: 'The previous settings are back.',
    undoFailed: "Couldn't go back. The previous settings are no longer available."
  },
  /** §2.4 ①: the preview, on the Save button. */
  preview: {
    nothingElse: 'Nothing else is affected.',
    willRestart: (names: string[]) => `Saving will restart ${list(names)}.`,
    willStart: (names: string[]) => `Saving will start ${list(names)}.`,
    willStop: (names: string[]) => `Saving will stop ${list(names)}.`,
    everything:
      'Adding or removing a station or a website restarts everything, because they all have to be rebuilt together.',
    unchanged: (n: number) => `${n} other things are left alone.`,
    nothingToDo: 'Nothing changes.',
    checking: 'Working out what this affects…',
    invalid: 'That cannot be saved yet:'
  }
};

/** "a", "a and b", "a, b and c" -- the §0.4 register, not a comma-joined array. */
export function list(items: string[]): string {
  if (items.length === 0) return 'nothing';
  if (items.length === 1) return items[0];
  return `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}`;
}
