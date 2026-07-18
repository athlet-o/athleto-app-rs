// Puppeteer browser suite driving the REAL running server (degraded no-secrets
// mode when APP_BASE_URL is unset). Run with: node --test storefront.pptr.test.mjs
import assert from "node:assert/strict";
import { test, before, after } from "node:test";
import puppeteer from "puppeteer";
import { startServer } from "./server.mjs";

const BASE = (process.env.APP_BASE_URL ?? "http://127.0.0.1:0").replace(/\/+$/, "");

// Workstation/CI portability trick: prefer Puppeteer's own chromium, fall back
// to the Playwright-installed chromium executable when the default is missing.
async function launchPuppeteerWithFallback() {
  try {
    return await puppeteer.launch({
      headless: true,
      args: ["--no-sandbox", "--disable-setuid-sandbox"],
    });
  } catch {
    return await puppeteer.launch({
      headless: true,
      executablePath: (await import("playwright")).chromium.executablePath(),
      args: ["--no-sandbox", "--disable-setuid-sandbox"],
    });
  }
}

let server;
let browser;
let base;

before(async () => {
  server = await startServer();
  base = server.base;
  browser = await launchPuppeteerWithFallback();
});

after(async () => {
  if (browser && browser.connected) await browser.close();
  if (server) await server.stop();
});

test("[pptr] /login GET renders the sign-in page", async () => {
  const page = await browser.newPage();
  try {
    const response = await page.goto(`${base}/login`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 200);
    const bodyText = (await page.$eval("body", (el) => el.innerText)).toLowerCase();
    assert.match(bodyText, /sign in/);
    console.log("[pptr] login page OK");
  } finally {
    await page.close();
  }
});

test("[pptr] storefront forms embed the hidden csrf_token field", async () => {
  // In degraded mode /login shows a not-configured notice (no Supabase form),
  // but the storefront's add-to-cart forms always carry the double-submit
  // csrf_token hidden field, and the layout hands it to htmx via hx-headers.
  const page = await browser.newPage();
  try {
    const response = await page.goto(`${base}/`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 200);
    const html = await page.content();
    assert.match(html, /name="csrf_token"/);
    assert.match(html, /hx-headers/);
    assert.match(html, /x-csrf-token/);
    assert.ok((await page.$("form")) !== null, "storefront must contain a form");
    console.log("[pptr] storefront csrf form OK");
  } finally {
    await page.close();
  }
});

test("[pptr] an unknown route returns 404", async () => {
  const page = await browser.newPage();
  try {
    const response = await page.goto(`${base}/definitely/not/a/route`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 404);
    console.log("[pptr] 404 OK");
  } finally {
    await page.close();
  }
});

test("[pptr] vendored htmx ws extension serves 200 javascript", async () => {
  const page = await browser.newPage();
  try {
    const response = await page.goto(`${base}/static/htmx-ext-ws-2.0.2.js`, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    assert.ok(response);
    assert.equal(response.status(), 200);
    const headers = response.headers();
    assert.match(headers["content-type"] ?? "", /javascript/);
    assert.match(headers["cache-control"] ?? "", /immutable/);
    console.log("[pptr] htmx ws ext OK");
  } finally {
    await page.close();
  }
});
