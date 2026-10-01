//! Parsing `application/x-www-form-urlencoded` bodies and lenient query
//! values, for the gallery's query strings and the admin area's forms.

/// Parse an optional query value, treating an empty or malformed one as absent.
pub(super) fn lenient<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value.and_then(|v| v.trim().parse().ok())
}

/// Read one field out of an `application/x-www-form-urlencoded` body.
pub(super) fn form_field(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (decode_form_value(k) == name).then(|| decode_form_value(v))
    })
}

/// Every value of a repeated form field (`v=1.0.0&v=2.0.0`), in order.
pub(super) fn form_fields(body: &str, name: &str) -> Vec<String> {
    body.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (decode_form_value(k) == name).then(|| decode_form_value(v))
        })
        .collect()
}

fn decode_form_value(raw: &str) -> String {
    let plus_decoded = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&plus_decoded)
        .decode_utf8_lossy()
        .into_owned()
}
