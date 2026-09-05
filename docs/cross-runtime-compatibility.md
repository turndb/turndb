# Cross-runtime compatibility

Native TurnDB and the `wasm32-wasip1` package use the same store format but different zstd
implementations, threading models, writer-exclusion guarantees, and JavaScript boundaries. Format
compatibility is therefore tested as behavior rather than inferred from shared Rust types.

`npm/interop.sh` runs both directions over the overlap profile:

1. the portable package writes a current-draft store at its WASM compression default; a native Rust
   reader compares the complete ordered id set, every byte of content, and ordered typed attributes
   against an independent deterministic oracle;
2. native Rust writes the same corpus with its native compression path; the portable package performs
   the same exact comparison.

The corpus covers signed/unsigned integers, finite floats, booleans, binary metadata, nanosecond
timestamps, explicit null, duplicate string keys in order, large repeated content, unique tails,
flush, reopen, and paged id order. Per-piece BLAKE3 verification remains active, but it is not used as
the oracle: the expected whole content and metadata are independently regenerated on each side so a
wrong program order cannot pass merely because each individual piece is valid.

CI runs the proof once on Node 24 after rebuilding the WASM artifact from the checked-out source.
It runs once because Node-major coverage lives in the package matrices and repeating identical
format bytes on every Node major would add time without broadening the runtime boundary. The script
uses isolated temporary stores and removes them on every exit.

The proof does not claim capability parity. WASI still has embedder-enforced writer exclusion, inline
compression, no SQL/Arrow lens, and refold-only reclamation. It proves only the overlapping persisted
record contract, which is the claim consumers need when a store moves between lightweight and native
embeddings.
