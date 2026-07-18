// Storefront + cart + 90-minute holds, driven as a guest (no auth needed).
// Runs under whichever engine E2E_ENGINE selects (playwright | puppeteer).
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL } from './lib/harness.mjs';

let driver;
before(async () => {
  driver = await Driver.launch();
});
after(async () => {
  await driver?.close();
});

test(`[${Driver.engine()}] storefront renders the product lineup`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card');
    const cards = await page.count('.product-card');
    assert.ok(cards >= 6, `expected >=6 product cards, got ${cards}`);
    const html = await page.content();
    assert.match(html, /Wobble hard/i, 'hero copy present');
    assert.match(html, /Add to cart/i, 'buy buttons present');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] product detail page loads with a price`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/product/recover-o-cup`);
    await page.waitFor('.product-card');
    const html = await page.content();
    assert.match(html, /\$\d+\.\d{2}/, 'shows a formatted price');
    assert.match(html, /recover/i, 'shows the product subname');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] guest add-to-cart claims a 90-minute hold`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    // htmx posts /cart/items (CSRF token rides the body's hx-headers).
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });
    const status = await page.text('.card-status');
    assert.match(status, /Added|Reserved|Sold out|available/i);
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] cart shows the item and the hold countdown banner`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });

    await page.goto(`${BASE_URL}/cart`);
    await page.waitFor('.cart-table');
    assert.ok(await page.exists('#hold-banner'), 'hold banner present');
    const seconds = Number(await page.attr('#hold-banner', 'data-seconds'));
    assert.ok(seconds > 5000, `hold ~90min, got ${seconds}s`);
    const total = await page.text('.cart-total');
    assert.match(total, /\$\d+\.\d{2}/, 'cart total shown');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] /cart/hold reports an active lease as JSON`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });

    const body = await page.evaluate(async (base) => {
      const r = await fetch(`${base}/cart/hold`, { credentials: 'same-origin' });
      return r.json();
    }, BASE_URL);
    assert.equal(body.active, true, 'lease active');
    assert.ok(body.seconds_left > 5000, `seconds_left ~90min, got ${body.seconds_left}`);
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] removing the only line empties the cart`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.product-card button.buy');
    await page.click('.product-card button.buy');
    await page.waitFor('.card-status .added', { timeout: 10000 });

    await page.goto(`${BASE_URL}/cart`);
    await page.waitFor('.cart-table button.danger');
    await page.click('.cart-table button.danger'); // htmx remove -> #cart-contents swap
    await page.waitFor('#cart-contents .notice', { timeout: 10000 });
    const empty = await page.text('#cart-contents .notice');
    assert.match(empty, /empty/i, 'cart reports empty after removal');
  } finally {
    await page.close();
  }
});
