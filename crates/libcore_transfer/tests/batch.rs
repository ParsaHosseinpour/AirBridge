//! Integration tests for manifests, the queue, the preamble and a real
//! two-sided batch transfer over an in-memory duplex stream.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libcore_transfer::{
    normalize_relative_path, payload_id_for, read_manifest, receive_batch, upload_batch,
    write_manifest, BatchError, BatchEvent, BatchOptions, CancelToken, ManifestEntry,
    ManifestError, PreparedBatch, QueueItemStatus, QueueStatus, TransferManifest, TransferQueue,
    MANIFEST_HEADER_LEN, MAX_MANIFEST_BYTES,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A self-cleaning temporary directory.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "airbridge-transfer-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn entry(path: &str, size: u64, fill: u8) -> ManifestEntry {
    ManifestEntry::new(path, size, [fill; 32]).unwrap()
}

fn two_file_manifest() -> TransferManifest {
    TransferManifest::from_entries(
        0x1234_5678_9abc_def0,
        [entry("alpha.txt", 10, 1), entry("nested/beta.bin", 20, 2)],
    )
    .unwrap()
}

#[test]
fn manifest_roundtrips_in_canonical_form() {
    let manifest = two_file_manifest();
    let bytes = manifest.encode().unwrap();
    assert_eq!(bytes, manifest.encode().unwrap(), "encoding is canonical");
    assert_eq!(
        bytes.len(),
        MANIFEST_HEADER_LEN + (2 + 9 + 8 + 32) + (2 + 15 + 8 + 32)
    );
    let decoded = TransferManifest::decode(&bytes).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(decoded.batch_id(), manifest.batch_id());
    assert_eq!(decoded.total_bytes(), 30);
}

#[test]
fn manifest_rejects_unsafe_and_duplicate_paths() {
    for raw in [
        "",
        "/absolute",
        "\\absolute",
        "C:/drive",
        "a/../../b",
        "a/./b",
        "a//b",
        "a\\..\\b",
        "a/b:c",
    ] {
        assert!(
            normalize_relative_path(raw).is_err(),
            "accepted unsafe path {raw:?}"
        );
    }
    assert!(matches!(
        normalize_relative_path("a\0b"),
        Err(ManifestError::NulByte)
    ));
    assert!(matches!(
        normalize_relative_path(&"a".repeat(4097)),
        Err(ManifestError::PathTooLong)
    ));
    assert_eq!(
        normalize_relative_path("photos\\2026/a.jpg").unwrap(),
        "photos/2026/a.jpg"
    );
    assert!(matches!(
        TransferManifest::from_entries(1, [entry("same", 1, 0), entry("same", 2, 1)]),
        Err(ManifestError::DuplicatePath { .. })
    ));
}

#[test]
fn manifest_decoder_rejects_corruption() {
    let valid = two_file_manifest().encode().unwrap();

    assert!(matches!(
        TransferManifest::decode(&valid[..valid.len() - 1]),
        Err(ManifestError::Truncated)
    ));

    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(matches!(
        TransferManifest::decode(&trailing),
        Err(ManifestError::TrailingBytes)
    ));

    let mut magic = valid.clone();
    magic[0] = b'X';
    assert!(matches!(
        TransferManifest::decode(&magic),
        Err(ManifestError::InvalidMagic)
    ));

    let mut version = valid.clone();
    version[4..6].copy_from_slice(&2_u16.to_be_bytes());
    assert!(matches!(
        TransferManifest::decode(&version),
        Err(ManifestError::UnsupportedVersion(2))
    ));

    let mut flags = valid.clone();
    flags[6..8].copy_from_slice(&1_u16.to_be_bytes());
    assert!(matches!(
        TransferManifest::decode(&flags),
        Err(ManifestError::UnsupportedFlags(1))
    ));

    let mut total = valid.clone();
    total[20..28].copy_from_slice(&999_u64.to_be_bytes());
    assert!(matches!(
        TransferManifest::decode(&total),
        Err(ManifestError::InvalidTotal)
    ));

    // A sum that overflows u64 must not be accepted merely because the
    // saturating total happens to equal the declared value.
    let mut overflow = valid.clone();
    overflow[20..28].copy_from_slice(&u64::MAX.to_be_bytes());
    overflow[MANIFEST_HEADER_LEN + 2 + 9..MANIFEST_HEADER_LEN + 2 + 9 + 8]
        .copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(matches!(
        TransferManifest::decode(&overflow),
        Err(ManifestError::InvalidTotal)
    ));

    let oversized = vec![0_u8; MAX_MANIFEST_BYTES + 1];
    assert!(matches!(
        TransferManifest::decode(&oversized),
        Err(ManifestError::ManifestTooLarge)
    ));

    let mut path_encoding = valid.clone();
    path_encoding[MANIFEST_HEADER_LEN + 2] = 0xFF;
    assert!(matches!(
        TransferManifest::decode(&path_encoding),
        Err(ManifestError::InvalidPathEncoding)
    ));

    // The decoder accepts only the canonical `/` form, not a backslash that
    // the local-path normalizer would have rewritten.
    let mut noncanonical = valid.clone();
    noncanonical[MANIFEST_HEADER_LEN + 2 + 5] = b'\\';
    assert!(matches!(
        TransferManifest::decode(&noncanonical),
        Err(ManifestError::NonCanonicalPath { .. })
    ));

    // Build two equal-length paths, then rewrite the second to the first so
    // the decoder sees a duplicate without changing the buffer length.
    let same_len = TransferManifest::from_entries(7, [entry("aaaa", 1, 0), entry("bbbb", 2, 1)])
        .unwrap()
        .encode()
        .unwrap();
    let second_path = MANIFEST_HEADER_LEN + (2 + 4 + 8 + 32) + 2;
    let mut duplicate = same_len;
    duplicate[second_path..second_path + 4].copy_from_slice(b"aaaa");
    assert!(matches!(
        TransferManifest::decode(&duplicate),
        Err(ManifestError::DuplicatePath { .. })
    ));
}

#[test]
fn payload_ids_are_stable_and_distinct() {
    let manifest = two_file_manifest();
    assert_eq!(
        manifest.payload_id(0),
        Some(payload_id_for(0x1234_5678_9abc_def0, 0))
    );
    assert_eq!(manifest.payload_id(0), manifest.payload_id(0));
    assert_ne!(manifest.payload_id(0), manifest.payload_id(1));
    assert_eq!(manifest.payload_id(2), None);

    let mut seen = std::collections::HashSet::new();
    for index in 0..10_000 {
        assert!(
            seen.insert(payload_id_for(99, index).get()),
            "payload id collision at index {index}"
        );
    }
}

#[test]
fn queue_tracks_lifecycle_and_totals() {
    let manifest = two_file_manifest();
    let queue = TransferQueue::from_manifest(&manifest).unwrap();
    assert_eq!(queue.status(), QueueStatus::Idle);
    assert_eq!(queue.snapshot().pending_files, 2);

    queue.begin().unwrap();
    queue.activate(0).unwrap();
    queue.record_progress(0, 4).unwrap();
    // Progress is monotonic and clamped.
    queue.record_progress(0, 1).unwrap();
    assert_eq!(queue.items()[0].bytes_transferred, 4);

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.status, QueueStatus::Running);
    assert_eq!(snapshot.active_files, 1);
    assert_eq!(snapshot.pending_files, 1);
    assert_eq!(snapshot.transferred_bytes, 4);
    assert_eq!(snapshot.remaining_bytes, 26);
    assert_eq!(snapshot.current_file_index, Some(0));

    queue.complete(0, 2, None).unwrap();
    queue.activate(1).unwrap();
    queue.fail(1, "disk full").unwrap();
    assert_eq!(queue.finish(), QueueStatus::Failed);
    let report = queue.report();
    assert_eq!(report.completed_files, 1);
    assert_eq!(report.failed_files, 1);
    assert_eq!(report.transferred_bytes, 10);
    assert_eq!(report.files[0].status, QueueItemStatus::Completed);
    assert_eq!(report.files[0].resumed_from, 2);
    assert_eq!(report.files[1].status, QueueItemStatus::Failed);
    assert_eq!(report.files[1].error.as_deref(), Some("disk full"));
    assert_eq!(report.files[1].sha256_hex(), "02".repeat(32));
}

#[test]
fn stopping_early_cancels_items_that_never_started() {
    let manifest = two_file_manifest();
    let queue = TransferQueue::from_manifest(&manifest).unwrap();
    queue.begin().unwrap();
    queue.activate(0).unwrap();
    queue.fail(0, "disk full").unwrap();
    queue.cancel_pending_from(1);

    let status = queue.finish();
    let report = queue.report();
    assert_eq!(status, QueueStatus::Failed);
    assert_eq!(report.failed_files, 1);
    assert_eq!(report.files[0].status, QueueItemStatus::Failed);
    assert_eq!(report.files[1].status, QueueItemStatus::Cancelled);
    assert_eq!(queue.snapshot().pending_files, 0);
}

#[test]
fn queue_rejects_reuse_and_cancellation_marks_unfinished_items() {
    let manifest = two_file_manifest();
    let queue = TransferQueue::from_manifest(&manifest).unwrap();
    assert!(queue.install_manifest(&manifest).is_err());

    queue.begin().unwrap();
    queue.activate(0).unwrap();
    queue.cancel();
    assert_eq!(queue.status(), QueueStatus::Cancelled);
    assert!(queue
        .items()
        .iter()
        .all(|item| item.status == QueueItemStatus::Cancelled));
}

#[tokio::test]
async fn manifest_preamble_roundtrips_over_a_stream() {
    let manifest = two_file_manifest();
    let mut bytes = Vec::new();
    write_manifest(&mut bytes, &manifest).await.unwrap();
    assert_eq!(
        u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize,
        bytes.len() - 4
    );
    let mut slice = bytes.as_slice();
    assert_eq!(read_manifest(&mut slice).await.unwrap(), manifest);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_batch_transfers_every_file() {
    let source = TempDir::new("source");
    let destination = TempDir::new("destination");
    std::fs::write(source.join("empty.bin"), b"").unwrap();
    std::fs::write(source.join("small.txt"), vec![b'a'; 1000]).unwrap();
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/big.bin"), vec![b'b'; 3000]).unwrap();

    let mut batch = PreparedBatch::new(42);
    batch
        .add_file(source.join("empty.bin"), "empty.bin")
        .await
        .unwrap();
    batch
        .add_file(source.join("small.txt"), "docs/small.txt")
        .await
        .unwrap();
    batch
        .add_file(source.join("nested/big.bin"), "nested/big.bin")
        .await
        .unwrap();

    let options = BatchOptions {
        chunk_size: 1024,
        progress_interval: Duration::ZERO,
        ..BatchOptions::new()
    };
    let token = CancelToken::new();
    let (client, server) = tokio::io::duplex(64 * 1024);

    let send_queue = TransferQueue::empty();
    let send_events = Arc::new(Mutex::new(Vec::new()));
    let sender = {
        let token = token.clone();
        let queue = send_queue.clone();
        let events = Arc::clone(&send_events);
        tokio::spawn(async move {
            upload_batch(client, &batch, &queue, &options, &token, |event| {
                events.lock().unwrap().push(event);
            })
            .await
        })
    };

    let receive_queue = TransferQueue::empty();
    let receive_events = Arc::new(Mutex::new(Vec::new()));
    let receive_report = receive_batch(
        server,
        &receive_queue,
        destination.path(),
        &options,
        &token,
        |event| receive_events.lock().unwrap().push(event),
    )
    .await
    .unwrap();
    let send_report = sender.await.unwrap().unwrap();

    for report in [&send_report, &receive_report] {
        assert_eq!(report.status, QueueStatus::Completed);
        assert_eq!(report.total_files, 3);
        assert_eq!(report.completed_files, 3);
        assert_eq!(report.failed_files, 0);
        assert_eq!(report.total_bytes, 4000);
        assert_eq!(report.transferred_bytes, 4000);
        assert!(report.is_success());
    }

    assert_eq!(send_queue.status(), QueueStatus::Completed);
    assert_eq!(receive_queue.status(), QueueStatus::Completed);
    assert_eq!(std::fs::read(destination.join("empty.bin")).unwrap(), b"");
    assert_eq!(
        std::fs::read(destination.join("docs/small.txt")).unwrap(),
        vec![b'a'; 1000]
    );
    assert_eq!(
        std::fs::read(destination.join("nested/big.bin")).unwrap(),
        vec![b'b'; 3000]
    );

    for events in [&send_events, &receive_events] {
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BatchEvent::Started { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BatchEvent::FileStarted { .. }))
                .count(),
            3
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BatchEvent::FileCompleted { .. }))
                .count(),
            3
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, BatchEvent::Progress { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BatchEvent::Finished { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupted_source_is_a_recoverable_item_failure() {
    let source = TempDir::new("corrupt-source");
    let destination = TempDir::new("corrupt-destination");
    let path = source.join("changed.bin");
    std::fs::write(&path, vec![b'x'; 3000]).unwrap();

    let mut batch = PreparedBatch::new(7);
    batch.add_file(&path, "changed.bin").await.unwrap();
    // Change the bytes after hashing but keep the size identical: the
    // manifest digest no longer matches what the sender streams.
    std::fs::write(&path, vec![b'y'; 3000]).unwrap();

    let options = BatchOptions {
        chunk_size: 1024,
        progress_interval: Duration::ZERO,
        ..BatchOptions::new()
    };
    let token = CancelToken::new();
    let (client, server) = tokio::io::duplex(64 * 1024);

    let send_queue = TransferQueue::empty();
    let sender = {
        let token = token.clone();
        let queue = send_queue.clone();
        tokio::spawn(async move {
            upload_batch(client, &batch, &queue, &options, &token, |_event| {}).await
        })
    };
    let receive_queue = TransferQueue::empty();
    let receive_report = receive_batch(
        server,
        &receive_queue,
        destination.path(),
        &options,
        &token,
        |_event| {},
    )
    .await
    .unwrap();
    let send_report = sender.await.unwrap().unwrap();

    assert_eq!(send_report.failed_files, 1);
    assert_eq!(receive_report.failed_files, 1);
    assert_eq!(send_report.status, QueueStatus::Failed);
    assert_eq!(receive_report.status, QueueStatus::Failed);
    assert_eq!(receive_queue.status(), QueueStatus::Failed);
    assert!(!destination.join("changed.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_on_error_cancels_unattempted_items() {
    let source = TempDir::new("stop-source");
    let destination = TempDir::new("stop-destination");
    let changed = source.join("changed.bin");
    std::fs::write(&changed, vec![b'x'; 3000]).unwrap();
    std::fs::write(source.join("later.bin"), vec![b'l'; 128]).unwrap();

    let mut batch = PreparedBatch::new(8);
    batch.add_file(&changed, "changed.bin").await.unwrap();
    batch
        .add_file(source.join("later.bin"), "later.bin")
        .await
        .unwrap();
    std::fs::write(&changed, vec![b'y'; 3000]).unwrap();

    let options = BatchOptions {
        chunk_size: 1024,
        progress_interval: Duration::ZERO,
        continue_on_error: false,
        ..BatchOptions::new()
    };
    let token = CancelToken::new();
    let (client, server) = tokio::io::duplex(64 * 1024);

    let send_queue = TransferQueue::empty();
    let sender = {
        let token = token.clone();
        let queue = send_queue.clone();
        tokio::spawn(async move {
            upload_batch(client, &batch, &queue, &options, &token, |_event| {}).await
        })
    };
    let receive_queue = TransferQueue::empty();
    let receive_report = receive_batch(
        server,
        &receive_queue,
        destination.path(),
        &options,
        &token,
        |_event| {},
    )
    .await
    .unwrap();
    let send_report = sender.await.unwrap().unwrap();

    for report in [&send_report, &receive_report] {
        assert_eq!(report.status, QueueStatus::Failed);
        assert_eq!(report.total_files, 2);
        assert_eq!(report.completed_files, 0);
        assert_eq!(report.failed_files, 1);
        assert_eq!(report.files[0].status, QueueItemStatus::Failed);
        assert_eq!(report.files[1].status, QueueItemStatus::Cancelled);
    }
    assert_eq!(send_queue.snapshot().pending_files, 0);
    assert_eq!(receive_queue.snapshot().pending_files, 0);
    assert!(!destination.join("later.bin").exists());
}

#[tokio::test]
async fn cancellation_before_the_manifest_marks_the_queue_cancelled() {
    let source = TempDir::new("cancel-source");
    let path = source.join("file.bin");
    std::fs::write(&path, b"payload").unwrap();
    let mut batch = PreparedBatch::new(11);
    batch.add_file(&path, "file.bin").await.unwrap();

    let options = BatchOptions::new();
    let token = CancelToken::new();
    token.cancel();

    let (client, _server) = tokio::io::duplex(1024);
    let send_queue = TransferQueue::empty();
    let send_error = upload_batch(client, &batch, &send_queue, &options, &token, |_event| {})
        .await
        .unwrap_err();
    assert!(matches!(send_error, BatchError::Cancelled));
    assert_eq!(send_queue.status(), QueueStatus::Cancelled);

    let destination = TempDir::new("cancel-destination");
    let (_client, server) = tokio::io::duplex(1024);
    let receive_queue = TransferQueue::empty();
    let receive_error = receive_batch(
        server,
        &receive_queue,
        destination.path(),
        &options,
        &token,
        |_event| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(receive_error, BatchError::Cancelled));
    assert_eq!(receive_queue.status(), QueueStatus::Cancelled);
}

#[tokio::test]
async fn empty_batches_fail_before_any_io() {
    let (client, _server) = tokio::io::duplex(1024);
    let batch = PreparedBatch::new(1);
    let queue = TransferQueue::empty();
    let result = upload_batch(
        client,
        &batch,
        &queue,
        &BatchOptions::new(),
        &CancelToken::new(),
        |_event| {},
    )
    .await;
    assert!(result.is_err());
    assert_eq!(queue.status(), QueueStatus::Idle);
}
