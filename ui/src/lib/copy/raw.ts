/**
 * The vocabulary rule's escape hatch (§6.6), and the only one.
 *
 * D-36 says no internal term reaches the screen. One screen has to break that rule on
 * purpose: `System -> Advanced` shows the raw configuration and the internal component
 * ids, because the person who has navigated to it is debugging and the id is the thing
 * they need to put in a support message. Writing those terms as `raw('ReadingStore')`
 * makes each one a greppable, reviewable line rather than a hole in the rule -- `cargo
 * xtask gate-ui-copy` prints the current exception list on every run, so the list cannot
 * grow quietly.
 *
 * Permitted only in `src/lib/copy/advanced.ts`. The gate enforces that, not a convention.
 */
export function raw(term: string): string {
  return term;
}
