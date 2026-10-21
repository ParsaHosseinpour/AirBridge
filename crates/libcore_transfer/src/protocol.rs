//! The batch preamble.
//!
//! A batch connection carries one length-prefixed manifest before the first
//! `HELLO`. The transport frames that follow are unchanged: the manifest is an
//! application preamble on the same authenticated stream, not a new transport
//! frame tag.
//!
//! ```text
//! batch connection:
//!   u32 BE manifest_len | manifest bytes | HELLO(file 0) | chunks | COMPLETE
//!                       | HELLO(file 1) | chunks | COMPLETE | ...
//! ```
//!
//! The length is checked against [`MAX_MANIFEST_BYTES`] before allocating.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::BatchError;
use crate::manifest::{TransferManifest, MAX_MANIFEST_BYTES};

/// Writes a manifest preamble and flushes it.
///
/// # Errors
///
/// Returns a manifest error when encoding fails, or an I/O error when the
/// stream fails.
pub async fn write_manifest<S>(
    stream: &mut S,
    manifest: &TransferManifest,
) -> Result<(), BatchError>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    let bytes = manifest.encode()?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| BatchError::Protocol("manifest length does not fit in u32".to_owned()))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(BatchError::Protocol(format!(
            "manifest exceeds the {MAX_MANIFEST_BYTES}-byte protocol maximum"
        )));
    }
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Reads and validates a manifest preamble.
///
/// # Errors
///
/// Returns a protocol error for a length outside the accepted window, a
/// manifest decode error, or an I/O error when the stream fails.
pub async fn read_manifest<S>(stream: &mut S) -> Result<TransferManifest, BatchError>
where
    S: AsyncRead + Unpin + ?Sized,
{
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix).await?;
    let length = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| BatchError::Protocol("manifest length does not fit in usize".to_owned()))?;
    if length == 0 {
        return Err(BatchError::Protocol(
            "the peer sent an empty manifest preamble".to_owned(),
        ));
    }
    if length > MAX_MANIFEST_BYTES {
        return Err(BatchError::Protocol(format!(
            "manifest of {length} bytes exceeds the {MAX_MANIFEST_BYTES}-byte maximum"
        )));
    }
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes).await?;
    Ok(TransferManifest::decode(&bytes)?)
}
