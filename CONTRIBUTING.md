# Contributing to Axeon

Thanks for helping improve Axeon! Bug reports, fixes, docs, examples and new
features are all welcome.

## Ways to contribute

- **Report a bug**: [open an issue](https://github.com/axeon-rs/axeon/issues/new/choose)
  with the Axeon version, a minimal example and what you expected to happen.
- **Suggest a feature**: open a feature request describing the problem you
  want solved, not only the solution you have in mind.
- **Fix something**: issues labelled `good first issue` or `help wanted` are a
  good place to start. For larger changes, open an issue first so we can agree
  on the approach before you spend time on it.
- **Report a security problem**: please don't open a public issue; follow
  [SECURITY.md](SECURITY.md) instead.

## Development setup

You need a recent stable Rust toolchain ([rustup](https://rustup.rs)).

```bash
git clone https://github.com/axeon-rs/axeon.git
cd axeon
cargo test --all-features
```

## Before you open a pull request

Run the same checks as CI:

```bash
cargo test --all-features                          # unit, integration and doc tests
cargo test --no-default-features                   # the crate without optional features
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

If your change touches an optional feature (`tls`, `compression`, `ws`,
`cache`), also run `cargo test --no-default-features --features <name>`.

For performance-sensitive changes (routing, middleware, request handling),
compare `cargo bench` before and after and mention the results in the PR.

## Guidelines

- **Tests**: every bug fix needs a test that fails without the fix, and every
  feature needs tests for its behaviour. Integration tests live in `tests/`;
  `tests/common` has a raw HTTP client for checking what goes over the wire,
  and `Server::into_app()` runs requests in-process.
- **Docs**: document public items. Examples in doc comments and in
  `README.md` are compiled by `cargo test`, so keep them complete and working.
- **Changelog**: add a line to the unreleased section of
  [CHANGELOG.md](CHANGELOG.md) for anything users will notice. Call out
  breaking changes and how to migrate.
- **Style**: match the surrounding code. Keep clippy clean, prefer clear names
  over comments, and comment only what the code can't say itself.
- **Dependencies**: new dependencies need a good reason. If one is only needed
  by an optional part of the crate, put it behind a feature.
- **Scope**: keep pull requests focused on one change; unrelated refactors are
  easier to review separately.

## Pull requests

1. Fork the repository and create a branch from `master`.
2. Make your change with tests and docs.
3. Run the checks above.
4. Open a pull request and fill in the template. Link the issue it fixes
   (`Fixes #123`).

A maintainer will review it. Please be patient: this is a volunteer project.
CI must pass before a pull request is merged.

## License

Axeon is licensed under the [MIT License](LICENSE). By contributing, you agree
that your contributions are licensed under the same terms.

## Code of conduct

Everyone taking part is expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md).
