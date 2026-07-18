// Shared harness helper for the browser suites.
//
// When APP_BASE_URL is set (CI service container or the k8s cluster), the
// suites drive that URL directly and this module spawns nothing. Otherwise it
// builds the Rust binary once, launches it in degraded no-secrets mode on an
// ephemeral port, waits for GET /healthz to answer 200, and hands back a base
// URL plus a teardown that kills the child.

import assert from "node:assert/strict";
import net from "node:net";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
// tests/browser -> repo root is two levels up.
const REPO_ROOT = resolve(HERE, "..", "..");

function normalize(url) {
  return url.replace(/\/+$/, "");
}

async function findOpenPort() {
  return await new Promise((resolvePort, reject) => {
    const srv = net.createServer();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      if (!address || typeof address !== "object") {
        reject(new Error("failed to read bound address"));
        return;
      }
      srv.close(() => resolvePort(address.port));
    });
  });
}

async function waitForHealth(base, { timeoutMs = 60_000, intervalMs = 250 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastErr;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${base}/healthz`);
      if (res.status === 200 && (await res.text()) === "ok") {
        return;
      }
      lastErr = new Error(`healthz status ${res.status}`);
    } catch (err) {
      lastErr = err;
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  throw new Error(`server never became healthy at ${base}: ${lastErr}`);
}

/**
 * Start (or attach to) an app instance.
 * @returns {Promise<{base: string, stop: () => Promise<void>}>}
 */
export async function startServer() {
  const external = process.env.APP_BASE_URL;
  if (external && external.trim()) {
    const base = normalize(external.trim());
    await waitForHealth(base);
    return { base, stop: async () => {} };
  }

  // Build the binary once (a no-op when already built).
  const build = spawnSync("cargo", ["build", "--bin", "athleto-app-rs"], {
    cwd: REPO_ROOT,
    stdio: "inherit",
  });
  assert.equal(build.status, 0, "cargo build must succeed");

  const port = await findOpenPort();
  const bin = resolve(REPO_ROOT, "target", "debug", "athleto-app-rs");

  // Curated env: force degraded mode by NOT inheriting any DATABASE_URL /
  // SUPABASE / FIDUCIA / ATHLETO_* secrets the workstation may have exported.
  const child = spawn(bin, [], {
    cwd: REPO_ROOT,
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      HOST: "127.0.0.1",
      PORT: String(port),
      RUST_LOG: "warn",
    },
    stdio: ["ignore", "inherit", "inherit"],
  });

  child.on("error", (err) => {
    console.error(`[server] failed to spawn app binary: ${err}`);
  });

  const base = `http://127.0.0.1:${port}`;
  const stop = async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGTERM");
      await new Promise((r) => setTimeout(r, 200));
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGKILL");
      }
    }
  };

  try {
    await waitForHealth(base);
  } catch (err) {
    await stop();
    throw err;
  }
  return { base, stop };
}
