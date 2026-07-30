# Contributing to turndb

turndb is an embedded, content-addressed columnar store for AI traces. Before changing anything,
read [FORMAT.md](FORMAT.md). It is **normative**: where it and the code disagree, one of them is a
bug, and finding out which is the first job.

## Branching

`main` is protected. Nobody merges to it directly.

| branch | what it is |
|---|---|
| `main` | Protected. Pull requests only, approved by the repository owner. |
| `develop` | The integration trunk. **Branch your work from here, and merge it back here.** |
| `review/<yyyy-mm-dd>` | Cut from `develop` when a batch is ready. The PR to `main` comes from this branch and is **frozen** during review, so the diff cannot shift under the reviewer while they are reading it. `develop` keeps moving the whole time. |

The same model applies in CommandSuite, and is written down there in its own `CONTRIBUTING.md`.
It is stated once per repository and pointed at from everywhere else — three copies of a branch
model is worse than one, because they drift and then nobody knows which is true.

### Commit signing does not currently work

`commit.gpgsign=true` is configured with an SSH key and **signing fails on the machines the core
team develops on**, observed independently on two hosts, in two repositories, by three agents. The
failure differs between attempts rather than between machines: sometimes git's signing path returns
`communication with agent failed`, sometimes the commit hangs past a bounded timeout. In every
observed case the commit does not complete.

This is environmental rather than one person's misconfiguration, and it says nothing about your
machine. Commit with `-c commit.gpgsign=false`.

**This matters if branch protection ever requires signed commits.** It would have to be solved
before that requirement lands, not after — otherwise the first anyone learns of it is a rejected
push to a protected branch.

## Author proposes, partner verifies

Every change is verified by someone who did not write it, and **whoever did not write it decides
whether it is done.** That is not a rubber stamp and not a style review. A verifier is expected to
disagree; agreement arrived at by deference is worth nothing.

Merge to `develop` once your partner has verified. Where you differ, say so rather than smoothing it
into consensus.

## Standards

These are not aspirations. They are what a change is checked against.

**Measure, do not assert.** Say where a number came from and what it omits. Every dial in this
engine that got measured — carve, compaction policy, block size, key granularity — produced a
different answer than intuition suggested at least once. Negative results are recorded in comments
rather than discarded, and a measured *"no change required"* is a first-class result.

**Name what you did not do.** A test run covering four of five configurations says so. An audit
tracing two subsystems rather than all of them says so. Implied coverage is the expensive kind of
wrong.

**Refuse rather than truncate.** Unknown flags, unknown versions, non-zero reserved bytes, a corrupt
manifest, a limit exceeded — all stop. A store that cannot be written is recoverable; one that lies
is not. This applies to APIs as much as to bytes: degrading quietly is the failure mode to design
against.

**Claim exactly what is true.** A missing feature is visible. A doc comment claiming more than the
code does is not, and it survives until someone trusts it.

**Byte-exact reconstruction is the invariant everything else serves.** Attribute order, duplicate
keys, NaN payloads, `-0.0`. A change that makes any of those lossy is wrong regardless of what it
buys.

## Writing tests

Assert **completeness and shape**, not presence or absence.

Presence assertions — *the field appeared*, *the deleted id is gone*, *the call returned* — are
cheap and survive refactors, which is exactly why they are everywhere and exactly why they are blind
to a contract silently degrading. A paged read that excludes every deleted id can still return a
short page. A serializer that emits the right keys can still render every value as `[object
Object]`.

Before committing a test, ask **both** of these. They are not the same question, and the second is
the one people forget:

- **Would this pass against a version that returns *some* of the right answer?** This catches a fix
  that does too little — the short page, the half-rendered field, the truncated window.
- **Would this pass against a version that refuses *more* than it should?** This catches a fix that
  does too much. A suite full of "rejects bad input" assertions passes happily against an
  implementation that also rejects good input. If you add a validity check, test the *nearest valid
  thing* it must still accept.

If either answer is yes, it is not yet testing the contract.

## Commit messages

The commit log is a style contract. Read it before writing one.

A message explains **why**, records what was measured and what was rejected, and is written for
someone reading it in a year with no other context. One logical change per commit. The reasoning in
these messages is the most valuable artifact in the repository, and it is far cheaper to keep than
to reconstruct.

## Before you open a pull request

Run every configuration. They catch different things, and the `sql`-off path in particular is green
today and easily broken:

```sh
cargo test
cargo test --no-default-features
cargo test --release                      # debug-only panics behave differently
cargo test --features dst --test dst      # the deterministic crash-state harness
cargo test --test corruption              # the mutation storm; set STORM_XOR for a fresh seed
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Report **cargo's own exit codes.** A green that came from the wrong process — a status captured
through a pipe, a suite that silently ran zero tests — is worse than no result at all.

Unix only, matching the crate's stated scope. The WASM binding (`bindings/wasm`, published to npm)
builds for `wasm32-wasip1`; `src/sys.rs` is the single answer to what turndb needs from an operating
system, and what it does not get on that target.

## Changing the format

**Anything that changes what a future build promises to read goes to the repository owner first.** A
format guarantee is irreversible in a way code is not.

FORMAT.md is normative and executed — every offset it states is asserted against bytes a real store
just wrote, so it cannot quietly stop being true. If you change the format, the document and those
assertions change in the same commit.

## Publishing

crates.io, npm, and making a repository public are one-way doors. Each goes through the repository
owner on its own review. **Landing on `main` does not approve publication.**
