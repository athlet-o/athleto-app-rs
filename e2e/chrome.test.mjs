// The one binary serves two storefronts by Host header: app.athleto.store
// (B2C) and biz.athleto.store (B2B). These assert each host's chrome. The biz
// host is reached either against the real deployed host (default) or against
// the local server with a rewritten Host header (E2E_BIZ_VIA_HOST_HEADER=1).
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, BIZ_URL, BIZ_VIA_HOST_HEADER } from './lib/harness.mjs';

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

test(`[${Driver.engine()}] B2B host shows the BUSINESS chip and wholesale hero`, async () => {
  // Local server + Host rewrite, or the real biz host.
  const page = BIZ_VIA_HOST_HEADER
    ? await driver.newPage({ hostHeader: 'biz.athleto.store' })
    : await driver.newPage();
  const url = BIZ_VIA_HOST_HEADER ? `${BASE_URL}/` : `${BIZ_URL}/`;
  try {
    await page.goto(url);
    await page.waitFor('.site-header');
    const html = await page.content();
    assert.match(html, /BUSINESS/, 'business chip present');
    assert.match(html, /Stock the wobble|Wholesale/i, 'wholesale hero');
    assert.match(html, /Quick order/i, 'B2B quick-order CTA');
  } finally {
    await page.close();
  }
});
