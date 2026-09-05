# Contributing to turndb

turndb is an embedded, content-addressed columnar store for AI traces. Before changing public
concepts or lifecycle claims, read [ONTOLOGY.md](ONTOLOGY.md); before changing storage behavior,
read [FORMAT.md](FORMAT.md). Each is normative in its domain: where either and the code disagree,
one of them is a bug, and finding out which is the first job.

## Branching

This repository is trunk-based. `main` is the only long-lived branch.

| branch | what it is |
|---|---|
| `main` | The trunk. **Branch your work from here.** Changes land through a pull request, verified by someone who did not write them, and merge with a merge commit — never a squash or rebase — so every commit that was reviewed is the commit that lands. |
| `<name>/<topic>` | Your working branch. Short-lived: it exists to carry one change to review and dies when the PR merges. |

Nobody pushes to `main` directly. Branch protection on `main` enforces this: changes reach `main`
only through a pull request.

**`develop` is retired.** Until 2026-07-31 this repository used a `develop` integration branch with
frozen `review/<date>` PR branches; every commit from that model is in `main`'s history. Deleting
the remote branch did not delete anyone's local ref, and a stale local `develop` checks out without
a word of complaint — so if your clone predates the migration, run `git fetch --prune` and
`git branch -D develop` before branching anything. Because `develop` was merged (not squashed) into
`main`, a branch accidentally cut from the stale ref is merely behind `main` rather than carrying
phantom commits — but base your work on `main` all the same. Nothing server-side currently refuses
a push that recreates `develop`; a CI guard that fails such a push is planned but not yet in
place.

### Commit signing: where it works, and where it did not

Signed commits are required of this team's agent members, and the configuration below is the one
that has been shown to work end to end — a commit made with it on 2026-08-30 was accepted by
GitHub as `verified: true, reason: valid` for the committing account. The host was Debian 13, git
2.47.3, OpenSSH 10.0p2, **no `ssh-agent` running** (`SSH_AUTH_SOCK` unset), an ed25519 key with no
passphrase generated for signing only, and the key registered on the account as an *SSH signing
key* (`POST /user/ssh_signing_keys`), not as an authentication key:

```sh
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519_signing.pub   # the .pub path, not a key id
git config --global commit.gpgsign true
git config --global gpg.ssh.allowedSignersFile ~/.ssh/allowed_signers
```

The same `commit.gpgsign=true` with an SSH key **did fail on the two hosts the core team first
developed on**, in two repositories, in a way that differed between attempts rather than between
machines: sometimes git's signing path returned `communication with agent failed`, sometimes the
commit hung past a bounded timeout, and in every observed case the commit did not complete. Those
hosts had an agent; the message names it. The working configuration above never talks to one,
because `user.signingkey` points at a key *file* and git invokes `ssh-keygen -Y sign` directly. That
is consistent with the failures being agent-side, but it has not been proven by fixing an affected
host, so it is a hypothesis, not a diagnosis. On a host where signing still fails, commit with
`-c commit.gpgsign=false` and say so in the PR.

**This matters if branch protection ever requires signed commits.** There is now a known-good
configuration to require, so the remaining work before such a rule lands is confirming it on every
host that pushes, not finding one.

## Author proposes, partner verifies

Every change is verified by someone who did not write it, and **whoever did not write it decides
whether it is done.** That is not a rubber stamp and not a style review. A verifier is expected to
disagree; agreement arrived at by deference is worth nothing.

Merge to `main` once your partner has verified. Where you differ, say so rather than smoothing it
into consensus.

## Standards

These are what a change is checked against.

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

Before committing a test, ask **both** of these. They are not the same question:

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
cargo clippy --all-targets --all-features -- -D warnings
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
suite, here or in CI.

### A new sweep must be shown to fail

**Anything added to `tests/dst.rs` or `tests/corruption.rs` gets a deliberate defect introduced
under it, once, to prove the sweep detects it.** Say in the test's doc comment what you broke and
where it failed. Then revert the defect — the mutation is a check on the harness, not a fixture to
keep.

This is not a style preference. Both harnesses have shipped sweeps that were green and inert:

- A container commit sweep passed all 48 of its crash states with the fsync that orders members
  ahead of the superblock **deleted**. No variant expressed a later write landing while an earlier
  one did not, so the failure was unreachable. Fixing that took a new variant, not a new assertion.
- A container corruption sweep passed 6,000 mutants with a bounds check in the directory walk
  **deleted**. Every route to that parser is checksum-gated, so byte flips never reach it. Reaching
  it needed a harness that damages the payload and then repairs the checksums over it.
- A space-accounting test passed against a function that returns zero for anything not backed by a
  directory, because nothing compared its answer to a second source.

In each case the sweep looked thorough, ran for real, and would have gone on passing through the
bug it existed to find. A green harness that has never been made to go red is a hypothesis about
coverage, not evidence of it — and the cost of checking is one commit you throw away.

**Not runnable locally:**

- The **native prebuild** and its install check, which exercise `ubuntu-22.04` glibc packaging.
- The **Node matrix** (22, 24, 26) unless you have all three installed; `npm/build.sh` uses whichever
  `node` is on your PATH.

**A workflow change is not a Rust-free change.** `tests/package.rs` reads
`.github/workflows/ci.yml` and asserts the Node matrix there matches what the packages claim. So
editing CI can fail `cargo test` for reasons that look nothing like CI — and the reverse: a Rust
test can be the thing that stops you silently narrowing the supported Node range. **Run `cargo test
--test package` after touching a workflow file.**

**Exercisable on demand:** the red-branch alert. `main-health.yml` opens an issue when CI concludes
anything but success on `main`, and closes it when `main` recovers. Because `workflow_run` only ever
runs the copy of a workflow file that is on the **default branch**, the filter cannot be widened
from a feature branch to test it — so the branch `ci-alert-drill` is permanently in the filter.
Push anything failing to it and read the issue that opens; push a fix and watch it close. That is
how to confirm the alert works, and `main` is never involved.

**Enforced at the merge button:** a ruleset on `main`, and **the live ruleset is the authority on
what it requires — this document does not restate it.** A restatement is one configuration change
away from being wrong, and wrong in the direction nobody notices: it keeps describing a gate that
was loosened. Ask the repository instead.

```sh
gh api repos/turndb/turndb/rules/branches/main
```

The one thing worth stating here, because it is a fact about this repository rather than about the
ruleset: the required status check, whatever the ruleset names, is produced by the `gate` job in
`ci.yml`.

## Changing the format

**Anything that changes what a future build promises to read goes to the repository owner first.** A
format guarantee is irreversible in a way code is not.

FORMAT.md is normative and executed — every offset it states is asserted against bytes a real store
just wrote, so it cannot quietly stop being true. If you change the format, the document and those
assertions change in the same commit.

## Publishing

crates.io, npm, and making a repository public are one-way doors. Each goes through the repository
owner on its own review. **Landing on `main` does not approve publication.**

User-visible changes are recorded with `knope document-change`. The generated file under
`.changeset/` names the single lockstep package (`default`) and one of `major`, `minor`, or `patch`;
CI validates manually written files too. If a Knope command that commits hangs or fails, first run
`ssh-add -l`: an unavailable signing key can make the commit wait without producing tool output.
For this pre-1.0 release line, both `patch` and `minor` advance `0.1.x`; only `major` advances to
`0.2.0`, so the change type remains a compatibility statement even when two choices produce the
same next number.

Pushing a change file to `main` updates the `release` PR. Merging that PR creates the single
annotated `vX.Y.Z` tag and GitHub release, then starts the crate, native, portable-wasm, Python, and
browser-artifact workflows. Registry publication stops at its protected `npm` or `pypi` environment
for owner review; the browser workflow attaches the byte-rebuilt one-file viewer to the already
approved GitHub release. The release-PR workflow needs the owner-managed `KNOPE_TOKEN` secret because GitHub does
not trigger CI for a pull request created with the default workflow token.

If a candidate check fails after a tag exists, repair the workflow on `main` and manually dispatch
`.github/workflows/release.yml` with that exact existing tag and the failed component. The manual
path verifies both the annotated tag and its GitHub release, then runs only that component's
workflow; it does not create or move a tag and does not fan out the other publication workflows.
Dispatching a leaf workflow directly is not equivalent: registry trusted publishing authenticates
workflow identity, and the Python upload deliberately lives in the top-level workflow so PyPI's
OIDC and attestation identities agree. The selector admits only components with an exercised
recovery need; add another deliberately rather than turning recovery into an unrestricted replay.

### Publishing the portable npm package

The portable `turndb` package is published by `.github/workflows/release-wasm.yml`, which runs from
the release fan-out alongside the crate and native workflows. It checks out the exact annotated
lockstep tag, verifies the package version against it, rebuilds the wasm from that source and runs
the package suite, exercises the packed tarball on every supported Node major, and publishes that
exact audited tarball through npm trusted publishing behind the protected `npm` environment.
Publication therefore requires an npm trusted publisher configured for that workflow — an owner
action; see the publication gate below.

If the workflow cannot be used, the fallback is a manual publish by the repository owner from a
clean local checkout, under the owner's npm credentials. For a release `X.Y.Z`, the publisher:

1. obtains explicit release approval and checks out the exact annotated lockstep `vX.Y.Z` tag
   created when the release PR merged;
2. verifies the tag, version, clean tree, and registry identity:

   ```sh
   test "$(git describe --tags --exact-match HEAD)" = "vX.Y.Z"
   test "$(git cat-file -t vX.Y.Z)" = tag
   test "$(node -p "require('./npm/turndb/package.json').version")" = "X.Y.Z"
   test -z "$(git status --porcelain)"
   npm whoami
   ```

3. from `npm/turndb`, runs the following in one shell. npm writes the `prepublishOnly` build output
   ahead of the JSON object, so the whole output is not valid JSON; the `sed` step deliberately
   extracts the final object before parsing it:

   ```sh
   set -o pipefail
   dry_run_output="$(mktemp)"
   dry_run_json="$(mktemp)"
   audited_integrity_file="${TMPDIR:-/tmp}/turndb-X.Y.Z.integrity"
   npm publish --dry-run --json | tee "$dry_run_output"
   sed -n '/^{/,$p' "$dry_run_output" > "$dry_run_json"
   node -e 'const fs=require("node:fs"); const p=JSON.parse(fs.readFileSync(process.argv[1])); console.table(p.files); console.log(p.size, p.unpackedSize, p.integrity)' "$dry_run_json"
   node -e 'const fs=require("node:fs"); process.stdout.write(JSON.parse(fs.readFileSync(process.argv[1])).integrity)' "$dry_run_json" > "$audited_integrity_file"
   ```

   The publisher confirms that `prepublishOnly` rebuilt the WASM and passed the package tests, then
   reads the reported tarball file list, packed size, unpacked size, and integrity. The expected
   payload is `LICENSE`, `NOTICE`, `README.md`, `index.d.ts`, `index.mjs`, `package.json`, and
   `turndb.wasm`; any addition or omission stops the release;
4. runs `npm publish --access public` from the same directory, without `--ignore-scripts`; and
5. verifies that the registry has the same bytes that the dry run audited, then installs that exact
   version into an empty directory and runs a documented README example against it:

   ```sh
   npm view turndb@X.Y.Z name version dist.integrity dist.tarball --json
   audited_integrity_file="${TMPDIR:-/tmp}/turndb-X.Y.Z.integrity"
   test -s "$audited_integrity_file"
   audited_integrity="$(cat "$audited_integrity_file")"
   published_integrity="$(npm view turndb@X.Y.Z dist.integrity)"
   test -n "$audited_integrity"
   test -n "$published_integrity"
   test "$published_integrity" = "$audited_integrity"
   ```

`npm/prepublish-check.sh` is reached by both the dry run and the real directory publish through
`prepublishOnly`; it refuses an uncommitted tree and `npm/build.sh` rebuilds and tests the artifact.
It is a provenance guard, not an authorization gate, and `--ignore-scripts` bypasses it. Publishing
a prebuilt tarball also does not re-run it. Neither bypass is part of this procedure.

Step 4 rebuilds before publishing, so this procedure depends on `rust-toolchain.toml` making that
build reproducible. The integrity comparison enforces the property against the registry result; a
mismatch is a failed release verification, never a value to update by hand.

### The publication gate

Most of these are enforced by the toolchain and need no remembering; the ones that are not say so.

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
  report the *old* engine's behaviour.
- **Native release tarballs come from the exact tagged source and exact tested bytes.** The release
  workflow verifies the annotated lockstep `vX.Y.Z` tag, cross-builds one addon, audits its ELF
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

- **The GitHub repository description.** It is a standalone surface: whatever it says is the whole
  claim for anyone who does not click through, so it must carry **no unqualified enforcement
  claim**. "Single-writer" belongs there only with the WASI reduction attached, or not at all — the
  npm package description dropped it for exactly this reason. Do not fill it in from the README's
  first line, which carries the unqualified form.
- **The copyright holder in `LICENSE` and `NOTICE`** must name whoever holds the copyright at
  publish time.
- **Whether `crates.io` and `npm` metadata still describe what ships.** Registry `description`
  fields render standalone, and `package.json` is not in its own `files` list — so no sweep over
  the package payload reaches them.
- **The registries' trusted publishers.** Crate and npm publication workflows run as *called*
  workflows under `.github/workflows/release.yml`. GitHub names the calling workflow in the OIDC
  token's `workflow_ref` claim — the called one appears only in `job_workflow_ref` — and crates.io
  and npm match on the former. PyPI currently exchanges a reusable job's token against
  `job_workflow_ref`, but verifies its attestation against the `workflow_ref` build-config URI; a
  publish action inside `release-python.yml` therefore cannot satisfy both identities. The Python
  candidates are still built and install-tested there, while the privileged upload runs directly in
  `release.yml`. Every registry trusted publisher must consequently name **`release.yml`**, never a
  leaf workflow. Publishers pointed at the leaves were correct while releases were dispatched leaf
  by leaf, and every one of them broke on the first orchestrated run: crates.io said so precisely,
  npm reported `ENEEDAUTH` as though no credential had been offered at all, and PyPI rejected the
  reusable workflow's mismatched attestation.
- **The npm releases' external configuration.** The owner must control the `@turndb` scope and every
  published package name, and protect the GitHub `npm` environment with required review. A package
  that does not yet exist on the registry has nowhere to attach a trusted publisher, so its first
  publication needs another route. Source code cannot prove those registry and repository settings.
