//! Streaming helpers for handling arbitrarily large package payloads.
//!
//! The functions here deliberately never materialize a whole package in
//! memory. A 25 GiB upload is written to disk chunk-by-chunk while its SHA-512
//! hash is computed incrementally, so peak memory stays at a single buffer.

use base64::Engine;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use sha2::{Digest, Sha512};
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// Result of streaming a payload to disk: the number of bytes written and the
/// base64-encoded SHA-512 of the content.
#[derive(Debug, Clone)]
pub struct StreamSummary {
    pub size: u64,
    pub sha512_base64: String,
}

/// Copy a byte stream into `writer`, computing the SHA-512 hash on the fly.
///
/// The writer is flushed but **not** closed; the caller owns its lifecycle.
/// Memory use is bounded by the size of the individual chunks yielded by the
/// stream, regardless of the total payload size.
pub async fn stream_to_writer<S, W>(stream: S, writer: &mut W) -> std::io::Result<StreamSummary>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
{
    stream_to_writer_limited(stream, writer, None).await
}

/// Like [`stream_to_writer`], but aborts with [`std::io::ErrorKind::InvalidData`]
/// as soon as more than `max_bytes` have been read. The check happens *before*
/// any bytes beyond the limit are written, so a hostile client cannot fill the
/// disk past the configured cap.
pub async fn stream_to_writer_limited<S, W>(
    mut stream: S,
    writer: &mut W,
    max_bytes: Option<u64>,
) -> std::io::Result<StreamSummary>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hasher = Sha512::new();
    let mut size: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size += chunk.len() as u64;
        if let Some(limit) = max_bytes {
            if size > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("payload exceeds the configured limit of {limit} bytes"),
                ));
            }
        }
        hasher.update(&chunk);
        writer.write_all(&chunk).await?;
    }
    writer.flush().await?;

    let digest = hasher.finalize();
    let sha512_base64 = base64::engine::general_purpose::STANDARD.encode(digest);
    Ok(StreamSummary {
        size,
        sha512_base64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_and_counts_known_input() {
        // SHA-512 of "abc", base64 encoded — a well known fixed vector.
        let data = Bytes::from_static(b"abc");
        let stream = Box::pin(futures::stream::once(async move { Ok(data) }));
        let mut buf: Vec<u8> = Vec::new();
        let summary = stream_to_writer(stream, &mut buf).await.unwrap();

        assert_eq!(summary.size, 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(
            summary.sha512_base64,
            "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw=="
        );
    }

    #[tokio::test]
    async fn streams_many_chunks_with_bounded_memory() {
        // 4 MiB delivered as 1 KiB chunks; verifies the running total.
        let chunks: Vec<std::io::Result<Bytes>> = (0..4096)
            .map(|_| Ok(Bytes::from(vec![0u8; 1024])))
            .collect();
        let stream = futures::stream::iter(chunks);
        let mut sink = tokio::io::sink();
        let summary = stream_to_writer(stream, &mut sink).await.unwrap();
        assert_eq!(summary.size, 4 * 1024 * 1024);
    }

    #[tokio::test]
    async fn enforces_size_limit() {
        let chunks: Vec<std::io::Result<Bytes>> =
            (0..10).map(|_| Ok(Bytes::from(vec![1u8; 1000]))).collect();
        let stream = futures::stream::iter(chunks);
        let mut sink = tokio::io::sink();
        // Limit of 5 KiB; 10 KiB will be offered.
        let result = stream_to_writer_limited(stream, &mut sink, Some(5000)).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn propagates_stream_errors() {
        let err = std::io::Error::other("boom");
        let stream = Box::pin(futures::stream::once(async move { Err(err) }));
        let mut buf: Vec<u8> = Vec::new();
        let result = stream_to_writer(stream, &mut buf).await;
        assert!(result.is_err());
    }
}
