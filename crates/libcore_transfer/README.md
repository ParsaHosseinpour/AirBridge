# libcore_transfer

Multi-file transfer orchestration for AirBridge. This crate sits between the
authenticated byte stream and the FFI:

* **Canonical manifests.** A batch is agreed before any payload byte moves: an
  ordered list of relative paths, sizes and SHA-256 digests behind a
  self-delimiting preamble.
* **A queue state machine.** Pending, active, completed, failed and cancelled
  items, plus overall progress and a linear ETA, all readable from any thread.
* **Sequential batch transfer.** The sender writes the manifest once and then
  runs the ordinary resumable `HELLO` / chunks / `COMPLETE` exchange for each
  file over the same stream. The receiver validates each `HELLO` against the
  manifest, writes under a safe destination root, and verifies the whole-file
  digest before committing.
* **Batch-level events.** Started, per-file start/progress/completion/failure,
  and a final report, shaped so the FFI can forward them unchanged.

The transport engine (`libcore_transport`) remains the source of truth for
chunking, per-chunk integrity and resume state. This crate adds only ordering,
manifest binding, queue bookkeeping and orchestration policy.

## Wire shape

```text
u32 BE manifest_len | manifest bytes
HELLO(file 0) | chunks | COMPLETE
HELLO(file 1) | chunks | COMPLETE
...
```

The payload id of each entry is derived deterministically from the batch id and
the entry index, so retrying the same batch reuses the receiver's persisted part
files.

## Testing

```sh
cargo test -p libcore_transfer
```

The end-to-end tests run both sides over `tokio::io::duplex`, so no network or
crypto layer is required.
