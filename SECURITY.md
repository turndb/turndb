# Security

## Reporting a vulnerability

Please do **not** open a public issue for a security vulnerability.

Report privately by email to **security@efficacious.io**. That is the
primary channel and it always works.

GitHub's [private vulnerability
reporting](https://github.com/turndb/turndb/security/advisories/new) is
also available. If that page is not reachable, the setting has not been
armed yet — use the email address above rather than waiting or filing
publicly.

Useful reports include the affected commit, the platform and binding
(native Rust, native Node addon, or portable WASI), and either a
reproduction or a store directory / pack file that triggers the
behaviour. If a store reproduces it, say so rather than attaching it
until we have agreed a channel — an adversarial store is the payload.

**We aim to acknowledge within 72 hours.** We do not commit to a patch
window. TurnDB has no published release: there is no artifact in a
registry to patch, and no supported version to patch it against. When
the crate and package are published, this section gets a concrete
remediation commitment and not before — a window we have never had to
meet is not a commitment, it is a decoration.

If the issue is in a dependency rather than in TurnDB, please still tell
us so we can pin, patch or fork.

## Supported versions

**There is no supported version.** TurnDB is pre-release: the `turndb`
crate is unpublished on crates.io, the portable `turndb` npm package is
unpublished, `@turndb/native` is marked `private`, and both binding
crates carry `publish = false`. Every artifact today is built from
source at a commit.

Security fixes land on `main`. If you are running TurnDB, you are
running a commit, and the fix for you is to move to a later one.

This section is replaced with a supported-version window when there is a
published release line to support.

## What an attacker gets to control

TurnDB is an embedded library. It has no network surface, no daemon, no
authentication, and no users of its own. It runs inside a host
application, with that application's filesystem authority.

So the attack surface is **bytes and paths supplied by, or reachable
from, the host**:

| Surface | What it means |
|---|---|
| A store directory | Its `MANIFEST`, fold generations, parts, WAL and retained history — any of which may be malformed, truncated, adversarial, or from a future format version |
| A pack file | An offline artifact supplied for `restore` or inspection, including its embedded paths |
| Binding inputs | Values crossing the Node or WASI boundary: out-of-range integers, oversized work requests, stale handles, cancellation, concurrent calls |

And what we are protecting is the **host**: its process (no panic, no
unbounded allocation, no wild read), and its filesystem (a crafted store
must not read or write outside the directory it was given).

**Full threat model, in scope and out, is
[`docs/security-review.md`](docs/security-review.md).** It is an
engineering threat review of the version-2 storage core, version-1 pack,
and both bindings — and it says plainly what it is not: not formal
verification, and not an independent third-party audit. Read it before
reporting; several classes are deliberately out of scope and are
documented there with reasons.

## Deliberately out of scope

These are design positions, not gaps we have missed. Each is argued in
`docs/security-review.md`; the short form:

- **Confidentiality of store contents.** The format is plaintext and
  explicitly refuses its reserved encryption flag. CRC32 and BLAKE3
  detect drift and bind identities; they are not keyed authenticity.
  Encrypt the filesystem, not the store.
- **An attacker who can write the live store directory concurrently.**
  Writer locking coordinates cooperating writers. It is not a sandbox
  against a hostile filesystem peer, and path inspection cannot close
  time-of-check/time-of-use replacement races.
- **Authorization, tenant isolation, redaction, network and transport
  security.** TurnDB is embedded; these belong to the host.
- **Denial of service from code already running in the same process.**
  That code can exhaust memory or CPU without going through TurnDB.

## Known security-relevant limitations

Documented rather than hidden, and each stated once in its authoritative
place rather than restated here:

- **The single-writer invariant is not enforced on WASI.** It is
  OS-enforced on Unix via `flock`; under WASI there is no advisory
  locking and the engine cannot enforce it, so the obligation is the
  embedder's. The measured consequence, and why a clean `verify()` does
  not settle it, is in [`FORMAT.md`](FORMAT.md#the-writer-lock).
  **This is a correctness and durability property, not only a security
  one** — it is listed here because an embedder who gets it wrong loses
  acknowledged writes silently.
- **Concurrent hostile filesystem mutation is not contained.** Canonical
  backup checks protect an offline supplied store, but are not an
  `openat2(RESOLVE_BENEATH)` sandbox.
- **CPU budgets are cooperative, not preemptive.** One zstd frame
  decode/encode and one part encoding unit are intentionally
  uninterruptible. Admission limits and worker isolation are the
  protection, not cancellation.
- **Dependency and supply-chain review is external.** Lockfiles pin the
  resolved graph; we have not independently audited DataFusion, Arrow,
  zstd, napi-rs, or the npm publication chain. The license report is
  reproduced from the locked release graph — that is not reproducible
  binaries and not vulnerability review.

Deployments that accept arbitrary third-party stores should validate
them in a resource-limited helper process.

## Disclosure history

*Empty. TurnDB has not been published and no vulnerability has been
reported.*
