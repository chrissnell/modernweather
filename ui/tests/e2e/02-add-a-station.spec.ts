import { test, expect } from './fixtures';
import { waitUntilReady } from './daemon';

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
