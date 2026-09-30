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
    stream_to_writer_limited(stream, writer, None, None).await
}

/// Like [`stream_to_writer`], but aborts with [`std::io::ErrorKind::InvalidData`]
/// as soon as more than `max_bytes` have been read. The check happens *before*
/// any bytes beyond the limit are written, so a hostile client cannot fill the
/// disk past the configured cap.
///
/// With `idle` set, it also gives up with [`std::io::ErrorKind::TimedOut`] once
/// no chunk has arrived for that long. An upload that simply stops sending
/// would otherwise hold its connection and its temp file open for as long as
/// the client likes: nothing else in the stack times a request body out.
pub async fn stream_to_writer_limited<S, W>(
    mut stream: S,
    writer: &mut W,
    max_bytes: Option<u64>,
    idle: Option<std::time::Duration>,
) -> std::io::Result<StreamSummary>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hasher = Sha512::new();
    let mut size: u64 = 0;

    while let Some(chunk) = next_chunk(&mut stream, idle).await? {
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

/// Append a byte stream to `writer`, feeding every chunk into `hasher` once it
/// is written, and refuse — with [`std::io::ErrorKind::InvalidData`], before
/// writing — any chunk that would take the total past `remaining` bytes.
///
/// Returns how many bytes were written *and* how the stream ended, separately:
/// a resumable upload keeps what arrived before a dropped connection, so the
/// caller needs the count even when the result is an error. `hasher` covers
/// exactly the bytes counted, so a resumed transfer can go on hashing where it
/// stopped.
pub async fn append_hashed<S, W, D>(
    stream: &mut S,
    writer: &mut W,
    hasher: &mut D,
    remaining: u64,
    idle: Option<std::time::Duration>,
) -> (u64, std::io::Result<()>)
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
    D: Digest,
{
    let mut written: u64 = 0;
    let result = async {
        while let Some(chunk) = next_chunk(stream, idle).await? {
            let chunk = chunk?;
            if written.saturating_add(chunk.len() as u64) > remaining {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("more than the {remaining} bytes expected"),
                ));
            }
            writer.write_all(&chunk).await?;
            hasher.update(&chunk);
            written += chunk.len() as u64;
        }
        writer.flush().await
    }
    .await;
    (written, result)
}

/// Copy a byte stream into `writer`, hashing it with SHA-256 on the way, and
/// return its size and lower-case hex digest. Limits as for
/// [`stream_to_writer_limited`].
pub async fn stream_to_writer_sha256<S, W>(
    mut stream: S,
    writer: &mut W,
    max_bytes: Option<u64>,
    idle: Option<std::time::Duration>,
) -> std::io::Result<(u64, String)>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut hasher = sha2::Sha256::new();
    let (size, result) = append_hashed(
        &mut stream,
        writer,
        &mut hasher,
        max_bytes.unwrap_or(u64::MAX),
        idle,
    )
    .await;
    result?;
    Ok((size, hex::encode(hasher.finalize())))
}

/// The next item of `stream`, or a [`std::io::ErrorKind::TimedOut`] error when
/// `idle` passes without one.
pub(crate) async fn next_chunk<S>(
    stream: &mut S,
    idle: Option<std::time::Duration>,
) -> std::io::Result<Option<S::Item>>
where
    S: Stream + Unpin,
{
    match idle {
        None => Ok(stream.next().await),
        Some(limit) => tokio::time::timeout(limit, stream.next())
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no data arrived for {} s", limit.as_secs()),
                )
            }),
    }
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
        let result = stream_to_writer_limited(stream, &mut sink, Some(5000), None).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn sha256_hashing_resumes_where_it_stopped() {
        // "abc" in one go and in two appends give the same, known digest.
        let (size, hex) = stream_to_writer_sha256(
            futures::stream::iter([Ok(Bytes::from_static(b"abc"))]),
            &mut tokio::io::sink(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut hasher = sha2::Sha256::new();
        let mut sink = Vec::new();
        for part in [&b"a"[..], &b"bc"[..]] {
            let mut s = futures::stream::iter([Ok(Bytes::copy_from_slice(part))]);
            let (n, r) = append_hashed(&mut s, &mut sink, &mut hasher, 3, None).await;
            r.unwrap();
            assert_eq!(n as usize, part.len());
        }
        assert_eq!(hex::encode(hasher.finalize()), hex);
        // More than declared is refused before it is written.
        let mut s = futures::stream::iter([Ok(Bytes::from_static(b"abcd"))]);
        let (n, r) =
            append_hashed(&mut s, &mut Vec::new(), &mut sha2::Sha256::new(), 3, None).await;
        assert_eq!(n, 0);
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn gives_up_on_a_stalled_stream() {
        // One chunk, then silence: the stream never ends and never errors.
        let first = futures::stream::once(async { Ok(Bytes::from_static(b"abc")) });
        let stream = Box::pin(first.chain(futures::stream::pending()));
        let mut sink = tokio::io::sink();
        let started = std::time::Instant::now();
        let err = stream_to_writer_limited(
            stream,
            &mut sink,
            None,
            Some(std::time::Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
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
