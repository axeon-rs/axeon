# Changelog

## Unreleased

First release: `Signer` (HMAC-SHA256 over `"<timestamp>.<body>"`, as a
`t=..,v1=..` header, with several signatures for secret rotation),
`Verifier` (constant-time check, timestamp tolerance against replays,
several secrets), and `VerifyWebhook`, Axeon middleware that answers
unsigned or badly signed requests with 401.
