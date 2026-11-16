import { api, asFailure, setUnauthenticatedHandler, unwrap, type ApiFailure } from '../api/client';
import type { Schemas } from '../api/client';

/**
 * Who is signed in, and whether this machine has been claimed yet.
 *
 * One read of `GET /api/v1/auth/session` answers both, which is what lets the shell route
 * an unclaimed instance to `/setup` and an unauthenticated one to `/sign-in` without a
 * flash of the application behind them (§8.8).
 */
class Session {
  state = $state<Schemas['AuthStateView'] | null>(null);
  loading = $state(true);
  error = $state<ApiFailure | null>(null);

  get claimed(): boolean {
    return this.state?.claimed ?? false;
  }

  get authenticated(): boolean {
    return this.state?.authenticated ?? false;
  }

  get username(): string | null {
    return this.state?.username ?? null;
  }

  get policy(): Schemas['PasswordPolicyView'] | null {
    return this.state?.password_policy ?? null;
  }

  async load(): Promise<void> {
    this.loading = true;
    try {
      this.state = unwrap(await api.GET('/api/v1/auth/session'));
      this.error = null;
    } catch (e) {
      const f = asFailure(e);
      // A 401 here is not an error: it is the answer. The route guard reads `claimed`.
      if (f.status === 401) this.error = null;
      else this.error = f;
    } finally {
      this.loading = false;
    }
  }

  /** Called by the client's 401 interceptor, from anywhere. */
  expire(): void {
    if (this.state) this.state = { ...this.state, authenticated: false, username: null };
  }

  async signOut(): Promise<void> {
    await api.DELETE('/api/v1/auth/session');
    await this.load();
  }
}

export const session = new Session();

setUnauthenticatedHandler(() => session.expire());
