import { writeFile } from 'node:fs/promises';
import { test as base, expect } from '@playwright/test';
import { ADMIN, claim, startDaemon, type Daemon } from './daemon';

/**
 * A fresh daemon per test, and a signed-in page for the four flows that need one.
 *
 * Per test rather than per file: the claim is a one-time, irreversible transition, so a
 * shared instance would make flow 1 order-dependent on every other flow -- and an
 * order-dependent test suite is one that fails for the wrong reason the first time
 * somebody adds a test in the middle.
 */
export const test = base.extend<{ daemon: Daemon; signedIn: Daemon }>({
  daemon: async ({}, use, testInfo) => {
    const daemon = await startDaemon();
    try {
      await use(daemon);
    } finally {
      // The daemon's own log on failure, because "the page did not say what I expected"
      // and "the daemon refused the write" look identical from the browser's side.
      if (testInfo.status !== testInfo.expectedStatus) {
        const path = testInfo.outputPath('daemon.log');
        await writeFile(path, daemon.output(), 'utf8');
        await testInfo.attach('daemon.log', { path, contentType: 'text/plain' });
      }
      await daemon.dispose();
    }
  },

  // Browser-side failures, attached on a failing test. Without this a thrown exception
  // in a component reads as "the element was not found", which sends the reader looking
  // at the locator instead of at the stack.
  page: async ({ page }, use, testInfo) => {
    const problems: string[] = [];
    page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}\n${e.stack ?? ''}`));
    page.on('console', (m) => {
      if (m.type() === 'error') problems.push(`console: ${m.text()}`);
    });
    await use(page);
    if (testInfo.status !== testInfo.expectedStatus && problems.length > 0) {
      const path = testInfo.outputPath('browser.log');
      await writeFile(path, problems.join('\n\n'), 'utf8');
      await testInfo.attach('browser.log', { path, contentType: 'text/plain' });
    }
  },

  signedIn: async ({ daemon, page }, use) => {
    await claim(daemon);
    await page.goto('/sign-in');
    await page.getByLabel('Username').fill(ADMIN.username);
    await page.getByLabel('Password', { exact: true }).fill(ADMIN.password);
    await page.getByRole('button', { name: 'Sign in' }).click();
    await expect(page.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();
    await use(daemon);
  }
});

export { expect, ADMIN };
