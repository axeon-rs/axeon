# axeon-webhooks

Signed webhooks for [Axeon](https://docs.rs/axeon) apps: HMAC-SHA256
signatures with a timestamp, so receivers know a request came from you,
wasn't changed on the way, and isn't an old one replayed.

The header is the widely used `t=<unix seconds>,v1=<hex>` (as Stripe's),
where `v1` is HMAC-SHA256 of `"<t>.<body>"` with a shared secret, so
customers' existing tooling can check it.

## Sending

```rust
use axeon_webhooks::Signer;

let signer = Signer::new(&endpoint.secret);            // at least 32 random bytes
let body = serde_json::to_vec(&event)?;
let response = client
    .post(&endpoint.url)
    .header("X-VS-Signature", signer.sign_now(&body))   // sign the exact bytes you send
    .header("Content-Type", "application/json")
    .body(body)
    .send()
    .await?;
```

Deliver from a background job (axeon-jobs), so retries with backoff come
for free.

## Receiving

```rust
use axeon_webhooks::{Verifier, VerifyWebhook};

app.route("/webhooks/replication")
    .middleware(VerifyWebhook::new("X-VS-Signature", Verifier::new(&config.webhook_secret)))
    .post(ingest);   // only verified requests get here
```

- Signatures are compared in constant time.
- Timestamps more than 5 minutes from now, either way, are rejected
  (`tolerance` to change it). Pair this with idempotency keys or event IDs
  to drop duplicates inside the window.
- Rejected requests get `401` with the reason (missing, malformed,
  outside the tolerance, mismatch).
- `Verifier::verify` works without Axeon, e.g. in a worker.

## Rotating secrets

1. Add the new secret to receivers: `Verifier::new(new).secret(old)`.
2. Switch senders to the new secret (or send both signatures for a
   while with `Signer::sign_with`).
3. Remove the old secret.

## License

MIT
