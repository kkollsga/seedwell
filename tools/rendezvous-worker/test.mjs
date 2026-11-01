// Tests of worker.js against a small local fake of the Workers runtime: `node --test` in this folder (Node 22.5 or
// later, for node:sqlite and Ed25519 in WebCrypto). Skips cleanly where those are missing. Nothing goes to the network.

import { test } from "node:test";
import assert from "node:assert/strict";

let sqlite = null;
try {
  sqlite = await import("node:sqlite");
} catch {
  sqlite = null;
}
const ed25519 = await crypto.subtle
  .generateKey({ name: "Ed25519" }, true, ["sign", "verify"])
  .then(() => true)
  .catch(() => false);
const skip = !sqlite || !ed25519 ? "needs node:sqlite and Ed25519 WebCrypto (Node 22.5 or later)" : false;

const worker = await import("./worker.js");

// A Durable Object's SQL storage on node:sqlite (exec(query, ...bindings) -> cursor with toArray()).
function fakeStorage() {
  const db = new sqlite.DatabaseSync(":memory:");
  const storage = {
    alarm: null,
    sql: {
      exec(query, ...bindings) {
        const args = bindings.map((b) => (b instanceof ArrayBuffer ? new Uint8Array(b) : b));
        const st = db.prepare(query);
        const rows = /^\s*SELECT/i.test(query) ? st.all(...args) : (st.run(...args), []);
        return { toArray: () => rows, one: () => rows[0] };
      },
    },
    async setAlarm(t) {
      storage.alarm = t;
    },
  };
  return storage;
}

// A namespace that keeps one object per name, like env.RENDEZVOUS.
function fakeEnv(clock) {
  const objects = new Map();
  const ns = {
    idFromName: (name) => name,
    get(name) {
      if (!objects.has(name)) {
        const storage = fakeStorage();
        const o = new worker.Rendezvous({ storage }, {});
        o.now = () => clock.now;
        objects.set(name, { o, storage });
      }
      const { o } = objects.get(name);
      return { fetch: (req) => o.fetch(req) };
    },
  };
  return { env: { RENDEZVOUS: ns }, objects };
}

const hex = (b) => [...new Uint8Array(b)].map((x) => x.toString(16).padStart(2, "0")).join("");

async function writer() {
  const k = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
  const pk = hex(await crypto.subtle.exportKey("raw", k.publicKey));
  return { k, pk };
}

async function signed(w, method, id, body, time) {
  const digest = hex(await crypto.subtle.digest("SHA-256", body));
  const msg = new TextEncoder().encode(`seedwell-rendezvous/v1\n${method}\n${id}\n${time}\n${digest}`);
  const sig = hex(await crypto.subtle.sign({ name: "Ed25519" }, w.k.privateKey, msg));
  return new Request(`https://rv.example.workers.dev/v1/r/${id}`, {
    method,
    body: method === "GET" ? undefined : body,
    headers: { "seedwell-key": w.pk, "seedwell-time": String(time), "seedwell-signature": sig },
  });
}

const ID = "ab".repeat(32);
const get = (env, id = ID) => worker.default.fetch(new Request(`https://rv.example.workers.dev/v1/r/${id}`), env);

test("a record is written, read back, and only its first write key may change it", { skip }, async () => {
  const clock = { now: 1_800_000_000_000 };
  const { env } = fakeEnv(clock);
  const a = await writer();
  const b = await writer();
  assert.equal((await get(env)).status, 404);
  const put = await worker.default.fetch(await signed(a, "PUT", ID, new Uint8Array([1, 2, 3]), clock.now), env);
  assert.equal(put.status, 204);
  const r = await get(env);
  assert.equal(r.status, 200);
  assert.deepEqual(new Uint8Array(await r.arrayBuffer()), new Uint8Array([1, 2, 3]));
  assert.equal(r.headers.get("seedwell-time"), String(clock.now));
  // Another key: refused (trust on first use).
  clock.now += 1000;
  const other = await worker.default.fetch(await signed(b, "PUT", ID, new Uint8Array([9]), clock.now), env);
  assert.equal(other.status, 403);
  // The first key again: accepted.
  const again = await worker.default.fetch(await signed(a, "PUT", ID, new Uint8Array([4]), clock.now), env);
  assert.equal(again.status, 204);
  assert.deepEqual(new Uint8Array(await (await get(env)).arrayBuffer()), new Uint8Array([4]));
  // Delete: gone, and the key still holds it.
  clock.now += 1000;
  assert.equal((await worker.default.fetch(await signed(a, "DELETE", ID, new Uint8Array(), clock.now), env)).status, 204);
  assert.equal((await get(env)).status, 404);
  clock.now += 1000;
  assert.equal((await worker.default.fetch(await signed(b, "PUT", ID, new Uint8Array([7]), clock.now), env)).status, 403);
});

test("a replayed or stale write is refused, and a bad signature too", { skip }, async () => {
  const clock = { now: 1_800_000_000_000 };
  const { env } = fakeEnv(clock);
  const a = await writer();
  const first = await signed(a, "PUT", ID, new Uint8Array([1]), clock.now);
  const replay = first.clone();
  assert.equal((await worker.default.fetch(first, env)).status, 204);
  clock.now += 10;
  assert.equal((await worker.default.fetch(replay, env)).status, 409, "the same write again");
  const old = await signed(a, "PUT", ID, new Uint8Array([2]), clock.now - 6 * 60 * 1000);
  assert.equal((await worker.default.fetch(old, env)).status, 409, "more than 5 minutes off");
  const tampered = await signed(a, "PUT", ID, new Uint8Array([3]), clock.now);
  const body = new Uint8Array([4]);
  const forged = new Request(tampered.url, { method: "PUT", body, headers: tampered.headers });
  assert.equal((await worker.default.fetch(forged, env)).status, 401, "the body is not the signed one");
  const otherId = await signed(a, "PUT", "cd".repeat(32), new Uint8Array([5]), clock.now);
  const moved = new Request(`https://rv.example.workers.dev/v1/r/${ID}`, { method: "PUT", body: new Uint8Array([5]), headers: otherId.headers });
  assert.equal((await worker.default.fetch(moved, env)).status, 401, "signed for another id");
  const bare = new Request(`https://rv.example.workers.dev/v1/r/${ID}`, { method: "PUT", body: new Uint8Array([6]) });
  assert.equal((await worker.default.fetch(bare, env)).status, 400);
  assert.deepEqual(new Uint8Array(await (await get(env)).arrayBuffer()), new Uint8Array([1]), "the first record stays");
});

test("records are capped in size and expire after 7 days", { skip }, async () => {
  const clock = { now: 1_800_000_000_000 };
  const { env, objects } = fakeEnv(clock);
  const a = await writer();
  const big = await signed(a, "PUT", ID, new Uint8Array(1025), clock.now);
  assert.equal((await worker.default.fetch(big, env)).status, 413);
  assert.equal((await worker.default.fetch(await signed(a, "PUT", ID, new Uint8Array(1024), clock.now), env)).status, 204);
  const { o, storage } = objects.get(ID);
  assert.equal(storage.alarm, clock.now + 7 * 86400000, "an alarm at the expiry");
  clock.now += 7 * 86400000 - 1;
  assert.equal((await get(env)).status, 200);
  clock.now += 1;
  await o.alarm();
  assert.equal((await get(env)).status, 404, "expired");
  // Expired: the id is free for a first write again.
  const b = await writer();
  assert.equal((await worker.default.fetch(await signed(b, "PUT", ID, new Uint8Array([1]), clock.now), env)).status, 204);
});

test("reads and writes per id are limited per hour, and the API answers its version", { skip }, async () => {
  const clock = { now: 1_800_000_000_000 };
  const { env } = fakeEnv(clock);
  const a = await writer();
  let last = 0;
  for (let i = 0; i < 121; i++) {
    clock.now += 1;
    last = (await worker.default.fetch(await signed(a, "PUT", ID, new Uint8Array([i]), clock.now), env)).status;
  }
  assert.equal(last, 429, "the 121st write in an hour");
  for (let i = 0; i < 1201; i++) last = (await get(env)).status;
  assert.equal(last, 429, "the 1201st read in an hour");
  clock.now += 3600000;
  assert.equal((await get(env)).status, 200, "a new hour");
  const info = await worker.default.fetch(new Request("https://rv.example.workers.dev/v1"), env);
  assert.deepEqual(await info.json(), { service: "seedwell-rendezvous", api: 1, max_record: 1024, ttl_days: 7 });
  assert.equal((await get(env, "XYZ")).status, 400);
  assert.equal((await worker.default.fetch(new Request("https://rv.example.workers.dev/other"), env)).status, 404);
});
