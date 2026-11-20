//! AWS Signature Version 4 (SigV4) request signing for S3 / RustFS.

use chrono::{DateTime, Utc};
use ring::{digest, hmac};

/// Payload hash constant used for streaming uploads over HTTPS/S3.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Computes lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let d = digest::digest(&digest::SHA256, data);
    hex::encode(d.as_ref())
}

/// Percent-encodes a string according to AWS SigV4 URI encoding rules.
pub fn aws_uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && byte == b'/');
        if is_unreserved {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Parameters required to sign an S3 HTTP request with AWS SigV4.
pub struct SigV4Params<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_hash: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub now: DateTime<Utc>,
}

/// Signed headers to attach to the outgoing `reqwest::RequestBuilder`.
#[derive(Debug, Clone)]
pub struct SigV4Headers {
    pub x_amz_date: String,
    pub x_amz_content_sha256: String,
    pub authorization: String,
}

/// Signs an S3 request and returns the `x-amz-date`, `x-amz-content-sha256`,
/// and `Authorization` header values.
pub fn sign_request(params: &SigV4Params<'_>) -> SigV4Headers {
    let amz_date = params.now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = params.now.format("%Y%m%d").to_string();

    let canonical_uri = if params.path.is_empty() {
        "/".to_string()
    } else {
        aws_uri_encode(params.path, false)
    };

    let mut encoded_query: Vec<(String, String)> = params
        .query
        .iter()
        .map(|(k, v)| (aws_uri_encode(k, true), aws_uri_encode(v, true)))
        .collect();
    encoded_query.sort();
    let canonical_query = encoded_query
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut header_map: Vec<(String, String)> = vec![
        ("host".to_string(), params.host.trim().to_string()),
        (
            "x-amz-content-sha256".to_string(),
            params.payload_hash.to_string(),
        ),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    for (k, v) in params.headers {
        let key_lower = k.trim().to_ascii_lowercase();
        if !matches!(
            key_lower.as_str(),
            "host" | "x-amz-content-sha256" | "x-amz-date"
        ) {
            header_map.push((key_lower, v.trim().to_string()));
        }
    }
    header_map.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical_headers = String::new();
    let mut signed_header_names = Vec::with_capacity(header_map.len());
    for (k, v) in &header_map {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(v);
        canonical_headers.push('\n');
        signed_header_names.push(k.as_str());
    }
    let signed_headers = signed_header_names.join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        params.method,
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        params.payload_hash
    );

    let scope = format!("{date_stamp}/{}/s3/aws4_request", params.region);
    let canonical_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_hash}");

    let k_secret = format!("AWS4{}", params.secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, params.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        params.access_key
    );

    SigV4Headers {
        x_amz_date: amz_date,
        x_amz_content_sha256: params.payload_hash.to_string(),
        authorization,
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&signing_key, data).as_ref().to_vec()
}
