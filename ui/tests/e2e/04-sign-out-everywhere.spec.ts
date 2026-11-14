import { test, expect, ADMIN } from './fixtures';

/**
 * Flow 4 of §6.7's five: **sign out everywhere**.
 *
 * §3.3's whole argument is that a session is not a credential, and the defect it replaces
 * is that today's session cookie **is** the API credential -- so rotating it signs you out
 * of a session you cannot otherwise revoke. Two browser contexts are the only way to
 * check that: one unit test cannot hold two sessions, and the server-side test cannot see
 * that the second browser is actually turned away at the door.
 */
test('a second browser is listed, is revocable, and is turned away', async ({
  signedIn,
  page,
  browser
}) => {
  // How many sessions there already are. More than one is normal and is not an accident:
  // the claim itself returns a session, and signing in through the form opens another --
  // which is §3.3's whole reading of D-35, "one identity, many revocable sessions".
  await page.goto('/system/administrator');
  const rows = page.locator('table tbody tr');
  // The list is fetched after the page renders, so count it once it has arrived rather
  // than racing it to zero.
  await expect(rows.first()).toBeVisible();
  const before = await rows.count();

  // A second, independent browser context -- a different cookie jar, which is what makes
  // it a second session rather than a second tab.
  const second = await browser.newContext({ baseURL: signedIn.url });
  const other = await second.newPage();
  await other.goto('/sign-in');
  await other.getByLabel('Username').fill(ADMIN.username);
  await other.getByLabel('Password', { exact: true }).fill(ADMIN.password);
  await other.getByRole('button', { name: 'Sign in' }).click();
  await expect(other.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();

  // It is listed, and exactly one row is marked as the one being looked at.
  await page.reload();
  // By text rather than by role: chonky's `Box` renders its `title` as a styled `div`, so
  // a page built from boxes has no heading outline at all. Reported upstream as gap G15;
  // every page in this application has an `<h1>` of its own, which is what keeps the
  // outline from being empty in the meantime.
  await expect(page.getByText('Where you are signed in')).toBeVisible();
  await expect(rows).toHaveCount(before + 1);
  await expect(page.getByText('This browser')).toHaveCount(1);

  // Revoke the other one individually. §3.3: sessions are individually revocable, which
  // is the capability that does not exist today at all.
  await page.getByRole('button', { name: 'Sign out', exact: true }).first().click();
  await expect(rows).toHaveCount(before);

  // The revoked browser is turned away on its next request -- and §8.8's signed-out state
  // is the sign-in screen rather than a banner, which is why the 401 has to be its own
  // status.
  await other.reload();
  await expect(other).toHaveURL(/\/sign-in$/);

  // "Sign out everywhere" takes this one too, which is the honest reading of the words.
  // The trigger and the confirmation share a name on purpose -- a destructive action
  // should say the same thing twice -- so the second click is scoped to the dialog.
  await page.getByRole('button', { name: 'Sign out everywhere' }).first().click();
  const confirm = page.getByRole('alertdialog');
  await expect(confirm).toBeVisible();
  await confirm.getByRole('button', { name: 'Sign out everywhere' }).click();
  await expect(page).toHaveURL(/\/sign-in$/);

  await second.close();
});
