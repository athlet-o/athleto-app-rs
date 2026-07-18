// Security + passwordless-login surface, driven as a guest. These assert the
// hardening from the SeaORM merge (CSRF middleware, CSP headers, same-origin
// vendored assets) through a real browser.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import { BASE_URL, hasAuth } from './lib/harness.mjs';

let driver;
before(async () => {
  driver = await Driver.launch();
});
after(async () => {
  await driver?.close();
});

// /login renders the magic-link form only when Supabase is configured; without
// it the page correctly shows a "not configured" notice and no form. Gate this
// test on the same signal orders.test.mjs / b2b-approval.test.mjs use, so the
// no-secrets CI (and offline runs) stay green while full coverage runs wherever
// SUPABASE_URL / SUPABASE_SERVICE_KEY are set.
const skip = hasAuth() ? false : 'SUPABASE_URL / SUPABASE_SERVICE_KEY not set';

test(`[${Driver.engine()}] login page offers a magic link and no password`, { skip }, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/login`);
    await page.waitFor('form[action="/login"]');
    const html = await page.content();
    assert.match(html, /magic link/i, 'magic-link copy present');
    assert.ok(await page.exists('input[name="email"]'), 'email field present');
    assert.equal(await page.count('input[type="password"]'), 0, 'no password field');
    // The CSRF cookie is minted for the form's double-submit token.
    const csrf = await page.evaluate(
      () => (document.cookie.match(/(?:^|; )athleto_csrf=([^;]+)/) || [])[1] || '',
    );
    assert.ok(csrf.length > 0, 'csrf cookie minted');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] state-changing POST without a CSRF token is rejected`, async () => {
  const page = await driver.newPage();
  try {
    await page.goto(`${BASE_URL}/`);
    const status = await page.evaluate(async (base) => {
      const r = await fetch(`${base}/cart/items`, {
        method: 'POST',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: 'product_id=1&qty=1',
        credentials: 'same-origin',
      });
      return r.status;
    }, BASE_URL);
    assert.equal(status, 403, 'CSRF-less POST is 403');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] security headers (CSP) are present on the home page`, async () => {
  const page = await driver.newPage();
  try {
    const { status, headers } = await page.navigate(`${BASE_URL}/`);
    assert.equal(status, 200);
    assert.ok(headers['content-security-policy'], 'CSP header present');
    assert.ok(
      headers['x-content-type-options'] || headers['x-frame-options'],
      'at least one hardening header present',
    );
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] htmx is vendored same-origin (CSP-friendly)`, async () => {
  const page = await driver.newPage();
  try {
    const { status, headers } = await page.navigate(`${BASE_URL}/static/htmx-2.0.4.min.js`);
    assert.equal(status, 200, 'vendored htmx served');
    assert.match(headers['content-type'] || '', /javascript/, 'served as JS');
    // The layout must reference the same-origin path, not a CDN.
    await page.goto(`${BASE_URL}/`);
    const html = await page.content();
    assert.doesNotMatch(html, /unpkg\.com|cdn\.jsdelivr/, 'no CDN script tags');
  } finally {
    await page.close();
  }
});

test(`[${Driver.engine()}] unknown route returns 404`, async () => {
  const page = await driver.newPage();
  try {
    // A bodyless 404 makes Playwright's goto throw (ERR_HTTP_RESPONSE_CODE_FAILURE)
    // but not Puppeteer's; an in-page fetch reports the status identically in both.
    await page.goto(`${BASE_URL}/`);
    const status = await page.evaluate(async (base) => {
      const r = await fetch(`${base}/no-such-page-xyz`, { credentials: 'same-origin' });
      return r.status;
    }, BASE_URL);
    assert.equal(status, 404);
  } finally {
    await page.close();
  }
});
