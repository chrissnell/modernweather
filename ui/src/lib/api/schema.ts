/**
 * The committed schema, re-exported from one place.
 *
 * `generated/` is wholly owned by the generator (W3 §1.6 rule 3) and lives outside `ui/`,
 * so the import crosses the project boundary exactly once -- here -- rather than from
 * every module that needs a type name. `$mgmt` is the alias `svelte.config.js` declares.
 */
export type { paths, components } from '$mgmt';
