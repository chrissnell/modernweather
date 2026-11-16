import { test, expect, ADMIN } from './fixtures';

/**
 * Flow 1 of §6.7's five: **the first-run claim**.
 *
 * Invisible to every other check: the setup code is printed to stderr and never to the
 * log ring (W5 D-27), the claim is a single transaction that creates the account and
 * invalidates the code together, and the result is a session cookie whose attributes no
 * unit test can see. §0.7 calls step 1 the only blocking step, and this is it.
 */
test('an unclaimed instance can only be set up, and the code works once', async ({
  daemon,
  page
}) => {
  // §7.1: `/setup` is the only page reachable while unclaimed, so a deep link to anything
  // else lands here. That also exercises the single-page fallback: the server has never
  // heard of `/stations`.
  await page.goto('/stations');
  await expect(page).toHaveURL(/\/setup$/);
  await expect(page.getByText('Welcome to RemoteWeather')).toBeVisible();

  // §6.3 gap G7: the code is normalised, because a person reading it off a console types
  // the dashes.
  await page.getByLabel('Setup code').fill(daemon.setupCode);
  await page.getByLabel('Username').fill(ADMIN.username);
  await page.getByLabel('Password', { exact: true }).fill(ADMIN.password);
  await page.getByLabel('Password again').fill(ADMIN.password);
  await page.getByRole('button', { name: 'Create my account' }).click();

  await expect(page.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();

  // §0.7's checklist is on Home until steps 1-3 are done, and step 1 is now done.
  await expect(page.getByText('Create your administrator account')).toBeVisible();
  await expect(page.getByText('Connect a weather station')).toBeVisible();

  // The code was invalidated in the same transaction that created the account, so a
  // second claim is refused. Asserted over the API rather than through the form, because
  // the form is no longer reachable -- which is itself the thing being checked.
  const second = await page.request.post('/api/v1/auth/claim', {
    headers: { 'X-MW-Request': '1' },
    data: {
      setup_code: daemon.setupCode.replace(/-/g, ''),
      username: 'someone-else',
      password: 'another-long-passphrase',
      password_again: 'another-long-passphrase'
    }
  });
  expect(second.status(), 'a used setup code is refused').toBe(409);

  // W5 D-27: the code is printed to the console and never to the log ring. The ring is
  // what `GET /api/v1/logs` serves, so this is the assertion that the rule holds.
  const logs = await page.request.get('/api/v1/logs?limit=2000', {
    headers: { 'X-MW-Request': '1' }
  });
  expect(logs.ok()).toBe(true);
  expect(await logs.text()).not.toContain(daemon.setupCode.replace(/-/g, ''));
});
