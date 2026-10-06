use std::collections::HashMap;
use std::net::SocketAddr;

mod hierarchy;
mod render;
mod runtime;
mod server;

pub use hierarchy::{discover_sheet_tree, SheetEntry};
pub use runtime::{run_from_env, ViewerState};
pub use server::{start_server, RunningServer};

pub const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;
pub const MAX_REQUEST_BODY_BYTES: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Page,
    Health,
    State,
    Svg { sheet_index: usize },
    Refresh { sheet_index: usize },
    OpenInKicad { sheet_index: usize },
    BrowserReady,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ValidatedRequest {
    pub route: Route,
}

pub fn new_capability_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Failed to obtain operating-system randomness: {error}"))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(token)
}

pub fn validate_bind_address(address: SocketAddr) -> Result<(), String> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "Refusing non-loopback viewer bind address: {address}"
        ))
    }
}

pub fn validate_http_request(
    request: &[u8],
    authority: &str,
    token: &str,
) -> Result<ValidatedRequest, String> {
    if request.len() > MAX_REQUEST_HEAD_BYTES + MAX_REQUEST_BODY_BYTES + 4 {
        return Err("HTTP request exceeds the configured size limit".to_string());
    }
    let head_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "Incomplete HTTP request head".to_string())?;
    if head_end + 4 > MAX_REQUEST_HEAD_BYTES {
        return Err("HTTP request head exceeds the configured size limit".to_string());
    }
    let head = std::str::from_utf8(&request[..head_end])
        .map_err(|_| "HTTP request head is not UTF-8".to_string())?;
    let body = &request[head_end + 4..];
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "Missing HTTP request line".to_string())?;
    let mut request_parts = request_line.split(' ');
    let method = request_parts
        .next()
        .ok_or_else(|| "Missing HTTP method".to_string())?;
    let target = request_parts
        .next()
        .ok_or_else(|| "Missing HTTP target".to_string())?;
    let version = request_parts
        .next()
        .ok_or_else(|| "Missing HTTP version".to_string())?;
    if request_parts.next().is_some() || version != "HTTP/1.1" {
        return Err("Only a canonical HTTP/1.1 request line is accepted".to_string());
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            return Err("Malformed or folded HTTP header".to_string());
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "Malformed HTTP header".to_string())?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("Invalid HTTP header name".to_string());
        }
        let name = name.to_ascii_lowercase();
        if headers.insert(name, value.trim()).is_some() {
            return Err("Duplicate HTTP headers are not accepted".to_string());
        }
        if headers.len() > 32 {
            return Err("Too many HTTP headers".to_string());
        }
    }

    if headers.get("host").copied() != Some(authority) {
        return Err("Host does not match the loopback viewer authority".to_string());
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Transfer-Encoding is not accepted".to_string());
    }
    let content_length = match headers.get("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| "Invalid Content-Length".to_string())?,
        None => 0,
    };
    if content_length > MAX_REQUEST_BODY_BYTES || body.len() != content_length {
        return Err("HTTP request body length is invalid".to_string());
    }

    if target.contains('%')
        || target.contains('#')
        || target.contains('\\')
        || target.contains("..")
        || target.contains("//")
    {
        return Err("Non-canonical HTTP target".to_string());
    }
    let prefix = format!("/{token}/");
    if target.len() < prefix.len()
        || !constant_time_eq(&target.as_bytes()[..prefix.len()], prefix.as_bytes())
    {
        return Err("Invalid viewer capability".to_string());
    }
    let relative = &target[prefix.len()..];
    let expected_origin = format!("http://{authority}");
    if let Some(origin) = headers.get("origin") {
        if *origin != expected_origin {
            return Err("Cross-origin viewer request refused".to_string());
        }
    }

    let route = match (method, relative) {
        ("GET", "") => Route::Page,
        ("GET", "api/health") => Route::Health,
        ("GET", "api/state") => Route::State,
        ("GET", target) if target.starts_with("api/svg?id=") => Route::Svg {
            sheet_index: parse_sheet_index(target, "api/svg?id=")?,
        },
        ("POST", target) if target.starts_with("api/refresh?id=") => {
            require_mutation_boundary(&headers, &expected_origin, content_length)?;
            Route::Refresh {
                sheet_index: parse_sheet_index(target, "api/refresh?id=")?,
            }
        }
        ("POST", target) if target.starts_with("api/open-kicad?id=") => {
            require_mutation_boundary(&headers, &expected_origin, content_length)?;
            Route::OpenInKicad {
                sheet_index: parse_sheet_index(target, "api/open-kicad?id=")?,
            }
        }
        ("POST", "api/browser-ready") => {
            require_mutation_boundary(&headers, &expected_origin, content_length)?;
            Route::BrowserReady
        }
        _ => return Err("Unknown viewer route or method".to_string()),
    };

    if method == "GET" && content_length != 0 {
        return Err("GET requests must not carry a body".to_string());
    }
    Ok(ValidatedRequest { route })
}

fn parse_sheet_index(target: &str, prefix: &str) -> Result<usize, String> {
    let value = target
        .strip_prefix(prefix)
        .ok_or_else(|| "Invalid viewer route".to_string())?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Invalid sheet index".to_string());
    }
    value
        .parse::<usize>()
        .map_err(|_| "Sheet index is out of range".to_string())
}

fn require_mutation_boundary(
    headers: &HashMap<String, &str>,
    expected_origin: &str,
    content_length: usize,
) -> Result<(), String> {
    if headers.get("origin").copied() != Some(expected_origin) {
        return Err("Mutation requires an exact same-origin Origin header".to_string());
    }
    if content_length != 0 {
        return Err("Mutation request body must be empty".to_string());
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (&left, &right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, target: &str, authority: &str, extra: &str) -> Vec<u8> {
        format!("{method} {target} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").into_bytes()
    }

    #[test]
    fn capability_tokens_are_unpredictable_256_bit_hex_values() {
        let first = new_capability_token().unwrap();
        let second = new_capability_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn server_refuses_every_non_loopback_bind() {
        assert!(validate_bind_address("127.0.0.1:1".parse().unwrap()).is_ok());
        assert!(validate_bind_address("[::1]:1".parse().unwrap()).is_ok());
        assert!(validate_bind_address("0.0.0.0:1".parse().unwrap()).is_err());
        assert!(validate_bind_address("192.0.2.10:1".parse().unwrap()).is_err());
    }

    #[test]
    fn page_requires_exact_capability_and_host() {
        let token = "a".repeat(64);
        let authority = "127.0.0.1:43210";
        let good = req("GET", &format!("/{token}/"), authority, "");
        assert_eq!(
            validate_http_request(&good, authority, &token)
                .unwrap()
                .route,
            Route::Page
        );

        let wrong_token = req("GET", &format!("/{}/", "b".repeat(64)), authority, "");
        assert!(validate_http_request(&wrong_token, authority, &token).is_err());

        let wrong_host = req("GET", &format!("/{token}/"), "evil.example", "");
        assert!(validate_http_request(&wrong_host, authority, &token).is_err());
    }

    #[test]
    fn request_rejects_traversal_fragments_duplicates_and_oversized_bodies() {
        let token = "a".repeat(64);
        let authority = "127.0.0.1:43210";
        for target in [
            format!("/{token}/../api/state"),
            format!("/{token}/%2e%2e/api/state"),
            format!("/{token}//api/state"),
            format!("/{token}/api/state#fragment"),
        ] {
            let raw = req("GET", &target, authority, "");
            assert!(
                validate_http_request(&raw, authority, &token).is_err(),
                "{target}"
            );
        }

        let duplicate_host =
            format!("GET /{token}/ HTTP/1.1\r\nHost: {authority}\r\nHost: {authority}\r\n\r\n");
        assert!(validate_http_request(duplicate_host.as_bytes(), authority, &token).is_err());

        let body = req(
            "POST",
            &format!("/{token}/api/refresh?id=0"),
            authority,
            &format!(
                "Origin: http://{authority}\r\nContent-Length: {}\r\n",
                MAX_REQUEST_BODY_BYTES + 1
            ),
        );
        assert!(validate_http_request(&body, authority, &token).is_err());
    }

    #[test]
    fn mutations_require_same_origin_post_and_zero_body() {
        let token = "a".repeat(64);
        let authority = "127.0.0.1:43210";
        let target = format!("/{token}/api/refresh?id=7");
        let good = req(
            "POST",
            &target,
            authority,
            &format!("Origin: http://{authority}\r\nContent-Length: 0\r\n"),
        );
        assert_eq!(
            validate_http_request(&good, authority, &token)
                .unwrap()
                .route,
            Route::Refresh { sheet_index: 7 }
        );

        let get = req("GET", &target, authority, "");
        assert!(validate_http_request(&get, authority, &token).is_err());
        let absent_origin = req("POST", &target, authority, "Content-Length: 0\r\n");
        assert!(validate_http_request(&absent_origin, authority, &token).is_err());
        let cross_origin = req(
            "POST",
            &target,
            authority,
            "Origin: https://evil.example\r\nContent-Length: 0\r\n",
        );
        assert!(validate_http_request(&cross_origin, authority, &token).is_err());
        let body = format!(
            "POST {target} HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\nContent-Length: 1\r\n\r\nx"
        );
        assert!(validate_http_request(body.as_bytes(), authority, &token).is_err());
    }
}
