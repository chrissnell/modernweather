import { defineConfig, devices } from '@playwright/test';

/**
 * §6.7: **five flows only**, run against a real `modernweather` binary with a seeded
 * configuration database.
 *
 * Five because these are the flows where an integration bug is invisible to every other
 * check -- `svelte-check` cannot see a cookie, a unit test cannot see an `EventSource`
 * reconnect, and the schema gate cannot see a save that reports success without applying
 * -- and because a large browser-test suite for a single-operator admin tool is a
 * maintenance cost the owner would carry alone.
 *
 * **One worker, no parallelism, and a fresh daemon per test.** Each flow starts the real
 * binary on the ports its configuration names, so two at once would fight over them; and
 * a flow that depends on another flow's leftover state is a flow that fails for the wrong
 * reason six months from now.
 */
export default defineConfig({
  testDir: 'tests/e2e',
  fullyParallel: false,
  workers: 1,
  forbidOnly: true,
  retries: 0,
  // Generous because the first test in a run starts a debug-profile binary and waits for
  // it to open a spool; the assertions themselves have their own, tighter, timeouts.
  timeout: 90_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : [['list']],
  use: {
    ...devices['Desktop Chrome'],
    baseURL: process.env.MW_E2E_URL ?? 'http://127.0.0.1:8081',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure'
  }
});
