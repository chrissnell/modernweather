/**
 * §3.5's headers, on the document rather than only on the API.
 *
 * `Router::layer` wraps the fallback as it exists at that moment, and the asset fallback
 * is registered after the guard's layer -- so these headers were present on JSON and
 * absent on `index.html`, the sign-in page and every chunk, which are the responses they
 * were written for. That is invisible from the Rust side, hence a browser.
 *
 * The second half is the trap the fix walked into: `app.html` carries an inline script
 * (the theme pin, which has to run before first paint) and Svelte emits inline `style`
 * attributes. A policy strict enough to be worth having is one keystroke away from
 * blocking the application it protects, so this asserts that nothing was blocked.
 */
import { test, expect } from './fixtures';

test('the document carries the policy, and the policy does not block its own script', async ({
  signedIn,
  page
}) => {
  expect(signedIn.url).toContain('127.0.0.1');
  const violations: string[] = [];
  page.on('console', (m) => {
    if (/Content Security Policy|Refused to execute/i.test(m.text())) violations.push(m.text());
  });

  const res = await page.goto('/sign-in');
  const csp = res?.headers()['content-security-policy'] ?? '';
  console.log('CSP:', csp);
  expect(csp).toContain("frame-ancestors 'none'");
  expect(csp).toContain("base-uri 'none'");
  expect(res?.headers()['x-content-type-options']).toBe('nosniff');

  // The theme pin is inline and must still run: it sets data-theme before first paint.
  await page.evaluate(() => localStorage.setItem('theme', 'dark'));
  await page.reload();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  expect(violations, `CSP blocked something: ${violations.join(' | ')}`).toEqual([]);
});
