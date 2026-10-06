use crate::{
    new_capability_token, validate_bind_address, validate_http_request, Route, ViewerState,
    MAX_REQUEST_BODY_BYTES, MAX_REQUEST_HEAD_BYTES,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ACTIVE_CONNECTIONS: usize = 16;

pub struct RunningServer {
    url: String,
    join: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

impl RunningServer {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn wait(&self) -> Result<(), String> {
        let join = self
            .join
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| "Viewer server was already joined".to_string())?;
        join.join()
            .map_err(|_| "Viewer server thread panicked".to_string())?
    }
}

pub fn start_server(
    state: Arc<ViewerState>,
    authenticated_idle_timeout: Duration,
) -> Result<RunningServer, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("Failed to bind loopback viewer server: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Failed to inspect viewer bind address: {error}"))?;
    validate_bind_address(address)?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Failed to configure viewer listener: {error}"))?;
    let authority = format!("127.0.0.1:{}", address.port());
    let token = new_capability_token()?;
    let csp_nonce = new_capability_token()?;
    let url = format!("http://{authority}/{token}/");
    let page = include_str!("../frontend/index.html").replace("__CSP_NONCE__", &csp_nonce);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let join = std::thread::spawn({
        let authority = authority.clone();
        let token = token.clone();
        let state = Arc::clone(&state);
        move || loop {
            if state.is_shutdown_requested()
                || (state.browser_seen()
                    && state.authenticated_idle_for() >= authenticated_idle_timeout)
            {
                return Ok(());
            }
            match listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        let _ = write_response(
                            &mut stream,
                            403,
                            "Forbidden",
                            "text/plain; charset=utf-8",
                            b"Forbidden",
                            ResponsePolicy::Api,
                            &csp_nonce,
                        );
                        continue;
                    }
                    if active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_ACTIVE_CONNECTIONS {
                        active_connections.fetch_sub(1, Ordering::AcqRel);
                        let _ = write_response(
                            &mut stream,
                            503,
                            "Service Unavailable",
                            "text/plain; charset=utf-8",
                            b"Busy",
                            ResponsePolicy::Api,
                            &csp_nonce,
                        );
                        continue;
                    }
                    let authority = authority.clone();
                    let token = token.clone();
                    let page = page.clone();
                    let csp_nonce = csp_nonce.clone();
                    let state = Arc::clone(&state);
                    let active_connections = Arc::clone(&active_connections);
                    std::thread::spawn(move || {
                        let _guard = ConnectionGuard(active_connections);
                        if let Err(error) = handle_connection(
                            &mut stream,
                            &authority,
                            &token,
                            &page,
                            &csp_nonce,
                            &state,
                        ) {
                            eprintln!("Viewer request failed: {error}");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("Viewer listener failed: {error}")),
            }
        }
    });
    Ok(RunningServer {
        url,
        join: Mutex::new(Some(join)),
    })
}

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    authority: &str,
    token: &str,
    page: &str,
    csp_nonce: &str,
    state: &ViewerState,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("Failed to set viewer read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("Failed to set viewer write timeout: {error}"))?;
    let request = match read_bounded_request(stream) {
        Ok(request) => request,
        Err(_) => {
            return write_response(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                b"Bad Request",
                ResponsePolicy::Api,
                csp_nonce,
            )
        }
    };
    let request = match validate_http_request(&request, authority, token) {
        Ok(request) => request,
        Err(_) => {
            return write_response(
                stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"Not Found",
                ResponsePolicy::Api,
                csp_nonce,
            )
        }
    };
    state.touch_authenticated_request();
    match request.route {
        Route::Page => {
            state.mark_browser_page();
            write_response(
                stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
                ResponsePolicy::Page,
                csp_nonce,
            )
        }
        Route::Health => {
            let render = state.render_health();
            let body = serde_json::to_vec(&json!({
                "ready": true,
                "pid": std::process::id(),
                "bind": "127.0.0.1",
                "browser_connected": state.browser_ready(),
                "root_rendered": state.svg(0).is_ok(),
                "auto_refresh_watcher": state.watcher_active(),
                "source": "isolated_snapshot",
                "total_sheets": render.total_sheets,
                "rendered_sheets": render.rendered_sheets,
                "render_errors": render.render_errors,
                "render_warnings": render.render_warnings
            }))
            .map_err(|error| format!("Failed to serialize health response: {error}"))?;
            write_response(
                stream,
                200,
                "OK",
                "application/json; charset=utf-8",
                &body,
                ResponsePolicy::Api,
                csp_nonce,
            )
        }
        Route::State => {
            state.mark_browser_state();
            let body = serde_json::to_vec(&state.public_state())
                .map_err(|error| format!("Failed to serialize viewer state: {error}"))?;
            write_response(
                stream,
                200,
                "OK",
                "application/json; charset=utf-8",
                &body,
                ResponsePolicy::Api,
                csp_nonce,
            )
        }
        Route::Svg { sheet_index } => match state.svg(sheet_index) {
            Ok(svg) => {
                let result = write_response(
                    stream,
                    200,
                    "OK",
                    "image/svg+xml",
                    svg.as_bytes(),
                    ResponsePolicy::Svg,
                    csp_nonce,
                );
                if result.is_ok() {
                    state.mark_browser_svg();
                }
                result
            }
            Err(error) => write_json_error(stream, 409, "Conflict", &error, csp_nonce),
        },
        Route::Refresh { sheet_index } => match state.refresh_sheet(sheet_index) {
            Ok(()) => {
                let body = serde_json::to_vec(&state.public_state())
                    .map_err(|error| format!("Failed to serialize viewer state: {error}"))?;
                write_response(
                    stream,
                    200,
                    "OK",
                    "application/json; charset=utf-8",
                    &body,
                    ResponsePolicy::Api,
                    csp_nonce,
                )
            }
            Err(error) => write_json_error(stream, 409, "Conflict", &error, csp_nonce),
        },
        Route::OpenInKicad { sheet_index } => match state.open_in_kicad(sheet_index) {
            Ok(()) => write_response(
                stream,
                204,
                "No Content",
                "text/plain; charset=utf-8",
                b"",
                ResponsePolicy::Api,
                csp_nonce,
            ),
            Err(error) => write_json_error(stream, 409, "Conflict", &error, csp_nonce),
        },
        Route::BrowserReady => {
            state.mark_browser_ui_ready();
            write_response(
                stream,
                204,
                "No Content",
                "text/plain; charset=utf-8",
                b"",
                ResponsePolicy::Api,
                csp_nonce,
            )
        }
    }
}

fn read_bounded_request(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let maximum = MAX_REQUEST_HEAD_BYTES + MAX_REQUEST_BODY_BYTES + 4;
    let mut request = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .map_err(|error| format!("Failed to read HTTP request: {error}"))?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > maximum {
            return Err("HTTP request exceeds the configured size limit".to_string());
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    if request.is_empty() {
        Err("Empty HTTP request".to_string())
    } else {
        Ok(request)
    }
}

fn write_json_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    message: &str,
    csp_nonce: &str,
) -> Result<(), String> {
    let body = serde_json::to_vec(&json!({ "error": message }))
        .map_err(|error| format!("Failed to serialize error response: {error}"))?;
    write_response(
        stream,
        status,
        reason,
        "application/json; charset=utf-8",
        &body,
        ResponsePolicy::Api,
        csp_nonce,
    )
}

#[derive(Clone, Copy)]
enum ResponsePolicy {
    Page,
    Svg,
    Api,
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    policy: ResponsePolicy,
    csp_nonce: &str,
) -> Result<(), String> {
    let csp = match policy {
        ResponsePolicy::Page => format!(
            "default-src 'none'; script-src 'nonce-{csp_nonce}'; style-src 'nonce-{csp_nonce}'; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
        ),
        ResponsePolicy::Svg => {
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'none'; sandbox".to_string()
        }
        ResponsePolicy::Api => "default-src 'none'; frame-ancestors 'none'".to_string(),
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store, max-age=0\r\nPragma: no-cache\r\nContent-Security-Policy: {csp}\r\nCross-Origin-Resource-Policy: same-origin\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|error| format!("Failed to write HTTP response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_schematic_editor::Schematic;
    use std::net::Shutdown;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn blank_schematic(directory: &Path) -> PathBuf {
        let path = directory.join("root.kicad_sch");
        let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
        std::fs::write(&path, template).unwrap();
        Schematic::load(&path).unwrap().overwrite().unwrap();
        path
    }

    #[cfg(unix)]
    fn fake_cli(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("fake-kicad-cli");
        std::fs::write(
            &path,
            "#!/bin/sh\nset -eu\nout=''; last=''; while [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; out=$1; fi; last=$1; shift; done\nstem=$(basename \"$last\" .kicad_sch)\nprintf '<svg viewBox=\"0 0 100 100\"></svg>' > \"$out/$stem.svg\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn split_url(url: &str) -> (String, String) {
        let rest = url.strip_prefix("http://").unwrap();
        let (authority, path) = rest.split_once('/').unwrap();
        (authority.to_string(), format!("/{path}"))
    }

    fn request(url: &str, path_override: Option<&str>) -> String {
        let (authority, path) = split_url(url);
        let path = path_override.unwrap_or(&path);
        let mut stream = TcpStream::connect(&authority).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn post(url: &str, path: &str) -> String {
        let (authority, _) = split_url(url);
        let mut stream = TcpStream::connect(&authority).unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_servers_have_isolated_ports_and_capabilities() {
        let first_files = TempDir::new().unwrap();
        let second_files = TempDir::new().unwrap();
        let first = ViewerState::new(
            &blank_schematic(first_files.path()),
            fake_cli(first_files.path()).to_string_lossy().to_string(),
        )
        .unwrap();
        let second = ViewerState::new(
            &blank_schematic(second_files.path()),
            fake_cli(second_files.path()).to_string_lossy().to_string(),
        )
        .unwrap();
        let first_server = start_server(Arc::clone(&first), Duration::from_secs(5)).unwrap();
        let second_server = start_server(Arc::clone(&second), Duration::from_secs(5)).unwrap();
        assert_ne!(first_server.url(), second_server.url());

        let (_, first_path) = split_url(first_server.url());
        let crossed = request(second_server.url(), Some(&first_path));
        assert!(crossed.starts_with("HTTP/1.1 404"), "{crossed}");
        assert!(!second.browser_seen());

        let first_page = request(first_server.url(), None);
        let second_page = request(second_server.url(), None);
        assert!(first_page.starts_with("HTTP/1.1 200"), "{first_page}");
        assert!(second_page.starts_with("HTTP/1.1 200"), "{second_page}");
        assert!(first_page.contains("Content-Security-Policy:"));
        assert!(!first_page.contains("__CSP_NONCE__"));
        assert!(first.browser_seen());
        assert!(second.browser_seen());
        assert!(!first.browser_ready());
        assert!(!second.browser_ready());

        let (_, first_base) = split_url(first_server.url());
        let (_, second_base) = split_url(second_server.url());
        assert!(
            request(first_server.url(), Some(&format!("{first_base}api/state")))
                .starts_with("HTTP/1.1 200")
        );
        assert!(request(
            first_server.url(),
            Some(&format!("{first_base}api/svg?id=0"))
        )
        .starts_with("HTTP/1.1 200"));
        assert!(request(
            second_server.url(),
            Some(&format!("{second_base}api/state"))
        )
        .starts_with("HTTP/1.1 200"));
        assert!(request(
            second_server.url(),
            Some(&format!("{second_base}api/svg?id=0"))
        )
        .starts_with("HTTP/1.1 200"));
        assert!(!first.browser_ready());
        assert!(!second.browser_ready());
        assert!(post(
            first_server.url(),
            &format!("{first_base}api/browser-ready")
        )
        .starts_with("HTTP/1.1 204"));
        assert!(post(
            second_server.url(),
            &format!("{second_base}api/browser-ready")
        )
        .starts_with("HTTP/1.1 204"));
        assert!(first.browser_ready());
        assert!(second.browser_ready());
        let refreshed = post(first_server.url(), &format!("{first_base}api/refresh?id=0"));
        assert!(refreshed.starts_with("HTTP/1.1 200"), "{refreshed}");

        first.request_shutdown();
        second.request_shutdown();
        first_server.wait().unwrap();
        second_server.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_idle_timeout_stops_server_after_browser_disconnect() {
        let files = TempDir::new().unwrap();
        let state = ViewerState::new(
            &blank_schematic(files.path()),
            fake_cli(files.path()).to_string_lossy().to_string(),
        )
        .unwrap();
        let server = start_server(Arc::clone(&state), Duration::from_millis(50)).unwrap();
        assert!(request(server.url(), None).starts_with("HTTP/1.1 200"));
        server.wait().unwrap();
    }
}
