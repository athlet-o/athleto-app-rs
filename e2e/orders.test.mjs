// Authenticated journeys: passwordless login, B2C order -> receipt, and the
// B2B 2FA-required gate. These need SUPABASE_URL + SUPABASE_SERVICE_KEY; when
// absent the whole suite skips so the no-auth suites still run in CI.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, hasAuth, testEmail, loginBrowser, deleteUser } from './lib/harness.mjs';

const skip = hasAuth() ? false : 'SUPABASE_URL / SUPABASE_SERVICE_KEY not set';

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

test(`[${Driver.engine()}] magic-link login lands signed in`, { skip }, async () => {
  const email = testEmail('login');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email);
    await page.goto(`${BASE_URL}/`);
    const html = await page.content();
    assert.match(html, /Log out/i, 'signed-in nav shows Log out');
    assert.match(html, new RegExp(email.replace(/[.@+]/g, '\\$&')), 'nav shows the email');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] B2C order flows through to a receipt`, { skip }, async () => {
  const email = testEmail('b2c');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email); // new user -> lands on /account/setup

    // Save the default (personal) profile.
    await page.goto(`${BASE_URL}/account/setup`);
    await page.waitFor('form[action="/account/setup"] button[type="submit"]');
    await page.click('form[action="/account/setup"] button[type="submit"]');
    await page.waitFor('.hero, .product-grid', { timeout: 10000 });

    // Add an item and check out.
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });

    await page.goto(`${BASE_URL}/cart`);
    await page.waitFor('.checkout-form button[type="submit"]');
    await page.click('.checkout-form button[type="submit"]');

    // Lands on /orders?placed=1.
    await page.waitFor('.order-card', { timeout: 10000 });
    assert.ok(await page.exists('.status-badge'), 'order status badge');
    const meta = await page.text('.order-meta');
    assert.match(meta, /Est\. delivery/i, 'delivery estimate shown');

    // Open the receipt.
    await page.click('.order-card a.button');
    await page.waitFor('.receipt', { timeout: 10000 });
    const receipt = await page.content();
    assert.match(receipt, /Subtotal/i, 'receipt subtotal');
    assert.match(receipt, /Total/i, 'receipt total');
    assert.match(receipt, /Print \/ Save PDF/i, 'printable');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] B2B setup requires 2FA before ordering`, { skip }, async () => {
  const email = testEmail('b2b');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email);
    await page.goto(`${BASE_URL}/account/setup`);
    await page.waitFor('input[name="customer_type"][value="b2b"]');
    await page.click('input[name="customer_type"][value="b2b"]');
    await page.fill('input[name="company_name"]', 'Wobble Distribution E2E');
    await page.click('form[action="/account/setup"] button[type="submit"]');
    // B2B without a factor is bounced to /account?required2fa=1.
    await page.waitFor('.notice.error, #security', { timeout: 10000 });
    const html = await page.content();
    assert.match(html, /two-factor/i, 'demands 2FA');
    assert.match(await page.url(), /required2fa=1|\/account/, 'on the account/security page');
  } finally {
    await page.close();
  }
});
