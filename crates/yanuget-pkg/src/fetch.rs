//! Small helpers both the server's mirror and the internalizer use when they
//! fetch from an upstream feed: how a URL appears in a log line, a base URL
//! that relative paths extend, and reading a body that may be gzipped under a
//! size cap.

use std::io::Read;

use url::Url;

/// A URL as it may appear in a log line or an error: without userinfo or a
/// fragment, and with any query replaced by `redacted`, since both are where
/// feeds put credentials (`https://user:token@host/…`, a signed `?sig=…`
/// download link).
pub fn redact(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if url.query().is_some() {
        url.set_query(Some("redacted"));
    }
    url.set_fragment(None);
    url.to_string()
}

/// `url` with a trailing slash on its path, so joining a relative path
/// extends it rather than replacing its last segment.
pub fn with_slash(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

/// Why [`decode_capped`] refused a body.
#[derive(Debug)]
pub enum DecodeError {
    /// Larger than the cap, once decoded.
    TooLarge,
    /// Not valid gzip.
    Gzip(std::io::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooLarge => f.write_str("too large"),
            DecodeError::Gzip(e) => write!(f, "not valid gzip: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A response body, gunzipped when it was sent `Content-Encoding: gzip`,
/// and held to `cap` bytes after decoding as before. nuget.org serves its
/// registration hives gzipped whether or not that was asked for; a small
/// body can decode to a very large one, so the cap applies to the result.
///
/// Decoding takes a while for a large document: call it off the async
/// runtime (`spawn_blocking`).
pub fn decode_capped(body: Vec<u8>, gzip: bool, cap: u64) -> Result<Vec<u8>, DecodeError> {
    if body.len() as u64 > cap {
        return Err(DecodeError::TooLarge);
    }
    if !gzip {
        return Ok(body);
    }
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(body.as_slice())
        .take(cap.saturating_add(1))
        .read_to_end(&mut decoded)
        .map_err(DecodeError::Gzip)?;
    if decoded.len() as u64 > cap {
        return Err(DecodeError::TooLarge);
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn redaction_drops_what_carries_credentials() {
        assert_eq!(
            redact(&url(
                "https://user:secret@feed.example/v3/x.nupkg?sig=abc#frag"
            )),
            "https://feed.example/v3/x.nupkg?redacted"
        );
        assert_eq!(
            redact(&url("https://feed.example/v3/index.json")),
            "https://feed.example/v3/index.json"
        );
    }

    #[test]
    fn trailing_slash_is_normalized() {
        assert_eq!(
            with_slash(url("https://x/flat")).as_str(),
            "https://x/flat/"
        );
        assert_eq!(
            with_slash(url("https://x/flat/")).as_str(),
            "https://x/flat/"
        );
    }

    #[test]
    fn gzip_is_decoded_under_the_cap() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&[b'a'; 100]).unwrap();
        let packed = gz.finish().unwrap();
        assert_eq!(decode_capped(packed.clone(), true, 100).unwrap().len(), 100);
        assert!(matches!(
            decode_capped(packed, true, 99),
            Err(DecodeError::TooLarge)
        ));
        assert!(matches!(
            decode_capped(b"plain".to_vec(), true, 100),
            Err(DecodeError::Gzip(_))
        ));
        assert_eq!(
            decode_capped(b"plain".to_vec(), false, 5).unwrap(),
            b"plain"
        );
        assert!(matches!(
            decode_capped(b"plain".to_vec(), false, 4),
            Err(DecodeError::TooLarge)
        ));
    }
}
