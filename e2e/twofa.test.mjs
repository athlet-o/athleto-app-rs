// Authenticator-app (TOTP) two-factor enrollment, driven through the browser:
// enroll -> read the secret off the QR page -> compute a live code -> verify ->
// the account reports 2FA on, and a fresh login is bounced to the 2FA step.
// Needs SUPABASE_URL + SUPABASE_SERVICE_KEY; skips otherwise.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import {
  BASE_URL,
  hasAuth,
  testEmail,
  loginBrowser,
  deleteUser,
  totp,
} from './lib/harness.mjs';

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

test(`[${Driver.engine()}] TOTP 2FA enrolls and is then required at login`, { skip }, async () => {
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

    // Kick off TOTP enrollment -> QR page with the factor id + shared secret.
    await page.goto(`${BASE_URL}/account`);
    await page.waitFor('form[action="/account/2fa/totp"] button[type="submit"]');
    await page.click('form[action="/account/2fa/totp"] button[type="submit"]');
    await page.waitFor('input[name="secret"]', { timeout: 10000 });
    const secret = await page.attr('input[name="secret"]', 'value');
    assert.ok(secret && secret.length >= 16, 'a TOTP secret was issued');

    // Verify with a live code computed from the secret.
    await page.fill('input[name="code"]', totp(secret));
    await page.click('form[action="/account/2fa/totp/verify"] button[type="submit"]');
    await page.waitFor('header.site-header', { timeout: 10000 });
    assert.match(await page.url(), /enrolled=1|\/account/, 'landed back on account');
    const account = await page.content();
    assert.match(account, /two-factor authentication is on|verified/i, 'account shows 2FA active');

    // A fresh browser logging in as the same user must now be stopped at 2FA.
    const page2 = await driver.newPage();
    try {
      await loginBrowser(page2, email);
      // needs_aal2 -> the landing is the second-factor challenge.
      assert.match(await page2.url(), /\/login\/2fa/, 'fresh login is bounced to 2FA');
      assert.match(await page2.content(), /second factor|authenticator/i, '2FA challenge shown');
    } finally {
      await page2.close();
    }
  } finally {
    await page.close();
  }
});
