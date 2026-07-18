// One browser-driver interface, two engines. The same test logic runs under
// Playwright and Puppeteer (selected by E2E_ENGINE) so we exercise both the
// auto-waiting engine and the closer-to-CDP one against identical assertions.
//
// Both engines drive the *system* Google Chrome (channel: 'chrome') so CI and
// laptops don't download a second Chromium. A `hostHeader` option rewrites the
// Host header via request interception, which is how the biz.-host chrome is
// exercised against a local server without DNS games.

const ENGINE = process.env.E2E_ENGINE || 'playwright';

/** A thin page wrapper normalizing the two engines' APIs. */
class Page {
  constructor(engine, page, ctx) {
    this.engine = engine;
    this._page = page;
    this._ctx = ctx; // playwright BrowserContext (null for puppeteer)
  }

  async goto(url, { waitUntil = 'load' } = {}) {
    await this._page.goto(url, { waitUntil });
  }

  /** Navigate and return { status, headers } of the main response. */
  async navigate(url, { waitUntil = 'load' } = {}) {
    const resp = await this._page.goto(url, { waitUntil });
    return { status: resp ? resp.status() : 0, headers: resp ? resp.headers() : {} };
  }

  async waitFor(selector, { state = 'visible', timeout = 10000 } = {}) {
    if (this.engine === 'playwright') {
      await this._page.waitForSelector(selector, { state, timeout });
    } else {
      const visible = state === 'visible';
      await this._page.waitForSelector(selector, { visible, timeout });
    }
  }

  async click(selector) {
    await this._page.click(selector);
  }

  async fill(selector, value) {
    if (this.engine === 'playwright') {
      await this._page.fill(selector, value);
    } else {
      await this._page.waitForSelector(selector);
      await this._page.$eval(selector, (el) => (el.value = ''));
      await this._page.type(selector, value);
    }
  }

  async selectOption(selector, value) {
    if (this.engine === 'playwright') {
      await this._page.selectOption(selector, value);
    } else {
      await this._page.select(selector, value);
    }
  }

  /** textContent of the first match, or null. */
  async text(selector) {
    if (this.engine === 'playwright') {
      const el = await this._page.$(selector);
      return el ? (await el.textContent()) : null;
    }
    const el = await this._page.$(selector);
    return el ? this._page.evaluate((e) => e.textContent, el) : null;
  }

  async attr(selector, name) {
    if (this.engine === 'playwright') {
      const el = await this._page.$(selector);
      return el ? el.getAttribute(name) : null;
    }
    const el = await this._page.$(selector);
    return el ? this._page.evaluate((e, n) => e.getAttribute(n), el, name) : null;
  }

  async exists(selector) {
    return (await this._page.$(selector)) !== null;
  }

  async count(selector) {
    const els = await this._page.$$(selector);
    return els.length;
  }

  async content() {
    return this._page.content();
  }

  async url() {
    return this._page.url();
  }

  async title() {
    return this._page.title();
  }

  async evaluate(fn, ...args) {
    return this._page.evaluate(fn, ...args);
  }

  async screenshot(path) {
    try {
      await this._page.screenshot({ path });
    } catch {
      /* best-effort */
    }
  }

  async close() {
    if (this.engine === 'playwright') {
      await this._ctx.close();
    } else {
      await this._page.close();
    }
  }
}

/** Launches the selected engine and hands out isolated pages. */
export class Driver {
  static engine() {
    return ENGINE;
  }

  static async launch() {
    if (ENGINE === 'playwright') {
      const { chromium } = await import('playwright');
      const browser = await chromium.launch({ channel: 'chrome', headless: true });
      return new Driver('playwright', browser);
    }
    if (ENGINE === 'puppeteer') {
      const puppeteer = (await import('puppeteer')).default;
      const browser = await puppeteer.launch({
        channel: 'chrome',
        headless: true,
        args: ['--no-sandbox', '--disable-dev-shm-usage'],
      });
      return new Driver('puppeteer', browser);
    }
    throw new Error(`unknown E2E_ENGINE: ${ENGINE}`);
  }

  constructor(engine, browser) {
    this.engine = engine;
    this._browser = browser;
  }

  /**
   * A fresh isolated page. `hostHeader` rewrites the Host header on every
   * request (used to exercise the biz. storefront against a local server).
   */
  async newPage({ hostHeader } = {}) {
    if (this.engine === 'playwright') {
      const ctx = await this._browser.newContext({ ignoreHTTPSErrors: true });
      if (hostHeader) {
        await ctx.route('**/*', (route) => {
          const headers = { ...route.request().headers(), host: hostHeader };
          route.continue({ headers });
        });
      }
      const page = await ctx.newPage();
      return new Page('playwright', page, ctx);
    }
    const page = await this._browser.newPage();
    if (hostHeader) {
      await page.setRequestInterception(true);
      page.on('request', (req) => {
        const headers = { ...req.headers(), host: hostHeader };
        req.continue({ headers }).catch(() => req.continue());
      });
    }
    return new Page('puppeteer', page, null);
  }

  async close() {
    await this._browser.close();
  }
}
