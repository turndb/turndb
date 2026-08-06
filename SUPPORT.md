# Support

Thanks for looking at TurnDB. Where to go for what:

- **Docs** — the in-repo [`docs/`](docs/) tree, plus
  [`FORMAT.md`](FORMAT.md) for the on-disk format and
  [`CONTRIBUTING.md`](CONTRIBUTING.md) for the development loop. API
  documentation for a published crate is at `docs.rs/turndb`.
- **Bugs** — file a
  [bug report](https://github.com/turndb/turndb/issues/new?template=bug_report.yml).
  Please search existing issues first.
- **Feature requests** — open a
  [feature request](https://github.com/turndb/turndb/issues/new?template=feature_request.yml).
- **Security vulnerabilities** — do **not** open a public issue. Follow
  [SECURITY.md](SECURITY.md) (email security@efficacious.io).
- **Conduct concerns** — see the [Code of Conduct](CODE_OF_CONDUCT.md)
  (conduct@efficacious.io).

**Identify what you ran.** Give the package version if you installed a
published artifact, or the commit if you built from source — whichever
you actually used. **And say which binding:** native Rust, the native
Node addon, and the portable WASI build differ in ways that decide the
answer, so "TurnDB does X" is often true of one and not the others.

**Check the release and compatibility state rather than assuming it.**
[`README.md`](README.md) carries the current status line and
[`FORMAT.md`](FORMAT.md) is normative on the on-disk format; where this
page and those disagree, they are right.
