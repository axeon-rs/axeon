# Changelog

## 0.1.0 (unreleased)

First release: `RedisBroadcast`, a backend for Axeon's `broadcast::Hub`
on Redis Streams (about 1000 messages kept per topic, idle topics expire
after a day) with pub/sub wake-ups; and `RedisSessionStore`, a `SessionStore` for Axeon sessions in
Redis (hashed keys, TTL expiry, any connection type), and `connect` for an
auto-reconnecting `ConnectionManager`. Feature `tls` for `rediss://`.
