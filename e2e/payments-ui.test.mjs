// Payment-status surface on the orders page. With no payment provider keys
// configured (the default), a placed order settles nowhere and stays
// payment-pending -- the orders page must show its payment badge and a
// "Pay now" retry. Needs SUPABASE_URL + SUPABASE_SERVICE_KEY; skips otherwise.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, hasAuth, testEmail, loginBrowser, deleteUser } from './lib/harness.mjs';

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

test(`[${Driver.engine()}] a placed order shows a payment status + Pay-now retry`, { skip }, async () => {
  const email = testEmail('payui');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email);
    await page.goto(`${BASE_URL}/account/setup`);
    await page.waitFor('form[action="/account/setup"] button[type="submit"]');
    await page.click('form[action="/account/setup"] button[type="submit"]');
    await page.waitAwayFrom('/account/setup', { timeout: 10000 });

    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });

    await page.goto(`${BASE_URL}/cart`);
    await page.waitFor('.checkout-form button[type="submit"]');
    await page.click('.checkout-form button[type="submit"]');
    await page.waitFor('.order-card', { timeout: 10000 });

    const orders = await page.content();
    // Every order carries a payment-status badge.
    assert.match(orders, /payment|pending|paid|invoiced/i, 'payment status shown on the order');
    // With no configured provider, the order is unpaid -> Pay now is offered.
    assert.ok(await page.exists('form[action$="/pay"] button'), 'Pay-now retry offered while unpaid');
  } finally {
    await page.close();
  }
});
