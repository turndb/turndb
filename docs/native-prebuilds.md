# Native Node prebuild and release contract

TurnDB's first native distribution target is deliberately narrow and complete:

| package target | Rust target | runtime | Node range | evidence required |
|---|---|---|---|---|
| `linux-x64-gnu` | `x86_64-unknown-linux-gnu` | Linux x86-64, glibc 2.17 or newer | `>=22 <27` | one audited addon installed and exercised on Node 22, 24, and 26 |

No other native OS, architecture, or libc is implied. Unsupported hosts receive a load error naming
their platform, architecture, libc, searched files, and optional package. TurnDB never turns a failed
native load into a WASI store with weaker writer-lock, threading, or reclamation guarantees.

The packages are release candidates but are not published merely because this pipeline exists. Both
tracked manifests and ordinary candidate tarballs remain `private`; an explicit release-pack mode is
used by the owner-gated workflow to create publishable staging tarballs. Registry publication remains
an explicit external action.

## Package graph and loader

The distribution follows the standard `napi-rs` optional-package model:

```text
@turndb/native                         platform-neutral JavaScript + TypeScript declarations
        |
        +-- optional dependency --> @turndb/native-linux-x64-gnu
                                        |
                                        +-- turndb.linux-x64-gnu.node
```

The root package is small and contains no native bytes. npm selects the platform package through its
`os`, `cpu`, and `libc` metadata. The loader checks, in order:

1. `TURNDB_NATIVE_PATH`, an explicit development override;
2. the standard local target filename, then legacy local development filenames;
3. `@turndb/native-linux-x64-gnu` on Linux x64 glibc.

Only an exact missing optional package is converted to TurnDB's diagnostic load error. A present
package whose addon fails to load is rethrown unchanged so ABI, shared-library, corruption, and
permission failures are not mislabeled as an absent prebuild.

The addon targets N-API 6. That makes one binary independent of a particular V8 ABI, but it is not a
substitute for runtime testing. CI builds the binary once and installs the exact same tarballs on all
three declared Node majors.

## Reproducible local candidate

From the repository root:

```sh
npm ci --ignore-scripts --no-audit --no-fund --prefix bindings/node
npm run package:create --prefix bindings/node
npm run prebuild:linux-x64-gnu --prefix bindings/node
npm run package:collect --prefix bindings/node
npm run package:pack --prefix bindings/node
npm run test:prebuild --prefix bindings/node
```

`package:pack` deliberately produces private tarballs. It stages the repository's exact `LICENSE`,
`NOTICE`, and generated `THIRD_PARTY_LICENSES.html`; keeps native bytes out of the root package;
requires exactly one correctly named addon in the platform package; inspects the ELF symbol versions
with `readelf`; and writes
`dist/prebuild-manifest.json`. The manifest records:

- package/version, N-API version, Rust target, and npm target;
- whether the tarballs are publishable;
- the highest required glibc symbol version;
- native binary and tarball byte counts;
- SHA-256 for the binary and both tarballs.

Set `TURNDB_MAX_GLIBC` to make packaging refuse an artifact above a chosen compatibility floor. Local
host builds commonly inherit newer glibc symbols and are useful development candidates, not release
artifacts. CI uses `--use-napi-cross` and independently requires `TURNDB_MAX_GLIBC=2.17`.

`test:prebuild` rehashes the collected tarballs, verifies the host can satisfy the recorded glibc
requirement, installs both packages into a new temporary consumer with lifecycle scripts disabled and
network access unavailable, loads the platform addon through the public root package, and exercises
an open/durable-write/health/close cycle. Temporary consumers and npm caches are always removed.

## Size profile

The `native-release` Cargo profile keeps unwinding semantics, strips symbols, uses thin LTO, and uses
one code-generation unit. On the 2026-08-03 Linux x86-64 development host, the full SQL/Arrow addon
was 77,852,768 bytes and its publishable platform tarball was 26,147,438 bytes. The ordinary stripped
release addon measured 115,467,704 bytes in the same development measurements, so the dedicated
profile removed 37,614,936 installed bytes (32.6%). After adding the compressed 332 KB third-party
attribution report, the thin publishable root tarball was 32,579 bytes.

These are local measurements, not registry sizes or performance promises. The host-built ELF required
glibc 2.34 and therefore does not qualify as the glibc-2.17 release artifact. Fat LTO was abandoned
for link time — more than nine minutes in the DataFusion-heavy final link without completing — while
clean thin-LTO builds complete in about six minutes. The artifact size is accepted because it carries
the maintained Arrow/DataFusion query machinery that the native package exposes.

## Owner-gated release

The `Release native Node package` workflow is manually dispatched with an exact annotated tag named
`vX.Y.Z`. It:

1. checks out and verifies that exact tag;
2. installs the locked `napi-rs` tooling;
3. cross-builds one glibc-2.17-compatible addon;
4. creates publishable staging tarballs and audits their contents, hashes, and ELF floor;
5. clean-installs the same artifact on Node 22, 24, and 26;
6. pauses at the protected `npm` GitHub environment;
7. uses npm trusted publishing/OIDC and provenance to publish the platform package, then the root
   selector package.

The ordinary tracked packages and candidate tarballs cannot be published accidentally.
The publish job installs npm 11.5.1 explicitly, and the release verifier rejects older clients.
`package:pack:release` is the explicit command that changes `private`, only inside staging copies.
`publish-prebuild.cjs` requires GitHub Actions and protected-job markers, a release manifest, and the
exact version tag; it also rehashes both tarballs and inspects their embedded manifests before
invoking npm. Environment markers are an accident guard, not an authorization boundary: registry
credentials and the protected GitHub environment enforce authority. A credential holder can always
deliberately bypass repository scripts with the npm CLI.

npm publication is not transactional. The platform package is published first so the visible root
package can never name a dependency that does not exist. If the second publication fails, an
installable but undiscoverable platform package may remain. The workflow deliberately does not
unpublish or rewrite versions; an owner decides how to recover.

Before the first release, repository owners must configure all external facts the source tree cannot:

- create or confirm control of the `@turndb` npm scope and both package names;
- configure npm trusted publishers for this repository and workflow;
- create a protected GitHub environment named `npm` with required reviewer approval;
- confirm `LICENSE`, `NOTICE`, descriptions, repository URLs, and release notes;
- run the ordinary CI and native release workflow green at the exact tag.

The third-party report is generated from the locked, all-feature Linux native dependency graph with
`cargo-about 0.9.1`. `scripts/check-third-party-licenses.sh` regenerates it offline and byte-compares
the result; dependency or license changes cannot silently leave the shipped attribution stale.

The package version is not the on-disk format version. A `0.x` consumer must follow the compatibility
policy: a minor release may advance the writer format or make a documented API break, and downgrade
writing is not promised. This is suitable for an early production integration without pretending
the format or API has reached the 1.0 freeze.

The candidate's consumer-facing scope and known limits are collected in the
[native 0.1.0 release notes](releases/native-0.1.0.md).
