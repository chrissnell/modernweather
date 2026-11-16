/**
 * The rendering vocabulary for status, and nothing the server already renders.
 *
 * §2.3 is explicit that **the server renders the vocabulary, not the client**: a
 * component arrives with `badge.label`, `badge.tone`, `sentence` and `notes` already in
 * the operator's words, so that a new component is nameable without a UI change and so
 * that §6.6's lint can see the mapping in one place. What is left for the client is the
 * part that is genuinely presentational -- which colour a tone is, whether a badge gets a
 * spinner -- plus the two things only a browser can do: relative time, and the
 * `applied_generation` comparison.
 */

/** chonky's `Badge` variants. §6.3 gap G3: there is no `pending`, so `info` carries it. */
export type BadgeVariant = 'default' | 'success' | 'warning' | 'danger' | 'info';

/**
 * §0.4's tone, as a chonky variant.
 *
 * `neutral` is chonky's `default`. Everything else is a straight name match, which is
 * deliberate: the server owns which state gets which tone, and a client-side remapping
 * would be a second opinion about the same question.
 */
export function variantFor(tone: string): BadgeVariant {
  switch (tone) {
    case 'success':
      return 'success';
    case 'warning':
      return 'warning';
    case 'danger':
      return 'danger';
    case 'info':
      return 'info';
    default:
      return 'default';
  }
}

/**
 * Whether a badge gets a spinner beside it.
 *
 * §6.3 gap G3: `Starting` and `Stopping` have no honest colour in chonky's five variants
 * -- `info` reads as a notice rather than as work in progress -- so they are `info` plus a
 * spinner until a `pending` variant exists upstream.
 */
export function spins(state: string): boolean {
  return state === 'starting' || state === 'stopping';
}

/** Whether a state means "a person should look at this". */
export function needsAttention(state: string): boolean {
  return state === 'failed' || state === 'backoff';
}

export const statusCopy = {
  /** The live-connection indicator, §5.2. */
  stream: {
    live: 'live',
    reconnecting: (seconds: number) => `reconnecting (${seconds}s)`,
    disconnected: 'disconnected',
    retry: 'Retry',
    staleNote: (when: string) => `last updated ${when}`,
    restarted: 'RemoteWeather restarted. Reconnected.'
  },
  /** §0.4's freshness sentences, which are rendered apart from state on purpose. */
  freshness: {
    never: (subject: string) => `${subject} has never been fetched.`,
    ago: (subject: string, human: string) => `${subject} last updated ${human} ago.`
  },
  /** §0.4's staleness sentence -- an available-but-stale thing is not a failure. */
  staleness: (human: string, missing: string[]) =>
    `Some values are being held from ${human} ago: ${missing.join(', ')}.`,
  /** §2.2's derived fact, said in words where a page has to say it. */
  pending: 'Saved, and still catching up.',
  live: 'Live.',
  blockedAction: (href: string, what: string) => ({ href, label: `Go to ${what}` })
};
