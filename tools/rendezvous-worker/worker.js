// Seedwell rendezvous: a small address book on the user's own Cloudflare account (SPEC §7G.3).
//
// Each Seedwell device keeps one record here: where it can be reached now, sealed (ChaCha20-Poly1305) with a key only
// devices that know it can work out, and signed inside the seal. This Worker sees opaque bytes under a hash; it never
// sees a device key, an address, a folder or a file. Records are small, live 7 days after their last write, and only
// the key that wrote a record first may change it (trust on first use).
//
// API v1 (docs/PROTOCOL.md, "Rendezvous"):
//
//   GET    /v1                 -> 200 {"service":"seedwell-rendezvous","api":1,"max_record":1024,"ttl_days":7}
//   GET    /v1/r/<id>          -> 200 the record (application/octet-stream; header Seedwell-Time) | 404
//   PUT    /v1/r/<id>          -> 204 | 400 | 401 | 403 | 409 | 413 | 429   body: the sealed record
//   DELETE /v1/r/<id>          -> 204 | 400 | 401 | 403 | 409 | 429
//
//   <id> = 64 lower-case hex digits. PUT and DELETE carry three headers:
//     Seedwell-Key:       the write key, an Ed25519 public key (64 hex digits)
//     Seedwell-Time:      Unix milliseconds, within 5 minutes of the Worker's clock, and later than the last write's
//     Seedwell-Signature: Ed25519 by the write key (128 hex digits) over the UTF-8 text
//                         "seedwell-rendezvous/v1\n" + method + "\n" + id + "\n" + time + "\n" + hex(SHA-256(body))
//
// One Durable Object per id (SQLite storage, which the free plan includes) keeps the record, its write key and the
// last write's time; an alarm deletes it when it expires. Reads and writes per id are limited per hour.
//
// No imports and no build step: Seedwell uploads this file as it is (Settings > Folder Sync), and
// `npx wrangler deploy` in this folder does the same.

const API = 1;
const MAX_RECORD = 1024;
const TTL_MS = 7 * 24 * 3600 * 1000;
const SKEW_MS = 5 * 60 * 1000;
const WRITES_PER_HOUR = 120;
const READS_PER_HOUR = 1200;
const ID = /^[0-9a-f]{64}$/;
const HEX64 = /^[0-9a-f]{64}$/;
const HEX128 = /^[0-9a-f]{128}$/;

function json(status, value) {
  return new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });
}

function fail(status, error) {
  return json(status, { error });
}

function hex(bytes) {
  return [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function unhex(s) {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/v1" || url.pathname === "/v1/") {
      if (request.method !== "GET") return fail(405, "method not allowed");
      return json(200, { service: "seedwell-rendezvous", api: API, max_record: MAX_RECORD, ttl_days: TTL_MS / 86400000 });
    }
    const m = url.pathname.match(/^\/v1\/r\/([^/]+)$/);
    if (!m) return fail(404, "not found");
    const id = m[1];
    if (!ID.test(id)) return fail(400, "the id is 64 lower-case hex digits");
    if (!["GET", "PUT", "DELETE"].includes(request.method)) return fail(405, "method not allowed");
    const length = Number(request.headers.get("content-length") || "0");
    if (length > MAX_RECORD) return fail(413, `a record is at most ${MAX_RECORD} bytes`);
    const stub = env.RENDEZVOUS.get(env.RENDEZVOUS.idFromName(id));
    return stub.fetch(request);
  },
};

// One record. Plain Durable Object class (no import of cloudflare:workers), so the same file runs in tests.
export class Rendezvous {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
    this.sql = ctx.storage.sql;
    this.sql.exec(
      "CREATE TABLE IF NOT EXISTS record (one INTEGER PRIMARY KEY CHECK (one = 1), write_key TEXT NOT NULL, time INTEGER NOT NULL, data BLOB, expires INTEGER NOT NULL)",
    );
    // Rate counters live in memory: they cost no row writes, and a fresh instance starts them again.
    this.hour = -1;
    this.reads = 0;
    this.writes = 0;
  }

  now() {
    return Date.now();
  }

  // Counts one read or write in this hour; false when over the limit.
  admit(write) {
    const hour = Math.floor(this.now() / 3600000);
    if (hour !== this.hour) {
      this.hour = hour;
      this.reads = 0;
      this.writes = 0;
    }
    if (write) return ++this.writes <= WRITES_PER_HOUR;
    return ++this.reads <= READS_PER_HOUR;
  }

  row() {
    const rows = this.sql.exec("SELECT write_key, time, data, expires FROM record WHERE one = 1").toArray();
    const r = rows[0];
    if (!r) return null;
    if (r.expires <= this.now()) {
      this.sql.exec("DELETE FROM record");
      return null;
    }
    return r;
  }

  async fetch(request) {
    const id = new URL(request.url).pathname.split("/").pop();
    if (request.method === "GET") {
      if (!this.admit(false)) return fail(429, "too many reads of this record; try again later");
      const r = this.row();
      if (!r || r.data === null || r.data === undefined) return fail(404, "no record");
      return new Response(new Uint8Array(r.data), {
        status: 200,
        headers: { "content-type": "application/octet-stream", "seedwell-time": String(r.time), "cache-control": "no-store" },
      });
    }
    if (!this.admit(true)) return fail(429, "too many writes to this record; try again later");
    const body = new Uint8Array(await request.arrayBuffer());
    if (body.length > MAX_RECORD) return fail(413, `a record is at most ${MAX_RECORD} bytes`);
    if (request.method === "PUT" && body.length === 0) return fail(400, "an empty record");
    const key = (request.headers.get("seedwell-key") || "").toLowerCase();
    const sig = (request.headers.get("seedwell-signature") || "").toLowerCase();
    const timeText = request.headers.get("seedwell-time") || "";
    if (!HEX64.test(key) || !HEX128.test(sig) || !/^[0-9]{1,16}$/.test(timeText)) {
      return fail(400, "Seedwell-Key, Seedwell-Time and Seedwell-Signature are required");
    }
    const time = Number(timeText);
    const now = this.now();
    if (Math.abs(now - time) > SKEW_MS) return fail(409, "the time is more than 5 minutes off");
    const digest = hex(await crypto.subtle.digest("SHA-256", body));
    const signed = new TextEncoder().encode(`seedwell-rendezvous/v1\n${request.method}\n${id}\n${timeText}\n${digest}`);
    let ok = false;
    try {
      const pk = await crypto.subtle.importKey("raw", unhex(key), { name: "Ed25519" }, false, ["verify"]);
      ok = await crypto.subtle.verify({ name: "Ed25519" }, pk, unhex(sig), signed);
    } catch (e) {
      ok = false;
    }
    if (!ok) return fail(401, "the signature does not verify");
    const r = this.row();
    if (r && r.write_key !== key) return fail(403, "this record belongs to another write key");
    if (r && time <= r.time) return fail(409, "not newer than the last write (a replay?)");
    const expires = now + TTL_MS;
    if (request.method === "DELETE") {
      if (!r) return new Response(null, { status: 204 });
      // The key and the time stay until it expires, so the delete cannot be undone by replaying the last write.
      this.sql.exec("UPDATE record SET data = NULL, time = ?, expires = ? WHERE one = 1", time, expires);
    } else {
      this.sql.exec(
        "INSERT INTO record (one, write_key, time, data, expires) VALUES (1, ?, ?, ?, ?) ON CONFLICT(one) DO UPDATE SET time = excluded.time, data = excluded.data, expires = excluded.expires",
        key,
        time,
        body.buffer,
        expires,
      );
    }
    await this.ctx.storage.setAlarm(expires);
    return new Response(null, { status: 204 });
  }

  async alarm() {
    const r = this.sql.exec("SELECT expires FROM record WHERE one = 1").toArray()[0];
    if (!r) return;
    if (r.expires <= this.now()) {
      this.sql.exec("DELETE FROM record");
    } else {
      await this.ctx.storage.setAlarm(r.expires);
    }
  }
}
