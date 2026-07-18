// Shared harness for the browser E2E suite: base URLs, a Supabase admin client
// for hermetic auth (mint a magic link -> drive /auth/confirm -> the app sets
// its own session cookies), and small helpers.
//
// Auth tests need SUPABASE_URL + SUPABASE_SERVICE_KEY. When they're absent the
// harness reports `hasAuth() === false` and the auth suites skip, so the
// no-auth suites still run (in CI without secrets, and offline).

export const BASE_URL = (process.env.E2E_BASE_URL || 'http://localhost:8145').replace(/\/$/, '');
// The biz storefront: a real host by default, or the local server with a
// rewritten Host header when E2E_BIZ_VIA_HOST_HEADER=1.
export const BIZ_URL = (process.env.E2E_BIZ_URL || 'https://biz.athleto.store').replace(/\/$/, '');
export const BIZ_VIA_HOST_HEADER = process.env.E2E_BIZ_VIA_HOST_HEADER === '1';

const SUPABASE_URL = (process.env.SUPABASE_URL || '').replace(/\/$/, '');
const SERVICE_KEY = process.env.SUPABASE_SERVICE_KEY || '';

export function hasAuth() {
  return Boolean(SUPABASE_URL && SERVICE_KEY);
}

/** A unique test email so parallel runs / both engines never collide. */
export function testEmail(tag) {
  const rand = Math.random().toString(36).slice(2, 8);
  const engine = process.env.E2E_ENGINE || 'pw';
  return `athleto-e2e-${tag}-${engine}-${rand}@example.com`;
}

async function admin(path, init = {}) {
  const res = await fetch(`${SUPABASE_URL}/auth/v1${path}`, {
    ...init,
    headers: {
      apikey: SERVICE_KEY,
      Authorization: `Bearer ${SERVICE_KEY}`,
      'content-type': 'application/json',
      ...(init.headers || {}),
    },
  });
  return res;
}

/** Create a confirmed user and return a one-time magic-link token_hash. */
export async function mintMagicLink(email) {
  await admin('/admin/users', {
    method: 'POST',
    body: JSON.stringify({ email, email_confirm: true }),
  });
  const res = await admin('/admin/generate_link', {
    method: 'POST',
    body: JSON.stringify({ type: 'magiclink', email }),
  });
  const body = await res.json();
  if (!body.hashed_token) throw new Error(`no hashed_token: ${JSON.stringify(body)}`);
  return body.hashed_token;
}

export async function deleteUser(email) {
  const res = await admin('/admin/users?page=1&per_page=200');
  const { users = [] } = await res.json();
  const user = users.find((u) => u.email === email);
  if (user) await admin(`/admin/users/${user.id}`, { method: 'DELETE' });
}

/** The Supabase auth user id for an email (for ops endpoints keyed by user_id). */
export async function getUserId(email) {
  const res = await admin('/admin/users?page=1&per_page=200');
  const { users = [] } = await res.json();
  return users.find((u) => u.email === email)?.id ?? null;
}

/**
 * Log a browser page in as `email`: mint a link and navigate to /auth/confirm,
 * which verifies the token and sets HttpOnly session cookies, then bounces
 * through the remembered-emails interstitial. Leaves the page on the landing
 * page (home or /account/setup for a new user).
 */
export async function loginBrowser(page, email) {
  // Sign-in is pinned to the browser that started it: /login sets an
  // `athleto_login_flow` cookie and the confirm must present a matching `flow`
  // UUID. We mint a link out-of-band via the admin API, so we set the pinning
  // cookie ourselves (the server only checks the cookie == the param, not that
  // it minted it) -- hermetic, no email send, exercises the real flow guard.
  const flow = crypto.randomUUID();
  await page.setCookie({
    name: 'athleto_login_flow',
    value: flow,
    url: BASE_URL,
    httpOnly: true,
  });
  const tokenHash = await mintMagicLink(email);
  await page.goto(
    `${BASE_URL}/auth/confirm?token_hash=${tokenHash}&type=magiclink&flow=${flow}`,
    { waitUntil: 'load' },
  );
  // /auth/confirm 302s to the remembered-emails interstitial, whose script
  // forwards to the real destination. Wait for that client-side forward to
  // complete (else a caller's navigation races it -> ERR_ABORTED), then for
  // the header to settle.
  await page.waitAwayFrom('/login/remembered', { timeout: 10000 });
  await page.waitFor('header.site-header', { timeout: 10000 });
}

/** Read the readable (non-HttpOnly) CSRF token cookie inside the page. */
export async function csrfToken(page) {
  return page.evaluate(() => (document.cookie.match(/(?:^|; )athleto_csrf=([^;]+)/) || [])[1] || '');
}

/**
 * Submit a form via the browser's own submit (so the hidden csrf_token field
 * the server rendered is included). `formSelector` defaults to the first form.
 */
export async function submitForm(page, fields = {}, formSelector = 'form') {
  await page.evaluate(
    ({ sel, values }) => {
      const form = document.querySelector(sel);
      for (const [name, value] of Object.entries(values)) {
        let input = form.querySelector(`[name="${name}"]`);
        if (!input) {
          input = document.createElement('input');
          input.type = 'hidden';
          input.name = name;
          form.appendChild(input);
        }
        if (input.tagName === 'SELECT') {
          input.value = value;
        } else {
          input.value = value;
        }
      }
      form.submit();
    },
    { sel: formSelector, values: fields },
  );
}
