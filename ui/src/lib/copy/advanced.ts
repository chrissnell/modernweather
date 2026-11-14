/**
 * The one module where §6.6's escape hatch is permitted, and `cargo xtask gate-ui-copy`
 * enforces that rather than trusting it.
 *
 * `System -> Advanced` shows internal component ids and the raw configuration, because
 * the person who has navigated there is debugging and the internal name is the thing they
 * need to put in a support message. Every such term is written `raw('...')` so the
 * exception list is a greppable set of lines that the gate prints on every run.
 */
import { raw } from './raw';

export const advancedTerms = {
  /** The column heading over the internal ids on `System -> Advanced`. */
  internalId: `Internal name`,
  /** What the ids are, said once, using the words the log uses. */
  internalIdNote: `These are the names the log uses. A ${raw('component')} id looks like ${raw('wu:backyard')}: the kind, then which station or website it belongs to.`,
  /** The capability names, shown only where a dependency has to be named exactly. */
  capabilityNote: `A dependency is named by its ${raw('Capability')} here — for example ${raw('ReadingStore')} for the weather database or ${raw('LatestSnapshot')} for the live-readings cache.`,
  /** The lifecycle, in its own words, for someone correlating with the log. */
  stateNote: `The states in the log are ${raw('Planned')}, ${raw('Waiting')}, ${raw('Starting')}, ${raw('Running')}, ${raw('Failed')}, ${raw('Backoff')}, ${raw('Finished')}, ${raw('Stopping')} and ${raw('Stopped')}. The badges elsewhere in this application are plainer words for the same nine.`,
  /** The scheduling vocabulary, likewise. */
  triggerNote: `A schedule in the log is a ${raw('Trigger')}: ${raw('Every')}, ${raw('Calendar')}, ${raw('OnReading')}, ${raw('OnStartup')}, ${raw('OnDemand')} or ${raw('Service')}.`,
  /** The spool, named, because the metric and the log line both use the word. */
  spoolNote: `Unsent readings are held in what the log calls the ${raw('spool')}, and its size is reported as ${raw('spool_depth_bytes')}.`
};

export { raw };
