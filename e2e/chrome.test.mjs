// The one binary serves two storefronts by Host header: app.athleto.store
// (B2C) and biz.athleto.store (B2B). The B2C host is exercised locally; the
// biz chrome depends on the inbound Host prefix, which a browser can't spoof
// for a navigation, so the biz check runs against the real deployed host
// (E2E_BIZ_URL, default https://biz.athleto.store) and skips with E2E_SKIP_LIVE=1.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, BIZ_URL } from './lib/harness.mjs';

const skipLive = process.env.E2E_SKIP_LIVE === '1' ? 'E2E_SKIP_LIVE=1' : false;

let driver;
before(async () => {
  driver = await Driver.launch();
});
after(async () => {
  await driver?.close();
});

test(`[${Driver.engine()}] B2C host shows the consumer hero, no BUSINESS chip`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    await page.waitFor('.site-header');
    const html = await page.content();
    assert.match(html, /Wobble hard/i, 'consumer hero');
    assert.doesNotMatch(html, /class="biz-chip"|>BUSINESS</, 'no business chip on app host');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] B2B host shows the BUSINESS chip and wholesale hero`, { skip: skipLive }, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BIZ_URL}/`);
    await page.waitFor('.site-header');
    const html = await page.content();
    assert.match(html, /BUSINESS/, 'business chip present');
    assert.match(html, /Stock the wobble|Wholesale/i, 'wholesale hero');
    assert.match(html, /Quick order/i, 'B2B quick-order CTA');
  } finally {
    await page.close();
  }
});
