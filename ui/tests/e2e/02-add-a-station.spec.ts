import { test, expect } from './fixtures';
import { readJson, waitUntilReady } from './daemon';

/**
 * Flow 2 of §6.7's five: **add a station**.
 *
 * §0.7 calls this the hardest screen in the product, and the two properties worth
 * checking end to end are the ones §7.3 names as defects today: the form is built from
 * the chosen type rather than showing all fifty-odd `DeviceData` columns, and the four
 * fields M5-2 made non-negotiable are actually on it and actually persist.
 */
test('the form is built from the type, and the calibration fields persist', async ({
  signedIn,
  page
}) => {
  // The fixture is the daemon: naming it is what claims the instance and signs in.
  expect(signedIn.url).toContain('127.0.0.1');

  await page.goto('/stations/new');
  await expect(page.getByText('What kind of weather station do you have?')).toBeVisible();

  // Before a type is chosen there is no form at all, which is §0.7's inversion of today's
  // behaviour.
  await expect(page.getByLabel('Name', { exact: true })).toHaveCount(0);

  await page.getByRole('radio', { name: /Davis Vantage Pro/ }).click();

  // Now the Davis fields are there -- and the WeatherLink sensor mapping is not, because
  // a Davis console does not have one.
  await expect(page.getByLabel('Name', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Baud rate')).toBeVisible();
  await expect(page.getByLabel('Which sensor is which')).toHaveCount(0);

  await page.getByLabel('Name', { exact: true }).fill('Backyard');
  await page.getByLabel('Short name').fill('backyard');

  // The M5-2 four. "It is not a cosmetic field": the Go configuration's −94° against this
  // side's 0 made `winddir` differ on 100% of readings with nothing downstream able to
  // tell, and a wrong longitude is a wrong answer that still looks like weather.
  await page.getByLabel('Latitude').fill('40.47571');
  await page.getByLabel('Longitude').fill('-111.84543');
  await page.getByLabel('Altitude').fill('1902');
  await page.getByLabel('Wind direction correction').fill('-94');

  // §0.7: serial ports are **enumerated**, never typed. Which ports a machine has is not
  // this test's business -- a CI container and a Pi disagree -- so what is asserted is the
  // affordance rather than its contents.
  await expect(page.getByLabel('Serial port')).toBeVisible();

  // A cabled station with no port chosen is refused by `plan()`, and §7.2 says that error
  // lands **on the field** rather than in a banner. Asserted here because "the save
  // silently did nothing" is the defect this whole design exists against.
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByRole('alert').first()).toContainText(/serial port/i);

  // Now the other connection the same type offers. Same form, same four calibration
  // fields, different transport -- which is §0.7's point about the form being built from
  // the type rather than from a fixed list of columns.
  await page.getByLabel('How it connects').click();
  await page.getByRole('option', { name: 'Over the network' }).click();
  await expect(page.getByLabel('Address')).toBeVisible();
  await expect(page.getByLabel('Serial port')).toHaveCount(0);
  await page.getByLabel('Address').fill('127.0.0.1');
  await page.getByLabel('Port', { exact: true }).fill('22222');

  await page.getByRole('button', { name: 'Save', exact: true }).click();

  // §0.7 step 3 is where a save lands, because "are readings arriving" is the question
  // the operator actually has.
  await expect(page).toHaveURL(/\/stations\/backyard\/status$/);

  // **Adding a station restarts every component, including the one listener** -- M7-1's
  // stated cost -- so the navigation that follows the save can be refused mid-flight
  // while the socket is rebound. §8.8's error page offers a Retry for exactly this, and
  // taking it is what an operator would do; a test waits for the socket instead of
  // racing it.
  await waitUntilReady(signedIn);
  if (!(await page.getByRole('heading', { name: 'Backyard', level: 1 }).isVisible())) {
    await page.goto('/stations/backyard/status');
  }
  await expect(page.getByRole('heading', { name: 'Backyard', level: 1 })).toBeVisible();

  // The round trip, through the API this time: a value that reaches the screen and not
  // the store is the defect this flow exists to catch.
  const stored = await page.request.get('/api/v1/stations/backyard', {
    headers: { 'X-MW-Request': '1' }
  });
  expect(stored.ok()).toBe(true);
  const station = await stored.json();
  expect(station.wind_dir_correction_degrees).toBe(-94);
  expect(station.latitude).toBeCloseTo(40.47571, 4);
  expect(station.longitude).toBeCloseTo(-111.84543, 4);
  expect(station.altitude_m).toBeCloseTo(1902, 1);

  // And it is back on the form, which is the half a `POST` test would miss.
  await page.goto('/stations/backyard');
  await expect(page.getByLabel('Wind direction correction')).toHaveValue('-94');
  await expect(page.getByLabel('Altitude')).toHaveValue('1902');
});

/**
 * A second type, and the reason it is a second test rather than a second paragraph.
 *
 * The first flow exercises a Davis, whose transport is `serial` or `tcp_out`. A snow gauge
 * is `http_out`, and `http_out` is the branch the plan **refuses without
 * `transport.streaming`** -- a field no control offers, because whether a WeatherLink Live
 * pushes or is polled is a fact about the hardware rather than a choice. A form that left
 * it unset could save a Davis perfectly and never save a gauge at all, which is precisely
 * what the first version of this did and what no other check could see.
 *
 * It also pins the type's spelling. `stations.type`'s values are enforced by a SQL `CHECK`
 * and typed as `string` in the OpenAPI document, so §4.3's drift mechanisms are blind to a
 * wrong one: it is a 422 on every save of that page and nothing fails until it is opened.
 */
test('a type whose transport the plan validates differently also saves', async ({
  signedIn,
  page
}) => {
  expect(signedIn.url).toContain('127.0.0.1');
  await page.goto('/stations/new');
  await page.getByRole('radio', { name: /Snow depth sensor/ }).click();

  // Only this type's fields: a gauge has a snow base distance and no wind vane.
  await expect(page.getByLabel('Base snow distance')).toBeVisible();
  await expect(page.getByLabel('Wind direction correction')).toHaveCount(0);

  await page.getByLabel('Name', { exact: true }).fill('Driveway');
  await page.getByLabel('Short name').fill('driveway');
  // A snow gauge is `grpc_out` -- `mw-station/src/plan.rs`'s table is the authority on
  // which transport each driver has a mode for, and offering one it does not is a Save
  // that cannot succeed.
  await page.getByLabel('Address to read from').fill('http://127.0.0.1:9000');
  await page.getByLabel('Base snow distance').fill('1422');
  await page.getByLabel('Latitude').fill('40.47571');
  await page.getByLabel('Longitude').fill('-111.84543');
  await page.getByLabel('Altitude').fill('1902');

  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page).toHaveURL(/\/stations\/driveway\/status$/);

  await waitUntilReady(signedIn);
  const stored = await page.request.get('/api/v1/stations/driveway', {
    headers: { 'X-MW-Request': '1' }
  });
  expect(stored.ok(), `the gauge was refused: ${await stored.text()}`).toBe(true);
  const station = await stored.json();
  expect(station.kind).toBe('snowgauge');
  expect(station.snow_base_distance_mm).toBeCloseTo(1422, 1);
  expect(station.transport.kind).toBe('grpc_out');
  expect(station.transport.url).toBe('http://127.0.0.1:9000');
  // **The field no control offers.** W2a §4a.3 gives `streaming` to `GrpcOut` and not to
  // `HttpOut`, and the configuration database enforces exactly that -- so this kind must
  // carry one and an `http_out` kind must not. Asserted because the API used to demand it
  // of both, which made the two `http_out` station types impossible to store at all.
  expect(station.transport.streaming).toBe(true);
});

/**
 * The toggle that stopped a station on the preview, and said nothing.
 *
 * It was the one unconfirmed, immediately-committed configuration change in the whole
 * application: a switch in a table row whose label text was part of its hit area, with no
 * preview, no confirmation and no undo. One stray click stopped three components --
 * including the public website and the frozen API the phone app uses -- and the operator
 * was left looking at a station that had simply stopped working.
 *
 * §8.7's rule is that a change which breaks something else says so first, and §2.4's
 * preview is how every other save on this surface already does that.
 */
test('stopping a station is previewed and confirmed; starting one is not', async ({
  signedIn,
  page
}) => {
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
  await waitUntilReady(signedIn);

  await page.goto('/stations');
  const toggle = page.getByRole('switch', { name: /Collecting readings for Backyard/ });
  await expect(toggle).toBeVisible();

  // Turning it OFF asks first, and names the consequence.
  await toggle.click();
  const dialog = page.getByRole('alertdialog');
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText('Stop collecting readings?');

  // Cancelling changes nothing -- which is the half that makes a confirmation worth
  // having, and the half a dialog with a one-way `open` gets wrong.
  await dialog.getByRole('button', { name: 'Cancel' }).click();
  await expect(dialog).toHaveCount(0);
  // **The switch goes back.** chonky's `Toggle` owns its own checked state, so a click
  // moves it whatever the caller does next -- and a switch left showing "off" over a
  // station that is still on turns the *next* click into "turn on", which is how a
  // confirmation becomes a way to silently do the opposite of what it asked.
  await expect(
    page.getByRole('switch', { name: /Collecting readings for Backyard/ })
  ).toBeChecked();
  let station = await readJson(page.request, signedIn, '/api/v1/stations/backyard');
  expect(station.collecting, 'cancelling must not have written anything').toBe(true);

  // Confirming does it.
  await page.getByRole('switch', { name: /Collecting readings for Backyard/ }).click();
  await page.getByRole('alertdialog').getByRole('button', { name: 'Stop collecting' }).click();
  station = await readJson(page.request, signedIn, '/api/v1/stations/backyard');
  expect(station.collecting).toBe(false);

  // Turning it back ON is additive and reversible by the same click, so it does not ask.
  await page.goto('/stations');
  await page.getByRole('switch', { name: /Collecting readings for Backyard/ }).click();
  await expect(page.getByRole('alertdialog')).toHaveCount(0);
  station = await readJson(page.request, signedIn, '/api/v1/stations/backyard');
  expect(station.collecting).toBe(true);
});
