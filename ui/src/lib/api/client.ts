import createClient from 'openapi-fetch';
import type { paths, components } from './schema';

/**
 * The typed client of §4.2, and the only place in the UI that knows a URL.
 *
 * `openapi-fetch` is parameterised by the committed schema's path union, so
 * `api.GET('/api/v1/runtime/componnts')` is a compile error rather than a 404 at 2 a.m.
 * That is §4.3's mechanism 2 -- the one that catches "the UI is stale" -- and it is the
 * highest-value check in §6.7's list precisely because it costs nothing to run.
 *
 * `baseUrl` is empty on purpose: the bundle is served by the same binary that serves the
 * API, so every request is same-origin. `vite dev` proxies `/api` to a running daemon
 * (§6.4), which keeps the dev loop same-origin too -- and a same-origin client is what
 * makes `SameSite=Strict` on the session cookie workable at all.
 */
export type Paths = paths;
export type Schemas = components['schemas'];

/**
 * W5 §3.3's second CSRF factor.
 *
 * `SameSite=Strict` is the first and it is the strong one; this header is the second,
 * and its whole value is that a cross-site form post or an `<img>` cannot set a custom
 * header without a successful CORS preflight. Sent on **every** request including reads,
 * because a read that leaks configuration is worth the same protection as a write.
 */
export const CSRF_HEADER = 'X-MW-Request';

/** What the guard expects to find in it. Any value would do; a constant is greppable. */
export const CSRF_VALUE = '1';

/** Fired when the server says the session is gone, so the shell can show the sign-in page. */
export type Unauthenticated = () => void;

let onUnauthenticated: Unauthenticated | null = null;

/** The shell registers this once; nothing else should care. */
export function setUnauthenticatedHandler(fn: Unauthenticated | null): void {
  onUnauthenticated = fn;
}

export const api = createClient<paths>({
  baseUrl: '',
  credentials: 'same-origin',
  headers: { [CSRF_HEADER]: CSRF_VALUE }
});

api.use({
  async onResponse({ response }) {
    // A 401 anywhere means the session ended -- expired, revoked from another browser, or
    // signed out everywhere. Handled centrally rather than at 40 call sites, and handled
    // as navigation rather than as an error toast: "your session expired" is not a failure
    // of the thing the operator was doing.
    if (response.status === 401) onUnauthenticated?.();
    return response;
  }
});

/**
 * One error shape for the whole UI, built from §8.5's body.
 *
 * `fields` is what makes an error land **on the field** rather than in a banner, which is
 * §7.2's rule: the server returns `{"field": "stations[0].hostname", "message": "..."}`
 * and `Field` reads it from here by path.
 */
export interface ApiFailure {
  readonly summary: string;
  readonly detail?: string;
  readonly fields: ReadonlyMap<string, string>;
  readonly status: number;
  /** True when the request never reached the server -- the §8.8 offline case. */
  readonly offline: boolean;
}

export function failure(status: number, body: unknown): ApiFailure {
  const b = (body ?? {}) as Partial<Schemas['ErrorBody']>;
  const fields = new Map<string, string>();
  for (const e of b.errors ?? []) fields.set(e.field, e.message);
  return {
    summary: b.summary ?? 'Something went wrong.',
    detail: b.detail ?? undefined,
    fields,
    status,
    offline: false
  };
}

export const OFFLINE: ApiFailure = {
  summary: "Can't reach RemoteWeather.",
  detail: 'The page is showing what it last knew. It will catch up when the connection comes back.',
  fields: new Map(),
  status: 0,
  offline: true
};

/**
 * Turns one `openapi-fetch` result into `data` or a thrown `ApiFailure`.
 *
 * Thrown rather than returned because every call site does the same two things with a
 * failure -- show it and stop -- and a `Result` that forty call sites unwrap by hand is
 * forty chances to forget.
 *
 * **Success is decided by the status, not by the presence of a body.** Every `DELETE` on
 * this API answers `204 No Content`, and `openapi-fetch` reports that as `data:
 * undefined` -- so a check for a body treats a successful revocation as a failure, shows
 * an error, and skips the refresh that would have removed the row. That is exactly the
 * class of defect §2 exists against: a screen that says the opposite of what happened.
 */
export function unwrap<T>(result: { data?: T; error?: unknown; response?: Response }): T {
  const response = result.response;
  if (response?.ok) return result.data as T;
  const status = response?.status ?? 0;
  if (status === 0) throw OFFLINE;
  throw failure(status, result.error);
}

/** Narrows an unknown caught value to the one error shape the UI renders. */
export function asFailure(e: unknown): ApiFailure {
  if (e && typeof e === 'object' && 'summary' in e && 'fields' in e) return e as ApiFailure;
  return { ...OFFLINE, summary: String(e) };
}
