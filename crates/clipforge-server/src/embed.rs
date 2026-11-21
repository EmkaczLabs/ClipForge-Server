//! Server-rendered Open Graph, Twitter/X Player, and oEmbed helpers.

use std::fmt::Write as _;

use serde::Serialize;

/// Metadata injected into `/c/{share_id}` and `/s/{share_id}` HTML responses.
#[derive(Debug, Clone)]
pub struct ShareEmbedMeta {
    pub title: String,
    pub description: String,
    pub share_url: String,
    pub thumbnail_url: String,
    pub video_url: String,
    pub oembed_url: String,
    pub width: u32,
    pub height: u32,
}

/// oEmbed 1.0 JSON response for public video shares.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OEmbedResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: &'static str,
    pub title: String,
    pub provider_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_url: Option<String>,
    pub thumbnail_url: String,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub width: u32,
    pub height: u32,
    pub html: String,
}

/// Strictly escapes HTML special characters for safe interpolation into text and attributes.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Percent-encodes a string for use in URL query parameters (RFC 3986 unreserved bytes preserved).
pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Injects server-rendered Open Graph, Twitter/X Player, and oEmbed tags into `html`.
pub fn inject_share_meta_tags(html: &str, meta: &ShareEmbedMeta) -> String {
    let title = escape_html(&meta.title);
    let description = escape_html(&meta.description);
    let share_url = escape_html(&meta.share_url);
    let thumbnail_url = escape_html(&meta.thumbnail_url);
    let video_url = escape_html(&meta.video_url);
    let oembed_url = escape_html(&meta.oembed_url);
    let width = meta.width;
    let height = meta.height;

    let tags = format!(
        concat!(
            "  <meta property=\"og:type\" content=\"video.other\" />\n",
            "  <meta property=\"og:site_name\" content=\"ClipForge\" />\n",
            "  <meta property=\"og:title\" content=\"{title}\" />\n",
            "  <meta property=\"og:description\" content=\"{description}\" />\n",
            "  <meta property=\"og:url\" content=\"{share_url}\" />\n",
            "  <meta property=\"og:image\" content=\"{thumbnail_url}\" />\n",
            "  <meta property=\"og:video\" content=\"{video_url}\" />\n",
            "  <meta property=\"og:video:secure_url\" content=\"{video_url}\" />\n",
            "  <meta property=\"og:video:type\" content=\"video/mp4\" />\n",
            "  <meta property=\"og:video:width\" content=\"{width}\" />\n",
            "  <meta property=\"og:video:height\" content=\"{height}\" />\n",
            "  <meta name=\"twitter:card\" content=\"player\" />\n",
            "  <meta name=\"twitter:title\" content=\"{title}\" />\n",
            "  <meta name=\"twitter:description\" content=\"{description}\" />\n",
            "  <meta name=\"twitter:image\" content=\"{thumbnail_url}\" />\n",
            "  <meta name=\"twitter:player\" content=\"{share_url}\" />\n",
            "  <meta name=\"twitter:player:width\" content=\"{width}\" />\n",
            "  <meta name=\"twitter:player:height\" content=\"{height}\" />\n",
            "  <meta name=\"twitter:player:stream\" content=\"{video_url}\" />\n",
            "  <meta name=\"twitter:player:stream:content_type\" content=\"video/mp4\" />\n",
            "  <link rel=\"alternate\" type=\"application/json+oembed\" href=\"{oembed_url}\" title=\"{title}\" />\n",
        ),
        title = title,
        description = description,
        share_url = share_url,
        thumbnail_url = thumbnail_url,
        video_url = video_url,
        oembed_url = oembed_url,
        width = width,
        height = height,
    );

    let with_title = html.replacen(
        "<title>ClipForge — Shared Clip</title>",
        &format!("<title>{title} — ClipForge</title>"),
        1,
    );

    if let Some(pos) = with_title.find("</head>") {
        let mut out = String::with_capacity(with_title.len() + tags.len());
        out.push_str(&with_title[..pos]);
        out.push_str(&tags);
        out.push_str(&with_title[pos..]);
        out
    } else {
        format!("{tags}{with_title}")
    }
}

/// Extracts the `{share_id}` token from a share URL such as `https://host/c/{share_id}` or `/s/{share_id}`.
pub fn extract_share_id_from_url(raw_url: &str) -> Option<String> {
    let without_frag = raw_url.split('#').next().unwrap_or(raw_url);
    let path_and_host = without_frag.split('?').next().unwrap_or(without_frag);
    let trimmed = path_and_host.trim().trim_end_matches('/');

    for marker in ["/c/", "/s/"] {
        if let Some((_, rest)) = trimmed.rsplit_once(marker) {
            if !rest.is_empty() && !rest.contains('/') {
                return Some(rest.to_string());
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escaping_neutralizes_xss() {
        let escaped = escape_html("<script>alert(\"x\" & 'y')</script>");
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#x27;y&#x27;)&lt;/script&gt;"
        );
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert!(!escaped.contains('"'));
    }

    #[test]
    fn share_id_extraction_handles_full_and_relative_urls() {
        assert_eq!(
            extract_share_id_from_url("https://clips.example.com/c/abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_share_id_from_url("http://localhost:3000/s/tok_99/?autoplay=1#t=2"),
            Some("tok_99".to_string())
        );
        assert_eq!(
            extract_share_id_from_url("/c/my-token"),
            Some("my-token".to_string())
        );
        assert_eq!(
            extract_share_id_from_url("https://example.com/other/123"),
            None
        );
        assert_eq!(extract_share_id_from_url("https://example.com/c/"), None);
    }
}
