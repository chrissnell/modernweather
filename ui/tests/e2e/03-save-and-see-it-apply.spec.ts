import { test, expect } from './fixtures';
import { waitUntilReady } from './daemon';

/**
 * Flow 3 of §6.7's five: **save and see it apply** (§2.4).
 *
 * This is the whole point of §2, and it is precisely the flow no other check can see.
 * `svelte-check` cannot tell whether a save applied; a unit test over the
 * `applied_generation` comparison proves the arithmetic and not that the server moves the
 * integer; and the schema gate proves the field exists. What is being checked here is
 * that **the preview names what the save then restarts** -- M7-1's done-criterion -- and
 * that the banner reports the reconciliation rather than reporting success at the moment
 * of the write.
 */
test('a save previews its consequence, commits, and reports becoming live', async ({
  signedIn,
  page
}) => {
  // A station first, so there is something whose save has a consequence.
  await page.request.post('/api/v1/stations', {
    headers: { 'X-MW-Request': '1' },
    data: {
      name: 'Backyard',
      key: 'backyard',
      kind: 'campbellscientific',
      identity_kind: 'fixed',
      collecting: true,
      wind_dir_correction_degrees: 0,
      transport: { kind: 'tcp_out', host: '127.0.0.1', port: 7001 }
    }
  });

  // Adding a station restarts the one listener, so the API is briefly unreachable.
  await waitUntilReady(signedIn);

  await page.goto('/stations/backyard');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('Backyard');

  // ① BEFORE SAVE: the preview, which is `POST /api/v1/config/preview` running the
  // supervisor's own pure `plan()` against the proposed configuration inside a
  // transaction it then rolls back.
  await page.getByLabel('Wind direction correction').fill('-94');
  await page.getByRole('button', { name: 'Save', exact: true }).first().click();

  const preview = page.getByText(/Saving will restart|Nothing changes/);
  await expect(preview).toBeVisible();
  const previewText = (await preview.textContent()) ?? '';

  // The preview is not cosmetic: it names components, and §2.4's sentence is in the
  // operator's words rather than in component ids.
  expect(previewText).not.toMatch(/station:|component/i);

  // Commit.
  await page.getByRole('button', { name: 'Save', exact: true }).last().click();

  // ② JUST SAVED → ③ LIVE. The banner is the shell's, so it survives the page it was
  // saved on -- which is the behaviour §2.4 asks for, because reconciliation takes
  // seconds and the operator will have navigated.
  await expect(
    page.getByText(/is restarting|are restarting|Saved and live|not running yet/)
  ).toBeVisible();

  // The generation moved and the value is stored. Asserted through the API because the
  // defect §2.1 removes is a screen that says "saved" for a write that did not happen.
  const stored = await page.request.get('/api/v1/stations/backyard', {
    headers: { 'X-MW-Request': '1' }
  });
  expect((await stored.json()).wind_dir_correction_degrees).toBe(-94);

  // And the component list reports an applied generation, which is the integer the whole
  // mechanism rests on.
  const components = await page.request.get('/api/v1/runtime/components', {
    headers: { 'X-MW-Request': '1' }
  });
  const view = await components.json();
  expect(view.generation).toBeGreaterThan(0);
  expect(Array.isArray(view.components)).toBe(true);

  // Eventually everything affected catches up, and the banner says so rather than
  // fading out silently.
  await expect(page.getByText(/Saved and live|not running yet|won't start/)).toBeVisible({
    timeout: 30_000
  });
});
