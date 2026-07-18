// Ops B2B approval endpoint. A business account is gated off ordering / the
// ERP API / API keys until ops approves it via
//   POST /api/v1/ops/customers/{user_id}/approval
// (gated by the operations credential). Needs SUPABASE_URL + SUPABASE_SERVICE_KEY
// and E2E_OPS_KEY (== the app's ATHLETO_OPERATIONS_API_KEY); skips otherwise.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { Driver } from './lib/driver.mjs';
import {
  BASE_URL,
  hasAuth,
  testEmail,
  loginBrowser,
  deleteUser,
  getUserId,
} from './lib/harness.mjs';

const OPS = process.env.E2E_OPS_KEY || '';
const skip = !hasAuth() ? 'SUPABASE not set' : !OPS ? 'E2E_OPS_KEY not set' : false;

async function approve(userId, body, key) {
  return fetch(`${BASE_URL}/api/v1/ops/customers/${userId}/approval`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(key ? { authorization: `Bearer ${key}` } : {}),
    },
    body: JSON.stringify(body),
  });
}

let driver;
const created = [];
before(async () => {
  if (skip) return;
  driver = await Driver.launch();
});
after(async () => {
  await driver?.close();
  for (const email of created) await deleteUser(email).catch(() => {});
});

test(`[${Driver.engine()}] ops approval: unauth 401, unknown 404, approve/revoke round-trip`, { skip }, async () => {
  // No key -> 401.
  assert.equal((await approve('00000000-0000-0000-0000-000000000000', {}, null)).status, 401);
  // Valid key, no such B2B account -> 404.
  assert.equal((await approve(crypto.randomUUID(), {}, OPS)).status, 404);

  // Stand up a real B2B account.
  const email = testEmail('approve');
  created.push(email);
  const page = await driver.newPage();
  try {
    await loginBrowser(page, email);
    await page.goto(`${BASE_URL}/account/setup`);
    await page.waitFor('input[name="customer_type"][value="b2b"]');
    await page.click('input[name="customer_type"][value="b2b"]'); // check the B2B radio
    await page.fill('input[name="company_name"]', 'Approval E2E Co');
    await page.click('form[action="/account/setup"] button[type="submit"]'); // server-rendered csrf field rides along
    await page.waitAwayFrom('/account/setup', { timeout: 10000 });
  } finally {
    await page.close();
  }
  const userId = await getUserId(email);
  assert.ok(userId, 'created a user id');

  // Approve -> approved:true.
  const r1 = await approve(userId, { approved: true }, OPS);
  assert.equal(r1.status, 200);
  assert.equal((await r1.json()).approved, true);

  // Idempotent re-approve stays true.
  assert.equal((await (await approve(userId, { approved: true }, OPS)).json()).approved, true);

  // Revoke -> approved:false.
  const r2 = await approve(userId, { approved: false }, OPS);
  assert.equal(r2.status, 200);
  assert.equal((await r2.json()).approved, false);
});
