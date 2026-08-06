# Support

Thanks for looking at TurnDB. Where to go for what:

- **Docs** — the in-repo [`docs/`](docs/) tree, plus
  [`FORMAT.md`](FORMAT.md) for the on-disk format and
  [`CONTRIBUTING.md`](CONTRIBUTING.md) for the development loop.
  There is no published API documentation yet: `docs.rs/turndb` will
  exist when the crate is published, and it is not.
- **Bugs** — file a
  [bug report](https://github.com/turndb/turndb/issues/new?template=bug_report.yml).
  Please search existing issues first, and include the commit you built
  from — there are no releases to name instead.
- **Feature requests** — open a
  [feature request](https://github.com/turndb/turndb/issues/new?template=feature_request.yml).
- **Security vulnerabilities** — do **not** open a public issue. Follow
  [SECURITY.md](SECURITY.md) (email security@efficacious.io).
- **Conduct concerns** — see the [Code of Conduct](CODE_OF_CONDUCT.md)
  (conduct@efficacious.io).

**TurnDB is pre-release.** The crate is unpublished, the npm packages are
unpublished, and every artifact is built from source at a commit. There
is no supported version, no upgrade path between releases, and the
on-disk format is not frozen. Please be specific about the commit,
platform, and which binding you are using — native and portable WASI
differ in ways that matter, and "TurnDB does X" is often true of one and
not the other.
