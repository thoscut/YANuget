//! The HTTP client every test talks to its servers with.
//!
//! Its own file so that the test binaries that keep a harness of their own
//! (`http_security.rs`, `packages.rs`, `mirror.rs`, `inbox_files.rs`) take
//! it with `#[path]` without the rest of `common`.

// Not every test binary needs both.
#![allow(dead_code)]

/// A client for the servers these tests start on loopback.
///
/// reqwest takes `HTTP_PROXY` and `HTTPS_PROXY` from the environment and does
/// not leave loopback out by itself, so on a machine behind a proxy whose
/// `NO_PROXY` does not name `127.0.0.1`, every request would go to the proxy
/// instead of the server under test: red for no fault of the code, or
/// answered by something that is not the test server. Every test client is
/// built from this, never from `reqwest::Client::new()` or `reqwest::get`.
pub fn client() -> reqwest::Client {
    client_builder().build().unwrap()
}

/// [`client`]'s builder, for a test that needs more settings on it.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_proxy()
}
