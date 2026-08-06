# Contributing to turndb

turndb is an embedded, content-addressed columnar store for AI traces. Before changing anything,
read [FORMAT.md](FORMAT.md). It is **normative**: where it and the code disagree, one of them is a
bug, and finding out which is the first job.

## Branching

This repository is trunk-based. `main` is the only long-lived branch.

| branch | what it is |
|---|---|
| `main` | The trunk. **Branch your work from here.** Changes land through a pull request, verified by someone who did not write them, and merge with a merge commit — never a squash or rebase — so every commit that was reviewed is the commit that lands. |
| `<name>/<topic>` | Your working branch. Short-lived: it exists to carry one change to review and dies when the PR merges. |

Nobody pushes to `main` directly. Changes land through a pull request, and a repository ruleset
enforces that server-side.

**The live ruleset is the authority on what it requires. This document does not restate it** —
a restatement is one configuration change away from being wrong, and the wrongness is silent.
`gh api repos/turndb/turndb/rules/branches/main` reports what currently applies.

**`develop` is retired.** Until 2026-07-31 this repository used a `develop` integration branch with
frozen `review/<date>` PR branches; every commit from that model is in `main`'s history. Deleting
the remote branch did not delete anyone's local ref, and a stale local `develop` checks out without
a word of complaint — so if your clone predates the migration, run `git fetch --prune` and
`git branch -D develop` before branching anything. Because `develop` was merged (not squashed) into
`main`, a branch accidentally cut from the stale ref is merely behind `main` rather than carrying
phantom commits — but base your work on `main` all the same.

CommandSuite made the same migration to trunk-based development (its #55); each repository states
its own model in its own `CONTRIBUTING.md` — three copies of a branch model is worse than one,
because they drift and then nobody knows which is true.

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

Merge to `main` once your partner has verified. Where you differ, say so rather than smoothing it
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

The native build is Unix only, matching the crate's stated scope. The WASM binding
(`bindings/wasm`, published to npm) builds for `wasm32-wasip1`; `src/sys.rs` is the single answer
to what turndb needs from an operating system, and what it does not get on that target.

### What you can run locally, and what only CI can run

**Same compiler as CI, and it is the file that makes it true — not the workflow.** `rust-toolchain.toml`
pins `1.95.0`, and rustup honours it over whatever you have installed and over whatever a CI action
installed first. Inside this repository `cargo --version` reports 1.95.0 even when your default is
newer; outside it, it reports your default.

**So do not read the workflow as the source of truth.** `rust gates` installs
`dtolnay/rust-toolchain@1.95.0` while the npm, native-addon and prebuild jobs install `@stable` —
those lines describe what gets *downloaded*, not what *builds*, and the pin overrides all of them.
Before the pin landed they genuinely diverged: the gate deciding acceptability and the jobs building
the shipped artifacts ran different rustc versions. Delete the pin and that returns, silently.

**Runnable locally, and identical to CI:** every command in the block above, plus
`bash npm/build.sh`, which rebuilds `turndb.wasm` from committed Rust and runs the package suite.
The artifact is byte-reproducible **because** of that pin — three machines produced the same sha256
for the same commit under `1.95.0`, and a build on `1.97.1` differed by 4,137 bytes. Reproducibility
is a property of the pin, not of the commit.

**Runnable locally but NOT on every push:** `cargo test --features dst --test dst`. The crash-state
harness runs nightly (`nightly.yml`) and on demand, not per push, because of its runtime. **It does
not run against `wasm32-wasip1` at all** — it drives the write path below `src/sys.rs`, and the
portable target does not execute it. So a change to the WASM binding is *not* crash-tested by any
suite, here or in CI. That is a real gap and it is stated rather than papered over.

**Not runnable locally:**

- The **native prebuild** and its install check, which exercise `ubuntu-22.04` glibc packaging.
- The **Node matrix** (22, 24, 26) unless you have all three installed; `npm/build.sh` uses whichever
  `node` is on your PATH.
- The **release-profile suite**, which runs hosted now that the repository is public — the larger
  public runner links the DataFusion-static release test binaries that the private free-tier runner
  could not. See `docs/support-and-compatibility.md`.

**A workflow change is not a Rust-free change.** `tests/package.rs` reads
`.github/workflows/ci.yml` and asserts the Node matrix there matches what the packages claim. So
editing CI can fail `cargo test` for reasons that look nothing like CI — and the reverse: a Rust
test can be the thing that stops you silently narrowing the supported Node range. **Run `cargo test
--test package` after touching a workflow file.** This cost a CI cycle to discover.

**Exercisable on demand:** the red-branch alert. `main-health.yml` opens an issue when CI concludes
anything but success on `main`, and closes it when `main` recovers. Because `workflow_run` only ever
runs the copy of a workflow file that is on the **default branch**, the filter cannot be widened
from a feature branch to test it — so the branch `ci-alert-drill` is permanently in the filter.
Push anything failing to it and read the issue that opens; push a fix and watch it close. **An
alert nobody has watched fire is indistinguishable from one that does not work**, and `main` is
never involved.

**Enforced, not documented here:** required status checks. **What is required is whatever the
ruleset says** — see [Branches](#branches) for where to ask it. The `gate` job in `ci.yml` exists so
that a required check has one line worth reading.

## Changing the format

**Anything that changes what a future build promises to read goes to the repository owner first.** A
format guarantee is irreversible in a way code is not.

FORMAT.md is normative and executed — every offset it states is asserted against bytes a real store
just wrote, so it cannot quietly stop being true. If you change the format, the document and those
assertions change in the same commit.

## Publishing

crates.io, npm, and making a repository public are one-way doors. Each goes through the repository
owner on its own review. **Landing on `main` does not approve publication.**

### The publication gate

These live here rather than in anyone's notes, because a gate item in one person's file is not a
gate. Most are enforced by the toolchain and need no remembering; the ones that are not say so.

**Enforced — you cannot skip these by accident:**

- **The published `.wasm` is built from the source you are publishing.** `prepublishOnly` runs
  `npm/build.sh`, which rebuilds from Rust and runs the package tests. A failing build aborts the
  publish. *Residue:* `npm publish <a-tarball-you-built-earlier>` does not re-run it — that is a
  deliberate act rather than an accident, and nothing in `package.json` can reach it.
- **A broken doc link fails the build.** `#![deny(rustdoc::broken_intra_doc_links)]` in both crate
  roots. *Reaches only `[`Item`]` link syntax* — 83 such lines against 302 carrying any backticked
  identifier, so a stale plain-backtick reference still passes.
- **Examples ship deliberately.** `tests/package.rs` fails if an example is neither excluded nor
  declared user-facing. `exclude` is opt-out, so an unclassified example ships to crates.io
  silently; this is what stops that.
- **Tests refuse a stale `.wasm`.** `npm/turndb/test/_artifact.mjs` compares the artifact against
  the newest engine source. Running `node --test` directly after changing Rust would otherwise
  report the *old* engine's behaviour, which has already cost one wrong verification.
- **Native release tarballs come from the exact tagged source and exact tested bytes.** The manual
  release workflow verifies an annotated `native-vX.Y.Z` tag, cross-builds one addon, audits its ELF
  glibc floor and package contents, and install-tests the same uploaded tarballs on every declared
  Node major before reaching the protected `npm` environment. Tracked native manifests remain
  private; only release staging removes that guard.
- **Native packages carry the legal payload and provenance.** Packaging stages the repository's
  exact `LICENSE`, `NOTICE`, and generated third-party license report; the pinned generator must
  reproduce that report from the locked release graph. Packaging hashes both tarballs, and
  publication requires GitHub OIDC with npm provenance. The publish script requires release-job
  markers, a release manifest, an exact tag, and unchanged tarball bytes. These are accident guards,
  not authorization: a registry credential holder can deliberately bypass scripts, which is why the
  protected `npm` environment and owner review remain required.

**Not enforceable here — check these by hand at publish time:**

- **The GitHub repository description.** Set 2026-08-05 and carrying no unqualified enforcement
  claim. It is a standalone surface: whatever it says is the whole claim for anyone who does not
  click through, so it must carry **no unqualified enforcement claim**. "Single-writer" belongs
  there only with the WASI reduction attached, or not at all — the npm package description dropped
  it for exactly this reason. **The hazard this survived** was someone filling it from the README's
  first line, in a hurry, on publication day. Re-check it whenever the README's opening changes.
- **The copyright holder in `LICENSE` and `NOTICE`** must name whoever holds the copyright at
  publish time.
- **Whether `crates.io` and `npm` metadata still describe what ships.** Registry `description`
  fields render standalone, and `package.json` is not in its own `files` list — so no sweep over
  the package payload reaches them.
- **The first native npm release's external configuration.** The owner must control the `@turndb`
  scope and both package names, configure trusted publishing for `.github/workflows/release-native.yml`,
  and protect the GitHub `npm` environment with required review. Source code cannot prove those
  registry and repository settings.
