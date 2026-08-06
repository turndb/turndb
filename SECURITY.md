# Security

## Reporting a vulnerability

Please do **not** open a public issue for a security vulnerability.

**Email security@efficacious.io.** That channel does not depend on any
repository setting.

GitHub's private vulnerability reporting may also be available, under
this repository's **Security → Report a vulnerability**. If you do not
see it there, it is not enabled yet — use email.

Useful reports include the affected commit, the platform and binding
(native Rust, native Node addon, or portable WASI), and either a
reproduction or a store directory / pack file that triggers the
behaviour. If a store reproduces it, say so rather than attaching it
until we have agreed a channel — an adversarial store is the payload.

**We aim to acknowledge within 72 hours.** We do not commit to a patch
window yet: TurnDB is pre-1.0 with a single released version and no
release history to base a commitment on. This section will gain a
concrete remediation commitment as that history accumulates.

If the issue is in a dependency rather than in TurnDB, please still tell
us so we can pin, patch or fork.

## Supported versions

**There is no supported-version window yet.** TurnDB is pre-1.0, and
version 0.1.0 (released 2026-08-06) is the only published release: the
`turndb` crate on crates.io, the portable `turndb` npm package, and
`@turndb/native`.

Security fixes land on `main` and reach the registries with the next
release. If you need a fix before then, build from source at a commit
that carries it.

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
native Node binding and portable WASI binding **as of 2026-08-03** — and
it says plainly what it is not: not formal verification, and not an
independent third-party audit. **Its own caveat carries as much weight:
findings remain useful only if new format fields, parsers and binding
methods update both the review and their adversarial tests.** Read it
before reporting; several classes are deliberately out of scope and are
documented there with reasons.

## Deliberately out of scope

These are design positions. Each is argued in
`docs/security-review.md`; the short form:

- **Authentication or confidentiality of store contents.** The format
  is plaintext and explicitly refuses its reserved encryption flag.
  CRC32 and BLAKE3 detect drift and bind identities; they are not keyed
  authenticity. Encrypt the filesystem, not the store.
- **An attacker who can write the live store directory concurrently.**
  Writer locking coordinates cooperating writers. It is not a sandbox
  against a hostile filesystem peer, and path inspection cannot close
  time-of-check/time-of-use replacement races.
- **Authorization, tenant isolation, redaction, network protocol
  security, and backup transport.** TurnDB is embedded; these belong to
  the host.
- **Denial of service from code already running in the same process.**
  That code can exhaust memory or CPU without going through TurnDB.

## Known security-relevant limitations

Each is documented in full in its authoritative place; the short form:

- **The single-writer invariant is not enforced on WASI.** It is
  OS-enforced on Unix via `flock`; under WASI there is no advisory
  locking and the engine cannot enforce it, so the obligation is the
  embedder's. The measured consequence, and why a clean `verify()` does
  not settle it, is in [`FORMAT.md`](FORMAT.md#the-writer-lock).
  **This is a correctness and durability property, not only a security
  one** — an embedder who gets it wrong loses acknowledged writes
  silently.
- **Concurrent hostile filesystem mutation is not contained.** Canonical
  backup checks protect an offline supplied store, but are not an
  `openat2(RESOLVE_BENEATH)` sandbox. **Applications must not grant
  untrusted writers access to an actively opened store directory.**
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

*Empty. No vulnerability has been reported.*
