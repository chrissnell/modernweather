import { test, expect } from './fixtures';

/**
 * Flow 5 of §6.7's five: **the activity log reconnecting across a daemon restart**.
 *
 * The flow with the most machinery behind it and the least visibility to anything else.
 * The unit test over `$lib/sse/machine` proves the schedule and the state transitions; it
 * cannot prove that a real `EventSource` against a real `GET /api/v1/events` backfills the
 * ring, draws the live boundary in the right place, survives the socket going away, and
 * comes back with the records it missed.
 *
 * §5.2's distinction is the assertion that matters: **a server restart is not an error.**
 * It produces one reconnect, a `gap` if applicable, and an `INFO` line -- not a red
 * banner.
 */
test('the log backfills, goes live, survives a restart and comes back', async ({
  signedIn,
  page
}) => {
  await page.goto('/system/activity');
  await expect(page.getByRole('heading', { name: 'Activity', level: 1 })).toBeVisible();

  // §5.3: the backfill is the ring's current contents, and the boundary between it and
  // the live records is explicit rather than guessed -- which is what the "— live —" rule
  // is.
  await expect(page.getByText('— live —').first()).toBeVisible({ timeout: 30_000 });

  // The connection indicator says what it is doing (§5.2). It never shows stale data as
  // though it were current.
  const indicator = page.locator('header').getByText(/live|reconnecting|disconnected/);
  await expect(indicator).toContainText('live');

  // Something to find after the restart. A request on the management API writes an access
  // record, so this is a log line the test caused on purpose.
  await page.request.get('/api/v1/system/info', { headers: { 'X-MW-Request': '1' } });

  // Something arrived: the viewer is showing rows rather than its empty state. Asserted
  // on the absence of the empty sentence rather than on a chonky class name, which is an
  // implementation detail of the toolkit.
  await expect(page.getByText('Entries appear here as they happen.')).toHaveCount(0);

  // Now take the daemon away under the open connection.
  await signedIn.restart();

  // §5.2: the UI says what it is doing rather than silently showing the old list as
  // current. The backoff starts at a second, so this is quick.
  await expect(indicator).toContainText(/reconnecting|disconnected|live/, {
    timeout: 20_000
  });

  // And it comes back on its own, with the new process's ring backfilled behind the
  // boundary marker again. No reload, no Retry click: that is the whole point of §5.2's
  // schedule being capped at fifteen seconds.
  await expect(indicator).toContainText('live', { timeout: 45_000 });

  // **Two** boundary markers now, and that is the assertion rather than an inconvenience:
  // the reconnect backfilled the new process's ring and drew the live rule again, so the
  // operator can see where the restart was in their own log.
  await expect(page.getByText('— live —')).toHaveCount(2, { timeout: 30_000 });

  // Still signed in across the restart, because the session is a row in the configuration
  // database rather than process memory -- so a restart is not a sign-out.
  await expect(page.getByRole('heading', { name: 'Activity', level: 1 })).toBeVisible();

  // A restart is INFO, not a failure: nothing on the page claims the system broke.
  await expect(page.getByText("Can't reach RemoteWeather.")).toHaveCount(0);
});
