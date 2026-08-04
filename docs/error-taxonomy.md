# Error taxonomy

TurnDB keeps detailed `anyhow::Error` chains for diagnosis and exposes a smaller stable class for
programmatic decisions. Rust embedders call `turndb::error::classify(&error)` and may persist
`ErrorClass::code()`. The native Node package rejects with `TurnDbError`, whose `code` uses the same
spelling.

Classification follows typed causes through context layers. It never searches rendered messages.
Consequently, adding diagnostic context cannot change a class, and an untyped error whose prose says
“not found” remains `INTERNAL` until its producer carries a typed cause.

## Stable engine classes

| Rust class | Node code | Meaning |
|---|---|---|
| `InvalidArgument` | `INVALID_ARGUMENT` | The request, cursor, policy, or authorized operation is structurally invalid. Retrying unchanged cannot succeed. |
| `Cancelled` | `CANCELLED` | A cancellation token or deadline stopped the operation at a documented safe checkpoint. No partial scan page is returned. |
| `ResourceExhausted` | `RESOURCE_EXHAUSTED` | A declared write, read, query, or maintenance work ceiling refused the operation. |
| `Unsupported` | `UNSUPPORTED` | The operation requires a capability this build or platform does not provide. |
| `Contention` | `CONTENTION` | Another writer owns the store's enforced exclusive lock. |
| `NotFound` | `NOT_FOUND` | A required filesystem object does not exist. Absence of a record/content value is ordinary `None`/`null`, not this error. |
| `Corruption` | `CORRUPTION` | Typed validation found persisted bytes or references that violate an integrity invariant. |
| `Io` | `IO` | A typed filesystem failure that is not more specifically classified above. |
| `Internal` | `INTERNAL` | No safe public classification is proven. This is intentionally the fallback, not a guess. |

`std::io::ErrorKind::InvalidData` and `UnexpectedEof` classify as corruption; `NotFound` and
`Unsupported` retain their narrower meanings. Other typed I/O failures are `IO`; caller-owned path
conflicts such as backup destination replacement use explicit engine variants rather than treating
every low-level `AlreadyExists` as a request error.

The initial typed engine causes include scan request/cursor validation, scan and lifecycle
interruption, write, atomic-frame, and persistent object-count admission, bounded-compaction
planning, SQL planning/execution and memory admission, writer exclusion, backup/restore, manifest
recovery, and explicit verification-integrity
failures. `IntegrityError` preserves a low-level source chain while identifying a verification
failure as corruption. Unknown parser or invariant failures outside an integrity operation still
remain `INTERNAL`; callers must not infer corruption from their message.

## Binding-owned classes

The Node actor adds two process-boundary states which are not storage-engine failures:

| Node code | Meaning |
|---|---|
| `BUSY` | The bounded actor queue or a single-consumer pull handle is temporarily occupied. Retry only with caller-controlled backoff. |
| `CLOSED` | The native handle has begun or completed closure and cannot accept more work. |

Native boundary type/range validation also becomes `INVALID_ARGUMENT`. Panics, task-join failures,
poisoned binding state, and an unexpectedly exited actor become `INTERNAL`.

## Contract rules

- Codes are stable API; diagnostic messages and context chains are not.
- `TurnDbError.cause` retains the original native error for logging, but consumers branch on `code`.
- A code describes the failed operation, not an automatic retry policy. In particular, `IO` and
  `INTERNAL` do not imply that retrying is safe after a mutating call.
- `CORRUPTION` is evidence from a typed integrity boundary. TurnDB does not relabel arbitrary bugs as
  corruption merely because they occurred while reading.
- `CANCELLED` follows each operation's atomicity contract. It does not mean every maintenance
  operation is interruptible at every point.
- The taxonomy is domain-neutral. It has no trace, OpenTelemetry, tenant, or consumer-specific error
  classes.

## Node example

```js
try {
  await db.scan({ cursor });
} catch (error) {
  if (!(error instanceof TurnDbError)) throw error;
  if (error.code === 'INVALID_ARGUMENT') discardCursor(cursor);
  else if (error.code === 'BUSY') scheduleBoundedRetry();
  else throw error;
}
```

Application code should not switch on `error.message`; it exists to give an operator the full
operation and cause context.
