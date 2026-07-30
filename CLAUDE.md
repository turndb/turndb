# turndb — agent context

Read **[CONTRIBUTING.md](CONTRIBUTING.md)** first. It is canonical for the branch model, the
verification standard, and how tests and commit messages are expected to read here.

Read **[FORMAT.md](FORMAT.md)** before changing anything on disk. It is normative: where it and the
code disagree, one of them is a bug.

Two things worth knowing before your first commit, because they are the ones most often missed:

- **Branch from `develop`, never from `main`.** `main` is protected and merges only through a
  reviewed pull request.
- **Assert completeness, not presence.** Ask whether your test would pass against a version that
  returns *some* of the right answer.
