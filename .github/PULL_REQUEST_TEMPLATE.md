<!--
  Thanks for contributing to TurnDB.

  Before submitting, read CONTRIBUTING.md. Two things it asks for that
  are easy to miss:

    - The commit log is a style contract. A message explains WHY, records
      what was measured and what was rejected, and is written for someone
      reading it in a year with no other context.
    - Run every configuration. They catch different things.
-->

## Summary

<!-- 1-3 sentences. What does this change, and why? -->

## What changed

-
-

## What was measured

<!--
  Numbers, and where they came from. Name the command, not the
  conclusion. Read the full output, not a truncated portion — report the
  number the command produced (`| wc -l`, a summary line, an exit code),
  never one read off a `head`, `tail`, or paged view.
-->

## What this does NOT cover

<!--
  Say what you did not test, not only what you did. A limit you name is
  cheaper than one a reader discovers.
-->

## Related

<!-- Issue / discussion links. Use `Closes #123` to auto-close on merge. -->

## Checklist

- [ ] `cargo test`
- [ ] `cargo test --no-default-features`
- [ ] `cargo test --release` (debug-only panics behave differently)
- [ ] `cargo test --features dst --test dst`
- [ ] `cargo clippy --all-targets` clean
- [ ] Docs updated if behaviour a caller can observe changed
- [ ] If this touches the on-disk format, `FORMAT.md` says so
