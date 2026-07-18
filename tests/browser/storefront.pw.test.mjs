// Playwright browser suite driving the REAL running server (degraded no-secrets
// mode when APP_BASE_URL is unset). Run with: node --test storefront.pw.test.mjs
import assert from "node:assert/strict";
import { test, before, after } from "node:test";
import { chromium } from "playwright";
import { startServer } from "./server.mjs";

const BASE = (process.env.APP_BASE_URL ?? "http://127.0.0.1:0").replace(/\/+$/, "");

let server;
let browser;
let base;

before(async () => {
  server = await startServer();
  base = server.base;
  browser = await chromium.launch({ headless: true });
});

after(async () => {
  if (browser) await browser.close();
  if (server) await server.stop();
});

test("[pw] storefront / renders the brand and carries security headers", async () => {
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    const response = await page.goto(`${base}/`, { waitUntil: "domcontentloaded", timeout: 30_000 });
    assert.ok(response, "expected a / response");
    assert.equal(response.status(), 200);

    const bodyText = (await page.locator("body").innerText()).toLowerCase();
    assert.match(bodyText, /the lineup/);
    assert.match(bodyText, /athlet/);

    const headers = response.headers();
    assert.match(headers["content-security-policy"] ?? "", /default-src 'self'/);
    assert.equal(headers["x-frame-options"], "DENY");
    assert.equal(headers["x-content-type-options"], "nosniff");
    console.log("[pw] storefront brand + security headers OK");
  } finally {
    await context.close();
  }
});

test("[pw] product page /product/recover-o-cup renders", async () => {
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    const response = await page.goto(`${base}/product/recover-o-cup`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 200);
    const bodyText = (await page.locator("body").innerText()).toLowerCase();
    assert.match(bodyText, /recover/);
    console.log("[pw] product page OK");
  } finally {
    await context.close();
  }
});

test("[pw] vendored htmx serves 200 javascript with immutable caching", async () => {
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    const response = await page.goto(`${base}/static/htmx-2.0.4.min.js`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 200);
    const headers = response.headers();
    assert.match(headers["content-type"] ?? "", /javascript/);
    assert.match(headers["cache-control"] ?? "", /immutable/);
    console.log("[pw] vendored htmx caching headers OK");
  } finally {
    await context.close();
  }
});
