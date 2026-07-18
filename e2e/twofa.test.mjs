// Authenticator-app (TOTP) two-factor SETUP UI, driven through the browser:
// the account security card offers enrollment, and starting it issues a
// scannable secret + QR + a code field. (The full enroll->verify->AAL2
// round-trip and the re-login 2FA challenge are covered end-to-end by the
// server-side e2e harness; a browser TOTP *verify* depends on GoTrue's live
// challenge timing and is deliberately not asserted here.)
// Needs SUPABASE_URL + SUPABASE_SERVICE_KEY; skips otherwise.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, hasAuth, testEmail, loginBrowser, deleteUser, totp } from './lib/harness.mjs';

const skip = hasAuth() ? false : 'SUPABASE not set';

let driver;
const created = [];
before(async () => {
  if (!hasAuth()) return;
  driver = await Driver.launch();
});
after(async () => {
  await driver?.close();
  for (const email of created) await deleteUser(email).catch(() => {});
});

test(`[${Driver.engine()}] account offers TOTP 2FA setup and issues a usable secret`, { skip }, async () => {
  const email = testEmail('twofa');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email);

    // Save the default (personal) profile so /account renders the security card.
    await page.goto(`${BASE_URL}/account/setup`);
    await page.waitFor('form[action="/account/setup"] button[type="submit"]');
    await page.click('form[action="/account/setup"] button[type="submit"]');
    await page.waitAwayFrom('/account/setup', { timeout: 10000 });

    // The security card advertises authenticator-app enrollment.
    await page.goto(`${BASE_URL}/account`);
    await page.waitFor('form[action="/account/2fa/totp"] button[type="submit"]');
    assert.match((await page.content()).toLowerCase(), /authenticator/, 'security card offers TOTP');

    // Starting enrollment issues the shared secret + QR + a verification field.
    await page.click('form[action="/account/2fa/totp"] button[type="submit"]');
    await page.waitFor('input[name="code"]', { timeout: 10000 }); // visible code field
    assert.match(await page.url(), /\/account\/2fa\/totp/, 'on the QR page');
    const secret = await page.attr('input[name="secret"]', 'value');
    assert.ok(secret && secret.length >= 16, 'a base32 TOTP secret was issued');
    // The secret is real: a live 6-digit code derives from it (what an app shows).
    assert.match(totp(secret), /^\d{6}$/, 'secret yields a valid TOTP code');
    assert.ok(await page.exists('input[name="factor_id"]'), 'factor id carried for verify');
  } finally {
    await page.close();
  }
});
