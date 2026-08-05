# Handling very large packages (25 GB+)

YANuget's headline goal is to host packages far larger than typical NuGet
servers tolerate, **without** their memory cost scaling with package size. This
document explains exactly how every stage stays bounded, and what to watch for
when operating such a feed.

## The problem with buffering

A naive server reads the uploaded `.nupkg` into a byte array to (a) compute its
hash, (b) open it as a ZIP to read the `.nuspec`, and (c) write it to storage.
For a 25 GB package that is 25 GB of RAM per concurrent upload — immediately
fatal. Downloads have the mirror-image problem if the file is read fully before
being sent.

## How YANuget stays bounded

### Upload → temp file (one buffer of memory)

The push handler (`web::push_package` → `web::write_upload`) takes the request
body as a **stream** of chunks. Whether the client sends `multipart/form-data`
(the `dotnet`/`nuget` default) or a raw body, the bytes are handed to
[`streaming::stream_to_writer_limited`](../src/streaming.rs), which:

1. writes each chunk to a temp file, and
2. feeds each chunk into a running SHA-512 hasher,

then returns the total size and base64 hash. Peak memory is a single chunk
(tens of KB), regardless of total size. The optional size limit is enforced
*before* writing past the cap, so a hostile client cannot fill the disk.

Multipart parsing is also streaming: axum yields the file field as a chunk
stream, so the multipart envelope never forces buffering either.

### Manifest read → seek, don't scan

A `.nupkg` is a ZIP, and a ZIP's *central directory* lives at the **end** of the
file. [`nupkg::read_archive`](../src/nupkg.rs) opens the temp file and lets the
`zip` crate **seek** to that directory and then to the single `.nuspec` entry.
A 25 GB archive is therefore touched in two tiny reads (directory + manifest),
never scanned front-to-back. The blocking ZIP work runs on a
`spawn_blocking` thread so the async runtime is never stalled. The manifest read
is additionally capped (16 MiB) to reject a hostile archive that declares an
absurd nuspec size.

### Store → atomic rename (zero copy)

`FilesystemStorage::store_package` `rename`s the temp file into its final path.
When the temp directory and the store share a filesystem — which YANuget
arranges by placing temp uploads under `{storage}/.uploads` — this is an atomic,
**O(1)** metadata operation: no second copy of 25 GB is made. If they are on
different filesystems, it falls back to a streaming copy (still no buffering).

### Download → stream + Range

`web::download_package` resolves the file to a local path and serves it via
[`web::files::serve_local_file`](../src/web/files.rs), which streams the file
with `tokio_util::io::ReaderStream` and honours a `Range: bytes=...` header,
replying `206 Partial Content`. This makes 25 GB downloads **resumable** (a
dropped connection resumes from the last byte) and keeps server memory flat.

### Sizes are 64-bit everywhere

Package sizes are `u64` in the domain model, bound as `i64` in SQLite, and
emitted as JSON numbers — nothing truncates at the 4 GB `u32` limit. The test
suite explicitly stores and round-trips a 25 GB size to guard this.

## Measured

The design above is only worth stating if it holds in practice, so it was
measured end to end against a release build serving a real **5 GiB** package
(5,368,709,716 bytes — deliberately past the 4 GiB `u32` boundary, which also
makes it a ZIP64 archive). Server resident memory was sampled every 300 ms
throughout:

| Operation | Peak server RSS |
| --- | --- |
| Idle, before any transfer | 11.7 MB |
| 5 GiB push, raw body | 15.8 MB |
| 5 GiB push, `multipart/form-data` (what `dotnet nuget push` sends) | 14.1 MB |
| 5 GiB download | 15.9 MB |
| **Six concurrent 5 GiB downloads** (30 GiB in flight) | **14.9 MB** |

So a 5 GiB transfer costs single-digit megabytes above idle, and six of them at
once cost no more than one — memory tracks the number of *buffers*, not the
number of bytes.

Alongside the memory numbers, the same run confirmed the correctness properties
that matter to a client:

- the stored file is byte-identical to the source, including via the multipart
  path (which must strip its framing exactly);
- the reported size is 5,368,709,716 — no truncation at 4 GiB;
- the advertised SHA-512 matches a hash computed independently over the source
  file, so the digest a client verifies really describes the bytes it received;
- `Range` requests resolve correctly *past* the 4 GiB mark — a range at offset
  5,368,709,000 returned the exact tail bytes with a correct `Content-Range`,
  which is what makes a 25 GB download resumable.

Worth noting for anyone reproducing this: `curl --data-binary @file` reads the
whole body into memory and will run out on a file this size. Use `curl -T file`,
which streams. The server is the part that does not buffer.

## Operational guidance

- **Disk, not RAM, is the limit.** Provision storage for your largest package
  plus headroom for in-flight `.uploads`.
- **Keep `{storage}/.uploads` on the storage filesystem** (the default) so
  ingest stays a rename. If you bind-mount storage, mount the whole directory,
  not a subpath, so `.uploads` rides along.
- **No upload timeout is imposed by YANuget.** If you put a reverse proxy in
  front, raise its request timeout and body-size limits (e.g. nginx
  `client_max_body_size 0;` and generous `proxy_read_timeout`).
- **Reverse-proxy buffering:** disable request/response buffering for the
  package endpoints (nginx `proxy_request_buffering off;`,
  `proxy_buffering off;`) so the proxy doesn't reintroduce the very buffering
  YANuget avoids.
- **Concurrency:** because each transfer uses ~one buffer, many concurrent
  large transfers are limited by disk and network throughput, not memory.
