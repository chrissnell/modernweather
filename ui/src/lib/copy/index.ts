/**
 * Every user-facing string in the management UI, in one place.
 *
 * §6.6: a rule that is not checked is a preference. The strings live here rather than
 * inline in components for two reasons that are both mechanical — the whole vocabulary
 * can be reviewed as a unit, and `cargo xtask gate-ui-copy` can check it against §0.1's
 * banned-word list. The gate is armed by **this file's existence** (`ci/gates.toml`), so
 * deleting the module would not quietly disarm the check: it would fail the build.
 *
 * The one escape hatch is `raw('...')` in `./advanced.ts`, and only there — the one
 * screen whose audience is debugging and needs the internal name verbatim. The gate
 * prints the current exception list on every run, so it cannot grow quietly.
 *
 * A term from §0.2's left column appearing anywhere else in this directory, or in any
 * `.svelte` template, is a build failure rather than a review comment.
 */
export { field, type FieldCopy, type PageCopy } from './types';
export { raw } from './raw';
export { nav, appName, common, pages, type NavItem } from './nav';
export { home, list } from './home';
export { auth } from './auth';
export { stations, stationKinds, receivedFromPeer, type StationKindCopy } from './stations';
export { share, destinations, type DestinationCopy } from './share';
export { website } from './website';
export { storage } from './storage';
export { system } from './system';
export { advancedTerms } from './advanced';
export { statusCopy, variantFor, spins, needsAttention, type BadgeVariant } from './status';
