//! Minimal HTTP/1.1 request-line parsing and response-header building.
//!
//! Pure, allocation-free helpers (heapless + core only) used by the hand-rolled server
//! ([`crate::server`], feature `server`). They are compiled **unconditionally** so the host test
//! suite validates them with plain `cargo test` — the embassy-net I/O that consumes them lives in
//! [`crate::server`] behind the `server` feature.

use core::fmt::Write as _;

use heapless::String;

/// Capacity of the response status + header block string.
///
/// The longest block we emit is roughly:
/// `HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n`
/// (~100 bytes). 160 leaves comfortable headroom.
pub const HEADER_CAP: usize = 160;

/// Build the HTTP/1.1 status line + headers for a fixed-length body:
///
/// `HTTP/1.1 <status>\r\nContent-Type: <content_type>\r\nContent-Length: <len>\r\nConnection: close\r\n\r\n`
///
/// `status` is the code + reason phrase, e.g. `"200 OK"` or `"404 Not Found"`. The body is written
/// separately by the caller. `Connection: close` matches the server's one-response-per-connection
/// model (the former picoserve `KeepAlive::Close`).
pub fn response_header(
    status: &str,
    content_type: &str,
    content_length: usize,
) -> String<HEADER_CAP> {
    let mut out: String<HEADER_CAP> = String::new();
    // Each `push_str`/`write!` is fallible only if the buffer overflows; `HEADER_CAP` is sized so
    // it never does for our fixed set of statuses/content-types, so ignoring the results is safe.
    let _ = out.push_str("HTTP/1.1 ");
    let _ = out.push_str(status);
    let _ = out.push_str("\r\nContent-Type: ");
    let _ = out.push_str(content_type);
    let _ = out.push_str("\r\nContent-Length: ");
    let _ = write!(out, "{content_length}");
    let _ = out.push_str("\r\nConnection: close\r\n\r\n");
    out
}

/// Parse the request target (path) from a raw HTTP request.
///
/// Only `GET` is served. Returns the path with any `?query` stripped, or `None` if the request is
/// not a well-formed `GET` request line. Only the first line (the request line) is inspected, so a
/// partial read that captured at least the request line is sufficient.
pub fn parse_request_target(request: &str) -> Option<&str> {
    // The request line is the text up to the first CRLF (or the whole slice if none seen yet).
    let line = match request.split_once("\r\n") {
        Some((first, _)) => first,
        None => request,
    };

    let mut parts = line.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    if target.is_empty() {
        return None;
    }

    // Strip an optional query string.
    Some(match target.split_once('?') {
        Some((path, _)) => path,
        None => target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_header_ok_json_exact_bytes() {
        let header = response_header("200 OK", "application/json", 42);
        assert_eq!(
            header.as_str(),
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 42\r\n\
             Connection: close\r\n\r\n"
        );
    }

    #[test]
    fn response_header_html_and_404_forms() {
        let html = response_header("200 OK", "text/html", 1024);
        assert_eq!(
            html.as_str(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n"
        );
        let not_found = response_header("404 Not Found", "text/plain", 9);
        assert_eq!(
            not_found.as_str(),
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn parse_target_root() {
        assert_eq!(
            parse_request_target("GET / HTTP/1.1\r\nHost: 192.168.0.5:8080\r\n\r\n"),
            Some("/")
        );
    }

    #[test]
    fn parse_target_strips_query() {
        assert_eq!(
            parse_request_target("GET /events?client=3 HTTP/1.1\r\n\r\n"),
            Some("/events")
        );
        assert_eq!(
            parse_request_target("GET /pinmodes HTTP/1.1\r\n\r\n"),
            Some("/pinmodes")
        );
    }

    #[test]
    fn parse_target_accepts_request_line_without_crlf_yet() {
        // A partial read may capture only the request line before the header CRLF arrives.
        assert_eq!(
            parse_request_target("GET /release HTTP/1.1"),
            Some("/release")
        );
    }

    #[test]
    fn parse_target_rejects_non_get() {
        assert_eq!(parse_request_target("POST /release HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_request_target("HEAD / HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn parse_target_rejects_malformed() {
        assert_eq!(parse_request_target(""), None);
        assert_eq!(parse_request_target("GARBAGE"), None);
        assert_eq!(parse_request_target("GET"), None);
        assert_eq!(parse_request_target("GET \r\n"), None);
    }
}
