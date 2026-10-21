//! Canonical multi-file manifests.
//!
//! A batch is agreed before a single payload byte moves. Both sides must see
//! exactly the same ordered list of relative paths, sizes and SHA-256 digests;
//! the sender then opens one per-file transfer per entry, and the receiver can
//! pre-create directories, resume each part file independently and reject a
//! `HELLO` that disagrees with the manifest.
//!
//! # Wire format
//!
//! The manifest is a self-delimiting binary document carried behind a 32-bit
//! big-endian length (see [`crate::read_manifest`]). Its body is:
//!
//! ```text
//! magic        "ABMF"       4 bytes
//! version      u16 BE       1
//! flags        u16 BE       must be zero
//! batch_id     u64 BE       random identifier chosen by the sender
//! file_count   u32 BE
//! total_bytes  u64 BE       sum of every entry's size
//! entries      file_count times:
//!   path_len   u16 BE       UTF-8 length of the relative path
//!   path       path_len bytes, canonical `/` separators
//!   size       u64 BE
//!   sha256     32 bytes
//! ```
//!
//! Every length is checked against a hard maximum *before* it drives an
//! allocation, paths are rejected unless they are strictly relative, and the
//! decoder rejects truncation, trailing bytes, duplicate paths and a
//! `total_bytes` that disagrees with the entries. The result is canonical:
//! the same manifest always produces the same bytes.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use ring::digest;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use libcore_transport::PayloadId;

/// Magic at the start of every encoded manifest.
pub const MANIFEST_MAGIC: [u8; 4] = *b"ABMF";

/// Current manifest version.
pub const MANIFEST_VERSION: u16 = 1;

/// Largest number of entries a manifest may contain.
pub const MAX_MANIFEST_FILES: u32 = 100_000;

/// Largest accepted UTF-8 length of one relative path.
pub const MAX_MANIFEST_PATH_BYTES: usize = 4096;

/// Largest encoded manifest, including the header and all entries.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;

/// Length of the fixed manifest header.
pub const MANIFEST_HEADER_LEN: usize = 4 + 2 + 2 + 8 + 4 + 8;

/// Length of one entry apart from its path.
pub const MANIFEST_ENTRY_FIXED_LEN: usize = 2 + 8 + 32;

const HASH_BUFFER_SIZE: usize = 256 * 1024;

/// A manifest could not be built, encoded or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// The path is empty.
    EmptyPath,
    /// The path is absolute, or carries a Windows drive prefix.
    AbsolutePath,
    /// The path contains `.` or `..`, an empty component, or a colon.
    InvalidComponent {
        /// The offending component.
        component: String,
    },
    /// The path contains a NUL byte.
    NulByte,
    /// The path is longer than [`MAX_MANIFEST_PATH_BYTES`].
    PathTooLong,
    /// The manifest contains more than [`MAX_MANIFEST_FILES`] entries.
    TooManyFiles,
    /// Two entries name the same relative path.
    DuplicatePath {
        /// The repeated path.
        path: String,
    },
    /// The encoded body is shorter than its header or an entry.
    Truncated,
    /// The encoded body has bytes after the last entry.
    TrailingBytes,
    /// The magic does not match [`MANIFEST_MAGIC`].
    InvalidMagic,
    /// The version is not [`MANIFEST_VERSION`].
    UnsupportedVersion(u16),
    /// Reserved flags are set.
    UnsupportedFlags(u16),
    /// A path is not valid UTF-8.
    InvalidPathEncoding,
    /// A decoded path is not in the canonical `/`-separated form.
    NonCanonicalPath {
        /// The non-canonical path.
        path: String,
    },
    /// The declared total does not equal the sum of the entries.
    InvalidTotal,
    /// The encoded manifest exceeds [`MAX_MANIFEST_BYTES`].
    ManifestTooLarge,
    /// A filesystem operation failed while preparing an entry.
    Io(String),
    /// The path exists but is not a regular file.
    NotAFile,
    /// The file changed while it was being hashed.
    FileChanged(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => f.write_str("the relative path is empty"),
            Self::AbsolutePath => f.write_str("the path must be relative"),
            Self::InvalidComponent { component } => {
                write!(f, "the path component {component:?} is not allowed")
            }
            Self::NulByte => f.write_str("the path contains a NUL byte"),
            Self::PathTooLong => write!(
                f,
                "the path exceeds the {MAX_MANIFEST_PATH_BYTES}-byte maximum"
            ),
            Self::TooManyFiles => {
                write!(
                    f,
                    "the manifest exceeds the {MAX_MANIFEST_FILES}-file maximum"
                )
            }
            Self::DuplicatePath { path } => write!(f, "the path {path:?} appears twice"),
            Self::Truncated => f.write_str("the manifest is truncated"),
            Self::TrailingBytes => f.write_str("the manifest has trailing bytes"),
            Self::InvalidMagic => f.write_str("the manifest magic is not ABMF"),
            Self::UnsupportedVersion(version) => {
                write!(f, "manifest version {version} is not supported")
            }
            Self::UnsupportedFlags(flags) => {
                write!(f, "manifest flags {flags:#06x} are not supported")
            }
            Self::InvalidPathEncoding => f.write_str("a path is not valid UTF-8"),
            Self::NonCanonicalPath { path } => {
                write!(f, "the manifest path {path:?} is not canonical")
            }
            Self::InvalidTotal => f.write_str("the manifest total does not match its entries"),
            Self::ManifestTooLarge => {
                write!(
                    f,
                    "the manifest exceeds the {MAX_MANIFEST_BYTES}-byte maximum"
                )
            }
            Self::Io(detail) => write!(f, "filesystem error: {detail}"),
            Self::NotAFile => f.write_str("the path is not a regular file"),
            Self::FileChanged(path) => write!(f, "the file {path:?} changed while hashing"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<std::io::Error> for ManifestError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Normalizes and validates a relative path for the wire format.
///
/// Backslashes are treated as separators and rewritten to `/`. Absolute paths,
/// drive prefixes, empty components, `.`, `..`, NUL bytes and colons are
/// rejected. The result is the canonical form stored in [`ManifestEntry`].
///
/// # Errors
///
/// Returns the matching [`ManifestError`] when the path is not a safe,
/// strictly relative UTF-8 path.
pub fn normalize_relative_path(raw: &str) -> Result<String, ManifestError> {
    if raw.is_empty() {
        return Err(ManifestError::EmptyPath);
    }
    if raw.contains('\0') {
        return Err(ManifestError::NulByte);
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(ManifestError::AbsolutePath);
    }

    let mut normalized = String::with_capacity(raw.len());
    for component in raw.split(&['/', '\\'][..]) {
        if component.is_empty() || component == "." || component == ".." || component.contains(':')
        {
            return Err(ManifestError::InvalidComponent {
                component: component.to_owned(),
            });
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    if normalized.is_empty() {
        return Err(ManifestError::EmptyPath);
    }
    if normalized.len() > MAX_MANIFEST_PATH_BYTES {
        return Err(ManifestError::PathTooLong);
    }
    Ok(normalized)
}

/// One file in a batch, as agreed on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    relative_path: String,
    size: u64,
    sha256: [u8; 32],
}

impl ManifestEntry {
    /// Creates an entry, validating and normalizing its relative path.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for an unsafe path.
    pub fn new(
        relative_path: impl Into<String>,
        size: u64,
        sha256: [u8; 32],
    ) -> Result<Self, ManifestError> {
        let relative_path = normalize_relative_path(&relative_path.into())?;
        Ok(Self {
            relative_path,
            size,
            sha256,
        })
    }

    /// Hashes `path` and records it under `relative_path`.
    ///
    /// The file is read once in 256 KiB blocks; the size is re-checked after
    /// hashing so a file that grows or shrinks during preparation is rejected
    /// instead of producing an entry that can never verify.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::NotAFile`] for directories and special files,
    /// [`ManifestError::FileChanged`] when the file changes while hashing, and
    /// the underlying I/O error string on read failures.
    pub async fn from_path(
        path: impl AsRef<Path>,
        relative_path: impl Into<String>,
    ) -> Result<Self, ManifestError> {
        let relative_path = normalize_relative_path(&relative_path.into())?;
        let path = path.as_ref();
        let before = tokio::fs::metadata(path).await?;
        if !before.is_file() {
            return Err(ManifestError::NotAFile);
        }
        let size = before.len();
        let sha256 = hash_file(path).await?;
        let after = tokio::fs::metadata(path).await?;
        if after.len() != size || !after.is_file() {
            return Err(ManifestError::FileChanged(
                path.to_string_lossy().into_owned(),
            ));
        }
        Ok(Self {
            relative_path,
            size,
            sha256,
        })
    }

    /// The canonical `/`-separated relative path.
    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    /// Size of the file in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Whole-file SHA-256.
    #[must_use]
    pub const fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Lower-case hexadecimal SHA-256, for logs and foreign bindings.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        hex(&self.sha256)
    }
}

/// An ordered file list plus the batch identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferManifest {
    batch_id: u64,
    entries: Vec<ManifestEntry>,
}

impl TransferManifest {
    /// Creates an empty manifest.
    #[must_use]
    pub const fn new(batch_id: u64) -> Self {
        Self {
            batch_id,
            entries: Vec::new(),
        }
    }

    /// Creates a manifest from already-validated entries.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for duplicate paths or too many entries.
    pub fn from_entries(
        batch_id: u64,
        entries: impl IntoIterator<Item = ManifestEntry>,
    ) -> Result<Self, ManifestError> {
        let mut manifest = Self::new(batch_id);
        for entry in entries {
            manifest.push(entry)?;
        }
        Ok(manifest)
    }

    /// Appends one entry, preserving order.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::DuplicatePath`] or
    /// [`ManifestError::TooManyFiles`] when the entry cannot be added.
    pub fn push(&mut self, entry: ManifestEntry) -> Result<(), ManifestError> {
        if self.entries.len() >= MAX_MANIFEST_FILES as usize {
            return Err(ManifestError::TooManyFiles);
        }
        if self
            .entries
            .iter()
            .any(|existing| existing.relative_path == entry.relative_path)
        {
            return Err(ManifestError::DuplicatePath {
                path: entry.relative_path,
            });
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Identifier chosen by the sender.
    #[must_use]
    pub const fn batch_id(&self) -> u64 {
        self.batch_id
    }

    /// Entries in transfer order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Number of files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the manifest is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sum of every entry's size.
    ///
    /// # Panics
    ///
    /// Never in practice: [`Self::validate`] rejects an overflowing total, and
    /// manifests are validated before they reach this method. The saturating
    /// fallback keeps a malformed in-memory manifest from panicking a worker.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .fold(0_u64, |total, entry| total.saturating_add(entry.size))
    }

    /// Stable payload id for the entry at `index`.
    ///
    /// The derivation is a bijective 64-bit mix of the batch id and the index,
    /// so the sender and receiver agree without extra negotiation and a retry
    /// of the same batch reuses the same ids (and therefore the same resume
    /// state).
    #[must_use]
    pub fn payload_id(&self, index: usize) -> Option<PayloadId> {
        (index < self.entries.len()).then(|| payload_id_for(self.batch_id, index))
    }

    /// Checks ordering invariants, duplicate paths and the total.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the manifest cannot be encoded or
    /// transferred safely.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.entries.len() > MAX_MANIFEST_FILES as usize {
            return Err(ManifestError::TooManyFiles);
        }
        let mut seen = HashSet::with_capacity(self.entries.len());
        let mut total = 0_u64;
        for entry in &self.entries {
            if !seen.insert(entry.relative_path.as_str()) {
                return Err(ManifestError::DuplicatePath {
                    path: entry.relative_path.clone(),
                });
            }
            total = total
                .checked_add(entry.size)
                .ok_or(ManifestError::InvalidTotal)?;
        }
        Ok(())
    }

    /// Encodes the manifest in the canonical wire format.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when the manifest is invalid or larger than
    /// [`MAX_MANIFEST_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        let count = u32::try_from(self.entries.len()).map_err(|_| ManifestError::TooManyFiles)?;
        let mut encoded_len = MANIFEST_HEADER_LEN;
        for entry in &self.entries {
            let path_len = entry.relative_path.len();
            if path_len > MAX_MANIFEST_PATH_BYTES {
                return Err(ManifestError::PathTooLong);
            }
            encoded_len = encoded_len
                .checked_add(2 + path_len + 8 + 32)
                .ok_or(ManifestError::ManifestTooLarge)?;
            if encoded_len > MAX_MANIFEST_BYTES {
                return Err(ManifestError::ManifestTooLarge);
            }
        }
        let mut out = Vec::with_capacity(encoded_len);
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&MANIFEST_VERSION.to_be_bytes());
        out.extend_from_slice(&0_u16.to_be_bytes());
        out.extend_from_slice(&self.batch_id.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&self.total_bytes().to_be_bytes());

        for entry in &self.entries {
            let path = entry.relative_path.as_bytes();
            let path_len = u16::try_from(path.len()).map_err(|_| ManifestError::PathTooLong)?;
            out.extend_from_slice(&path_len.to_be_bytes());
            out.extend_from_slice(path);
            out.extend_from_slice(&entry.size.to_be_bytes());
            out.extend_from_slice(&entry.sha256);
        }

        if out.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::ManifestTooLarge);
        }
        Ok(out)
    }

    /// Decodes and validates a canonical manifest.
    ///
    /// The input is treated as untrusted until every length, count, path and
    /// total has been checked.
    ///
    /// # Errors
    ///
    /// Returns the first structural or validation error encountered.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::ManifestTooLarge);
        }
        if bytes.len() < MANIFEST_HEADER_LEN {
            return Err(ManifestError::Truncated);
        }
        if bytes[..4] != MANIFEST_MAGIC {
            return Err(ManifestError::InvalidMagic);
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(version));
        }
        let flags = u16::from_be_bytes([bytes[6], bytes[7]]);
        if flags != 0 {
            return Err(ManifestError::UnsupportedFlags(flags));
        }
        let batch_id = u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let count = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        if count > MAX_MANIFEST_FILES {
            return Err(ManifestError::TooManyFiles);
        }
        let declared_total = u64::from_be_bytes([
            bytes[20], bytes[21], bytes[22], bytes[23], bytes[24], bytes[25], bytes[26], bytes[27],
        ]);

        let mut cursor = MANIFEST_HEADER_LEN;
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            if bytes.len().saturating_sub(cursor) < MANIFEST_ENTRY_FIXED_LEN {
                return Err(ManifestError::Truncated);
            }
            let path_len = usize::from(u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]));
            cursor += 2;
            if path_len == 0 || path_len > MAX_MANIFEST_PATH_BYTES {
                return Err(ManifestError::PathTooLong);
            }
            let path_end = cursor
                .checked_add(path_len)
                .ok_or(ManifestError::Truncated)?;
            if path_end > bytes.len() {
                return Err(ManifestError::Truncated);
            }
            let path = std::str::from_utf8(&bytes[cursor..path_end])
                .map_err(|_| ManifestError::InvalidPathEncoding)?;
            cursor = path_end;

            if bytes.len().saturating_sub(cursor) < 8 + 32 {
                return Err(ManifestError::Truncated);
            }
            let size = u64::from_be_bytes([
                bytes[cursor],
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
                bytes[cursor + 4],
                bytes[cursor + 5],
                bytes[cursor + 6],
                bytes[cursor + 7],
            ]);
            cursor += 8;
            let mut sha256 = [0_u8; 32];
            sha256.copy_from_slice(&bytes[cursor..cursor + 32]);
            cursor += 32;

            let entry = ManifestEntry::new(path, size, sha256)?;
            if entry.relative_path() != path {
                return Err(ManifestError::NonCanonicalPath {
                    path: path.to_owned(),
                });
            }
            entries.push(entry);
        }
        if cursor != bytes.len() {
            return Err(ManifestError::TrailingBytes);
        }

        let manifest = Self::from_entries(batch_id, entries)?;
        manifest.validate()?;
        if manifest.total_bytes() != declared_total {
            return Err(ManifestError::InvalidTotal);
        }
        Ok(manifest)
    }
}

impl Default for TransferManifest {
    fn default() -> Self {
        Self::new(0)
    }
}

/// A file ready to upload: the wire entry plus the local path to read.
#[derive(Clone, Debug)]
pub struct PreparedFile {
    entry: ManifestEntry,
    source: PathBuf,
}

impl PreparedFile {
    /// Hashes `source` and binds it to `relative_path`.
    ///
    /// # Errors
    ///
    /// Same as [`ManifestEntry::from_path`].
    pub async fn from_path(
        source: impl Into<PathBuf>,
        relative_path: impl Into<String>,
    ) -> Result<Self, ManifestError> {
        let source = source.into();
        let entry = ManifestEntry::from_path(&source, relative_path).await?;
        Ok(Self { entry, source })
    }

    /// The manifest entry this file will send.
    #[must_use]
    pub const fn entry(&self) -> &ManifestEntry {
        &self.entry
    }

    /// Local source path.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
}

/// An ordered, immutable-enough outgoing batch.
#[derive(Clone, Debug)]
pub struct PreparedBatch {
    batch_id: u64,
    files: Vec<PreparedFile>,
}

impl PreparedBatch {
    /// Creates an empty outgoing batch.
    #[must_use]
    pub const fn new(batch_id: u64) -> Self {
        Self {
            batch_id,
            files: Vec::new(),
        }
    }

    /// Batch identifier.
    #[must_use]
    pub const fn batch_id(&self) -> u64 {
        self.batch_id
    }

    /// Prepared files in transfer order.
    #[must_use]
    pub fn files(&self) -> &[PreparedFile] {
        &self.files
    }

    /// Number of files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the batch has no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Adds an already-prepared file.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for duplicate paths or too many files.
    pub fn push(&mut self, file: PreparedFile) -> Result<(), ManifestError> {
        self.push_many([file])
    }

    /// Adds several files atomically: either every entry is accepted or the
    /// batch is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for duplicate paths or too many files.
    pub fn push_many(
        &mut self,
        files: impl IntoIterator<Item = PreparedFile>,
    ) -> Result<(), ManifestError> {
        let mut pending: Vec<PreparedFile> = files.into_iter().collect();
        if self.files.len() + pending.len() > MAX_MANIFEST_FILES as usize {
            return Err(ManifestError::TooManyFiles);
        }
        let mut seen: HashSet<String> = self
            .files
            .iter()
            .map(|file| file.entry.relative_path.clone())
            .collect();
        for file in &pending {
            if !seen.insert(file.entry.relative_path.clone()) {
                return Err(ManifestError::DuplicatePath {
                    path: file.entry.relative_path.clone(),
                });
            }
        }
        self.files.append(&mut pending);
        Ok(())
    }

    /// Hashes `path` and adds it under `relative_path`.
    ///
    /// # Errors
    ///
    /// Same as [`ManifestEntry::from_path`], plus duplicate/invalid path
    /// errors from [`Self::push`].
    pub async fn add_file(
        &mut self,
        path: impl AsRef<Path>,
        relative_path: impl Into<String>,
    ) -> Result<(), ManifestError> {
        let file = PreparedFile::from_path(path.as_ref().to_path_buf(), relative_path).await?;
        self.push(file)
    }

    /// Recursively adds every regular file under `directory`.
    ///
    /// The directory's own name is used as the path prefix, so
    /// `/photos/2026/a.jpg` becomes `photos/2026/a.jpg`. Symbolic links are
    /// skipped rather than followed: a directory scan must not be able to
    /// escape its root. Files are sorted by relative path so the manifest is
    /// deterministic across filesystems.
    ///
    /// # Errors
    ///
    /// Returns an I/O or path error when the directory cannot be read or a
    /// name is not valid UTF-8. The batch is unchanged on any error.
    pub async fn add_directory(
        &mut self,
        directory: impl AsRef<Path>,
    ) -> Result<(), ManifestError> {
        let directory = directory.as_ref();
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ManifestError::EmptyPath)?;
        let prefix = normalize_relative_path(name)?;

        let mut paths = Vec::new();
        collect_regular_files(directory, &mut paths).await?;
        paths.sort();

        let mut prepared = Vec::with_capacity(paths.len());
        for path in paths {
            let relative = relative_under(directory, &path, &prefix)?;
            prepared.push(PreparedFile::from_path(path, relative).await?);
        }
        self.push_many(prepared)
    }

    /// Builds the wire manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] when an entry is invalid or the manifest
    /// exceeds the protocol maxima.
    pub fn manifest(&self) -> Result<TransferManifest, ManifestError> {
        TransferManifest::from_entries(
            self.batch_id,
            self.files.iter().map(|file| file.entry.clone()),
        )
    }
}

/// Derives the stable payload id for `index` inside `batch_id`.
///
/// The mixing function is SplitMix64's finalizer, which is a bijection. For a
/// fixed batch id, distinct indices therefore always map to distinct payload
/// ids; the random batch id separates concurrent batches.
#[must_use]
pub fn payload_id_for(batch_id: u64, index: usize) -> PayloadId {
    let mut value = batch_id ^ (index as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    PayloadId::new(value ^ (value >> 31))
}

async fn hash_file(path: &Path) -> Result<[u8; 32], ManifestError> {
    let mut file = File::open(path).await?;
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    let mut hasher = digest::Context::new(&digest::SHA256);
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(hasher.finish().as_ref());
    Ok(out)
}

async fn collect_regular_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), ManifestError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let mut entries = Vec::new();
        let mut reader = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = reader.next_entry().await? {
            entries.push(entry);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn relative_under(root: &Path, path: &Path, prefix: &str) -> Result<String, ManifestError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ManifestError::AbsolutePath)?;
    let mut out = prefix.to_owned();
    for component in relative.components() {
        let name = component
            .as_os_str()
            .to_str()
            .ok_or(ManifestError::InvalidPathEncoding)?;
        if name.is_empty() {
            continue;
        }
        out.push('/');
        out.push_str(name);
    }
    normalize_relative_path(&out)
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
    }
    out
}
