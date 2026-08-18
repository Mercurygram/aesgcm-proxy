<!--
SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# aesgcm-proxy

A rewrite proxy that lets [Mercurygram](https://github.com/Mercurygram/Mercurygram)
receive Telegram's **legacy WebPush** notifications through a
[UnifiedPush](https://unifiedpush.org/) distributor.

Telegram encrypts WebPush payloads with the pre-RFC `aesgcm` scheme
(`draft-ietf-webpush-encryption-04`), which carries the `Encryption` and
`Crypto-Key` parameters in **HTTP headers**. UnifiedPush distributors forward
only the request body and drop those headers, so the device can never decrypt
the payload. This proxy folds the headers into the body before forwarding, so
the client can reconstruct and decrypt the notification.

(RFC 8291 `aes128gcm` needs no such proxy — it already carries its parameters
in the body. This proxy exists solely for the legacy `aesgcm` scheme.)

## Endpoints

| Method | Path | Description |
|---|---|---|
| `POST` | `/aesgcm?e=<url>` | WebPush: serializes the `Encryption`/`Crypto-Key` headers into the body (`aesgcm\nEncryption: ...\nCrypto-Key: ...\n<ciphertext>`), forwards to the UnifiedPush endpoint, stamps a correlation-cache entry |
| `PUT` | `/<url>` | Simple Push (token_type=4): waits 200 ms for a matching POST; suppresses the wake-up if found (already delivered as an encrypted payload), else forwards the body as a synthetic wake-up |
| `POST` | `/fcm/<token>` | Same folding as `/aesgcm`, but the destination is FCM and the request is VAPID-signed here (see below) |
| `PUT` | `/fcm/<token>` | Simple Push leg of the FCM route, with the same correlation logic as `PUT /<url>` |

The correlation window (200 ms wait, 2 s cache age) prevents duplicate wake-ups
for regular messages while still letting secret-chat pushes reach the app.

## FCM leg

Google Play Services hands an app a plain WebPush endpoint without any Google
library on the client, but FCM only accepts pushes to it that carry a
[VAPID](https://www.rfc-editor.org/rfc/rfc8292) authorization, which Telegram
does not send. The `/fcm/<token>` routes sign on Telegram's behalf: the app
registers `https://<proxy>/fcm/<token>` as its endpoint and passes the proxy's
VAPID **public** key to Play Services, binding the subscription to this proxy.

Mint a keypair once:

```bash
aesgcm-proxy --generate-vapid
```

Put `VAPID_PRIVATE_KEY` in the environment of the service and the printed public
key in the client. Leaving the variable unset or empty disables `/fcm` (503) and
leaves every other route working; a malformed value aborts startup.

FCM caps a WebPush body at 4096 bytes. When the folded payload would exceed
that, the proxy sends an empty push instead, so the app still wakes and fetches
the message over MTProto rather than losing the notification to a 400.

## Building

```bash
cargo build --release
```

The default `native-tls` feature links the system OpenSSL. For a fully static
binary with no OpenSSL dependency, build with `rustls`:

```bash
cargo build --release --no-default-features --features rustls
```

## Running

Without socket activation it binds `127.0.0.1:8001`. Override with `LISTEN_ADDR`:

```bash
LISTEN_ADDR=0.0.0.0:8001 ./target/release/aesgcm-proxy
```

### systemd (production)

`aesgcm-proxy.service` + `aesgcm-proxy.socket` provide a hardened,
socket-activated deployment (the socket is passed via `listenfd`, so
`LISTEN_ADDR` is unused). Install the binary to `/usr/local/bin/aesgcm-proxy`
and enable the socket.

### Container

The image is published to `ghcr.io/mercurygram/aesgcm-proxy` for `linux/amd64`
and `linux/arm64`:

```bash
podman run --rm -p 8001:8001 ghcr.io/mercurygram/aesgcm-proxy:latest
```

Build it locally:

```bash
podman build -f Containerfile -t aesgcm-proxy .
```

CI rebuilds on every push, on a weekly cron (base-image / crate security
updates), and on manual dispatch.

## SSRF protection

- Non-http/https schemes and URLs with credentials are rejected.
- Literal IP addresses are checked before forwarding (private, loopback, CGNAT,
  link-local, ULA, NAT64 and similar ranges are blocked).
- Hostnames are filtered by `SafeResolver` (a custom reqwest DNS resolver) at
  connection time — a single resolution feeds both the safety check and the
  connection, eliminating the TOCTOU gap. Redirects are disabled.

## License

Licensed under either of [MIT](LICENSES/MIT.txt) or
[Apache-2.0](LICENSES/Apache-2.0.txt) at your option. The repository is
[REUSE](https://reuse.software/) compliant: every file carries an SPDX header
(or is covered by `REUSE.toml`).
