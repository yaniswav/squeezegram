//! Content identity `size:blake3`, computed off the async runtime.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use super::{MediaError, Result};

const CHUNK: usize = 16 * 1024 * 1024;

/// Hashes the file in 16 MiB chunks (each one spread over the CPU cores by BLAKE3), checking
/// `cancel` and reporting the fraction done between chunks.
pub fn content_id_blocking(
    path: &Path,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(f64),
) -> Result<String> {
    let unreadable = |e: std::io::Error| MediaError::failed("FILE_UNREADABLE", e.to_string());
    let mut file = File::open(path).map_err(unreadable)?;
    let total = file.metadata().map_err(unreadable)?.len();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        let filled = fill(&mut file, &mut buffer).map_err(unreadable)?;
        if filled == 0 {
            break;
        }
        hasher.update_rayon(&buffer[..filled]);
        done += filled as u64;
        on_progress(if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        });
    }
    Ok(format!("{done}:{}", hasher.finalize().to_hex()))
}

/// Reads until the buffer is full or the file ends.
fn fill(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// Same, on a blocking thread.
pub async fn content_id(
    path: std::path::PathBuf,
    cancel: CancellationToken,
    on_progress: impl FnMut(f64) + Send + 'static,
) -> Result<String> {
    tokio::task::spawn_blocking(move || content_id_blocking(&path, &cancel, on_progress))
        .await
        .map_err(|e| MediaError::failed("IO_ERROR", e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    #[test]
    fn identity_is_size_and_blake3_of_the_content() {
        let dir = TestDir::new("hash");
        let path = dir.path().join("a.bin");
        let content: Vec<u8> = (0..40_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();

        let id = content_id_blocking(&path, &CancellationToken::new(), |_| {}).unwrap();
        let expected = format!("{}:{}", content.len(), blake3::hash(&content).to_hex());
        assert_eq!(id, expected, "chunked hashing equals hashing in one go");

        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        let id = content_id_blocking(&empty, &CancellationToken::new(), |_| {}).unwrap();
        assert_eq!(id, format!("0:{}", blake3::hash(b"").to_hex()));
    }

    #[test]
    fn progress_grows_and_cancel_stops_the_hash() {
        let dir = TestDir::new("hash-cancel");
        let path = dir.path().join("a.bin");
        std::fs::write(&path, vec![7u8; 40_000_000]).unwrap();

        let mut seen = Vec::new();
        content_id_blocking(&path, &CancellationToken::new(), |p| seen.push(p)).unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen.windows(2).all(|w| w[0] < w[1]) && seen[2] == 1.0);

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let result = content_id_blocking(&path, &cancel, |_| token.cancel());
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }

    #[test]
    fn a_missing_file_is_unreadable() {
        let error = content_id_blocking(
            Path::new("Z:\\no\\such\\file.bin"),
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error.code(), Some("FILE_UNREADABLE"));
    }
}
