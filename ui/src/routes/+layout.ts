/**
 * SPA mode, stated per-route because `adapter-static` needs it somewhere and the root
 * layout is the honest place: there is no Node runtime in production (§6.4), so nothing
 * is rendered on a server and nothing is prerendered.
 *
 * `trailingSlash: 'never'` keeps `/stations` and `/stations/` from being two cache keys
 * in front of a single-page fallback.
 */
export const ssr = false;
export const prerender = false;
export const trailingSlash = 'never';
