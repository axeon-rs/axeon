# Security Policy

Axeon is a web framework, so security problems in it can affect every
application built on it. Thank you for reporting them responsibly.

## Supported versions

Security fixes are released for the latest minor version only.

| Version | Supported |
|---|---|
| 0.5.x | Yes |
| < 0.5 | No: please upgrade |

Versions before 0.4 have known issues fixed in 0.4.0, including a form body
that could crash the server process and missing request size limits and
timeouts. See the [changelog](CHANGELOG.md).

## Reporting a vulnerability

**Please do not report security problems in public issues, discussions or
pull requests.**

Report them privately through GitHub:
[**Report a vulnerability**](https://github.com/axeon-rs/axeon/security/advisories/new)
(Security tab → *Report a vulnerability*).

Please include:

- the affected Axeon version and enabled features,
- a description of the problem and its impact,
- steps or a minimal program to reproduce it,
- any suggested fix, if you have one.

## What to expect

- We aim to acknowledge your report within 7 days.
- We will confirm the problem, work on a fix, and keep you updated on progress.
- Once a fix is released, we will publish a GitHub security advisory and, for
  issues that warrant it, a [RustSec](https://rustsec.org) advisory. We are happy
  to credit you unless you prefer to stay anonymous.

Please give us a reasonable amount of time to release a fix before disclosing
the problem publicly.

## Scope

In scope: anything in this repository that could let an attacker crash or
exhaust a server, bypass a security feature (CORS, rate limiting, request
limits, TLS), read or modify data they shouldn't, or smuggle or split HTTP
messages.

Out of scope: vulnerabilities in your own application code, and issues in
dependencies that should be reported to those projects (though we would like
to hear about them if Axeon's use of a dependency makes things worse).
