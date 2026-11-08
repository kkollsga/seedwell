# Seedwell rendezvous Worker

A small address book on your own Cloudflare account, so your Seedwell devices find each other again after one of
them moves to another network (SPEC §7G.3). Each device keeps one record here: where it can be reached now,
**sealed** with a key that only devices that know it can work out, and signed by it inside the seal. Cloudflare sees
opaque bytes under a hash: no device keys, no addresses, no folders, no files, no chats. File data and chat content
never go through it. When it is down, Seedwell finds devices the other ways (local network, DHT, relays).

The free Workers plan is enough: one Durable Object per device, with SQLite storage (the kind the free plan
includes), a record of under 200 bytes written every 30 minutes and on network changes, and lookups only while a
device is away.

## Set it up from Seedwell (no Node, no wrangler)

Settings › Folder Sync › **Find your devices over the Internet (Cloudflare)**. Seedwell uploads `worker.js`
itself through the Cloudflare API with an API token you make once:

1. Open <https://dash.cloudflare.com/profile/api-tokens> (log in, or make a free account first).
2. **Create Token**.
3. Next to **Edit Cloudflare Workers**, press **Use template**.
4. **Account Resources**: Include, your account. **Zone Resources**: Include, All zones.
5. **Continue to summary**, then **Create Token**.
6. **Copy** the token, paste it into Seedwell and press **Set Up**.

Seedwell checks the token, finds your account, uploads the Worker (`seedwell-rendezvous`, with its Durable Object
binding and the `new_sqlite_classes` migration), turns on its `workers.dev` address (making your account's
workers.dev subdomain if it has none), reads the address from the API, tests it (a record written, read back and
deleted) and keeps the address. The token is used for these calls only: it is never written to disk or to a log,
and Seedwell asks for one again for a later update of the Worker. You may delete the token in the dashboard
afterwards.

Your other devices: paste the same address (Copy, then on the other device "Use an existing address").

## Deploy it yourself (developers)

```sh
cd tools/rendezvous-worker
npx wrangler login
npx wrangler deploy
```

`wrangler deploy` prints the address (`https://seedwell-rendezvous.<your subdomain>.workers.dev`); take it from
there, never make it up from your login. Paste it in Settings › Folder Sync › "Use an existing address". A headless
daemon reads it from `sync.toml`: `[settings] rendezvous = "https://…"`.

An agent deploys it this way only when the user explicitly allows it for that deploy (AGENTS.md: an outward-facing
step).

## Tests

```sh
cd tools/rendezvous-worker && node --test
```

`test.mjs` runs `worker.js` against a small local fake of the Workers runtime (Node 22.5 or later for
`node:sqlite` and Ed25519 in WebCrypto; it skips without them): first-key ownership, replays, the size cap, the
expiry and the rate limits. Nothing goes to the network. The Rust side's tests use a loopback fake of the same API
(`seedwell-rendezvous`'s `testkit`).

## API v1

| Request | Answer |
|---|---|
| `GET /v1` | `200 {"service":"seedwell-rendezvous","api":1,"max_record":1024,"ttl_days":7}` |
| `GET /v1/r/<id>` | `200` the record (`application/octet-stream`, header `Seedwell-Time`), or `404` |
| `PUT /v1/r/<id>` (body: the record) | `204`; `400` headers missing, `401` bad signature, `403` another write key, `409` time more than 5 min off or not later than the last write, `413` over 1024 bytes, `429` over the hourly limit |
| `DELETE /v1/r/<id>` | `204`, or as `PUT` |

`<id>` is 64 lower-case hex digits (the SHA-256 of the device id with a context, docs/PROTOCOL.md). Writes carry:

- `Seedwell-Key`: the write key, an Ed25519 public key in hex. A random key per device per rendezvous, kept in the
  device's secret store; never the device key. The first write of a record registers it (trust on first use); later
  writes must use it.
- `Seedwell-Time`: Unix milliseconds; within 5 minutes of the Worker's clock and later than the last write's (no
  replays).
- `Seedwell-Signature`: the write key's Ed25519 signature, in hex, over
  `"seedwell-rendezvous/v1\n" + method + "\n" + id + "\n" + time + "\n" + hex(SHA-256(body))`.

Reads need nothing: the records are sealed. A record lives 7 days after its last write (an alarm deletes it, with
its write key, so after a week of silence the id can be taken again; the record inside is still signed by the device,
so a newcomer cannot forge one). Each id takes 120 writes and 1200 reads an hour.
