import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/**
 * A real `modernweather`, started and stopped by the test.
 *
 * Against the **binary**, not against a mock, because the four things these flows exist
 * to catch all live in the seam: the session cookie's attributes, the `EventSource`
 * reconnect, the `applied_generation` comparison across a real supervisor, and the
 * single-page fallback serving a deep link. A mock that got any of those right would be a
 * second implementation of the thing under test.
 */
export interface Daemon {
  /** The management API's base URL. */
  url: string;
  /** The one-time setup code, read from the console banner. */
  setupCode: string;
  /** Everything the daemon has written, for a diagnosis when an assertion fails. */
  output: () => string;
  /** Stop it, leaving the configuration database in place. */
  stop: () => Promise<void>;
  /** Stop and start again over the same configuration -- flow 5's restart. */
  restart: () => Promise<void>;
  /** Stop it and remove its configuration database and spool. */
  dispose: () => Promise<void>;
}

const BINARY =
  process.env.MW_E2E_BINARY ?? join(process.cwd(), '..', 'target', 'debug', 'modernweather');

const PORT = Number(process.env.MW_E2E_PORT ?? 8081);

/** The banner prints the code on its own indented line; this is that line. */
function parseSetupCode(text: string): string | null {
  for (const line of text.split('\n')) {
    const trimmed = line.trim();
    if (/^[A-Z0-9]{4}(-[A-Z0-9]{4})+$/.test(trimmed)) return trimmed;
  }
  return null;
}

async function waitForReady(url: string, text: () => string, timeoutMs = 45_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${url}/api/v1/auth/session`, {
        headers: { 'X-MW-Request': '1' }
      });
      if (response.ok) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`the daemon did not answer on ${url} in time.\n\n${text()}`);
}

export async function startDaemon(): Promise<Daemon> {
  const dir = await mkdtemp(join(tmpdir(), 'mw-e2e-'));
  const config = join(dir, 'config.db');
  let collected = '';
  let child: ChildProcess | null = null;
  const url = `http://127.0.0.1:${PORT}`;

  const launch = () => {
    const proc = spawn(
      BINARY,
      ['--config', config, '--setup-listen', '127.0.0.1', '--log-format', 'json'],
      { stdio: ['ignore', 'pipe', 'pipe'] }
    );
    // Both streams, merged: the structured log goes to one and §3.4's setup-code banner to
    // the other, and the test needs the banner.
    proc.stdout?.on('data', (b: Buffer) => (collected += b.toString()));
    proc.stderr?.on('data', (b: Buffer) => (collected += b.toString()));
    return proc;
  };

  const stop = async () => {
    if (!child) return;
    const dying = child;
    child = null;
    dying.kill('SIGTERM');
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        dying.kill('SIGKILL');
        resolve();
      }, 10_000);
      dying.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });
    });
    // The port is released on exit, but the kernel can hold it for a moment and the next
    // launch would then fail for a reason that has nothing to do with the test.
    await new Promise((r) => setTimeout(r, 300));
  };

  child = launch();
  await waitForReady(url, () => collected);

  const setupCode = parseSetupCode(collected);
  if (!setupCode) {
    throw new Error(`no setup code in the daemon's output.\n\n${collected}`);
  }

  return {
    url,
    setupCode,
    output: () => collected,
    stop,
    restart: async () => {
      await stop();
      child = launch();
      await waitForReady(url, () => collected);
    },
    dispose: async () => {
      await stop();
      await rm(dir, { recursive: true, force: true });
    }
  };
}

/**
 * Wait until the management API answers again.
 *
 * Needed after any save that adds or removes a station or a website: that restarts
 * **every** component including the one listener (M7-1's stated cost), so the socket is
 * rebound and a request made in that instant is refused. A real operator sees §8.8's
 * error page with a Retry; a test has to wait rather than race.
 */
export async function waitUntilReady(daemon: Daemon): Promise<void> {
  await waitForReady(daemon.url, daemon.output, 30_000);
}

/** The administrator these flows create. */
export const ADMIN = { username: 'owner', password: 'a-long-enough-passphrase' };

/** Claim the instance over the API, for the four flows that are not the claim itself. */
export async function claim(daemon: Daemon): Promise<void> {
  const response = await fetch(`${daemon.url}/api/v1/auth/claim`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-MW-Request': '1' },
    body: JSON.stringify({
      setup_code: daemon.setupCode.replace(/-/g, ''),
      username: ADMIN.username,
      password: ADMIN.password,
      password_again: ADMIN.password
    })
  });
  if (!response.ok) {
    throw new Error(`the claim failed: ${response.status} ${await response.text()}`);
  }
}
