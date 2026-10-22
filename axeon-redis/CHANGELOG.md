# Changelog

## Unreleased

### Added

- `RedisIdempotencyStore`: Axeon `Idempotency-Key` records shared by
  every instance.
- `RedisRateLimitStore`: Axeon `RateLimiter` state shared by every
  instance (GCRA and fixed windows in one Lua call, Redis' clock, hashed
  keys that expire).

### Fixed

- With the `tls` feature, connecting no longer panics when several rustls
  crypto providers are compiled in: aws-lc-rs is installed as the process
  default if none is set.

## 0.1.0 (2026-10-22)

First release: `RedisBroadcast`, a backend for Axeon's `broadcast::Hub`
on Redis Streams (about 1000 messages kept per topic, idle topics expire
after a day) with pub/sub wake-ups; and `RedisSessionStore`, a `SessionStore` for Axeon sessions in
Redis (hashed keys, TTL expiry, any connection type), and `connect` for an
auto-reconnecting `ConnectionManager`. Feature `tls` for `rediss://`.
