use crate::models::connection::ProxyType;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::collections::HashMap;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct ProxyTunnelManager {
    tunnels: tokio::sync::Mutex<HashMap<String, (JoinHandle<()>, u16)>>,
}

impl ProxyTunnelManager {
    pub fn new() -> Self {
        Self { tunnels: tokio::sync::Mutex::new(HashMap::new()) }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_tunnel(
        &self,
        connection_id: &str,
        proxy_type: ProxyType,
        proxy_host: &str,
        proxy_port: u16,
        proxy_username: &str,
        proxy_password: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<u16, String> {
        if let Some(local_port) = self.local_port(connection_id).await {
            return Ok(local_port);
        }

        let local_port = portpicker::pick_unused_port().ok_or("No available port")?;
        let listener = TcpListener::bind(("127.0.0.1", local_port))
            .await
            .map_err(|e| format!("Failed to bind proxy tunnel local port: {e}"))?;

        let proxy = ProxyEndpoint {
            proxy_type,
            host: proxy_host.to_string(),
            port: proxy_port,
            username: proxy_username.to_string(),
            password: proxy_password.to_string(),
        };
        let remote = RemoteEndpoint { host: remote_host.to_string(), port: remote_port };
        let handle = tokio::spawn(proxy_forward_loop(listener, proxy, remote));

        let mut tunnels = self.tunnels.lock().await;
        if let Some((_, existing_port)) = tunnels.get(connection_id) {
            handle.abort();
            return Ok(*existing_port);
        }

        tunnels.insert(connection_id.to_string(), (handle, local_port));
        Ok(local_port)
    }

    pub async fn local_port(&self, connection_id: &str) -> Option<u16> {
        self.tunnels.lock().await.get(connection_id).map(|(_, port)| *port)
    }

    pub async fn stop_tunnel(&self, connection_id: &str) {
        if let Some((handle, _)) = self.tunnels.lock().await.remove(connection_id) {
            handle.abort();
        }
    }

    pub async fn stop_tunnels_with_prefix(&self, connection_id_prefix: &str) {
        let mut tunnels = self.tunnels.lock().await;
        let keys: Vec<String> = tunnels.keys().filter(|key| key.starts_with(connection_id_prefix)).cloned().collect();
        for key in keys {
            if let Some((handle, _)) = tunnels.remove(&key) {
                handle.abort();
            }
        }
    }
}

#[derive(Clone)]
struct ProxyEndpoint {
    proxy_type: ProxyType,
    host: String,
    port: u16,
    username: String,
    password: String,
}

#[derive(Clone)]
struct RemoteEndpoint {
    host: String,
    port: u16,
}

async fn proxy_forward_loop(listener: TcpListener, proxy: ProxyEndpoint, remote: RemoteEndpoint) {
    loop {
        let (mut inbound, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let proxy = proxy.clone();
        let remote = remote.clone();
        tokio::spawn(async move {
            let Ok(mut outbound) = connect_via_proxy(&proxy, &remote).await else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        });
    }
}

async fn connect_via_proxy(proxy: &ProxyEndpoint, remote: &RemoteEndpoint) -> Result<TcpStream, String> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((proxy.host.as_str(), proxy.port)))
        .await
        .map_err(|_| "Proxy connection timed out".to_string())?
        .map_err(|e| format!("Failed to connect proxy: {e}"))?;

    match proxy.proxy_type {
        ProxyType::Http => http_connect(stream, proxy, remote).await,
        ProxyType::Socks5 => socks5_connect(stream, proxy, remote).await,
    }
}

async fn http_connect(
    mut stream: TcpStream,
    proxy: &ProxyEndpoint,
    remote: &RemoteEndpoint,
) -> Result<TcpStream, String> {
    let target = format!("{}:{}", remote.host, remote.port);
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if !proxy.username.is_empty() || !proxy.password.is_empty() {
        let token = BASE64.encode(format!("{}:{}", proxy.username, proxy.password));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.map_err(|e| format!("Failed to send CONNECT request: {e}"))?;

    let mut response = Vec::new();
    let mut buf = [0_u8; 1];
    while !response.ends_with(b"\r\n\r\n") && response.len() < 8192 {
        let n = stream.read(&mut buf).await.map_err(|e| format!("Failed to read CONNECT response: {e}"))?;
        if n == 0 {
            break;
        }
        response.push(buf[0]);
    }
    let text = String::from_utf8_lossy(&response);
    if text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200") {
        Ok(stream)
    } else {
        let status = text.lines().next().unwrap_or("invalid proxy response");
        Err(format!("HTTP proxy CONNECT failed: {status}"))
    }
}

async fn socks5_connect(
    mut stream: TcpStream,
    proxy: &ProxyEndpoint,
    remote: &RemoteEndpoint,
) -> Result<TcpStream, String> {
    let wants_auth = !proxy.username.is_empty() || !proxy.password.is_empty();
    let methods: &[u8] = if wants_auth { &[0x00, 0x02] } else { &[0x00] };
    let mut hello = vec![0x05, methods.len() as u8];
    hello.extend_from_slice(methods);
    stream.write_all(&hello).await.map_err(|e| format!("Failed to send SOCKS greeting: {e}"))?;

    let mut method = [0_u8; 2];
    stream.read_exact(&mut method).await.map_err(|e| format!("Failed to read SOCKS greeting: {e}"))?;
    if method[0] != 0x05 {
        return Err("Invalid SOCKS proxy version".to_string());
    }
    match method[1] {
        0x00 => {}
        0x02 => socks5_authenticate(&mut stream, proxy).await?,
        0xff => return Err("SOCKS proxy rejected supported authentication methods".to_string()),
        other => return Err(format!("SOCKS proxy selected unsupported auth method: {other}")),
    }

    let host = remote.host.as_bytes();
    if host.len() > u8::MAX as usize {
        return Err("Remote host is too long for SOCKS5 domain address".to_string());
    }
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host);
    req.extend_from_slice(&remote.port.to_be_bytes());
    stream.write_all(&req).await.map_err(|e| format!("Failed to send SOCKS connect request: {e}"))?;

    let mut head = [0_u8; 4];
    stream.read_exact(&mut head).await.map_err(|e| format!("Failed to read SOCKS connect response: {e}"))?;
    if head[0] != 0x05 {
        return Err("Invalid SOCKS connect response version".to_string());
    }
    if head[1] != 0x00 {
        return Err(format!("SOCKS proxy connect failed with code {}", head[1]));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x03 => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len).await.map_err(|e| format!("Failed to read SOCKS bound address length: {e}"))?;
            len[0] as usize
        }
        0x04 => 16,
        other => return Err(format!("Unsupported SOCKS bound address type: {other}")),
    };
    let mut discard = vec![0_u8; addr_len + 2];
    stream.read_exact(&mut discard).await.map_err(|e| format!("Failed to read SOCKS bound address: {e}"))?;
    Ok(stream)
}

async fn socks5_authenticate(stream: &mut TcpStream, proxy: &ProxyEndpoint) -> Result<(), String> {
    let username = proxy.username.as_bytes();
    let password = proxy.password.as_bytes();
    if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
        return Err("SOCKS username or password is too long".to_string());
    }
    let mut req = vec![0x01, username.len() as u8];
    req.extend_from_slice(username);
    req.push(password.len() as u8);
    req.extend_from_slice(password);
    stream.write_all(&req).await.map_err(|e| format!("Failed to send SOCKS authentication: {e}"))?;

    let mut res = [0_u8; 2];
    stream.read_exact(&mut res).await.map_err(|e| format!("Failed to read SOCKS authentication response: {e}"))?;
    if res == [0x01, 0x00] {
        Ok(())
    } else {
        Err("SOCKS proxy authentication failed".to_string())
    }
}

// ---------------------------------------------------------------------------
// Retry helpers for proxy endpoint testing
// ---------------------------------------------------------------------------
// These wrap tokio read/write with ENOTCONN/WouldBlock retry logic,
// which is needed on macOS where async connect can resolve before the
// TCP handshake is fully complete.

async fn write_all_retry(stream: &mut TcpStream, data: &[u8]) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    loop {
        match stream.write_all(data).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected || e.kind() == std::io::ErrorKind::WouldBlock => {
                stream.writable().await.map_err(|e| format!("writable wait failed: {e}"))?;
            }
            Err(e) => return Err(format!("write failed: {e}")),
        }
    }
}

async fn read_byte_with_retry(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, String> {
    use tokio::io::AsyncReadExt;
    loop {
        match stream.read(buf).await {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected || e.kind() == std::io::ErrorKind::WouldBlock => {
                stream.readable().await.map_err(|e| format!("readable wait failed: {e}"))?;
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}

async fn read_exact_with_retry(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), String> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = read_byte_with_retry(stream, &mut buf[offset..]).await?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        offset += n;
    }
    Ok(())
}

async fn read_http_response_with_retry(stream: &mut TcpStream, max_size: usize) -> Result<Vec<u8>, String> {
    let mut response = Vec::with_capacity(max_size.min(4096));
    let mut single = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") && response.len() < max_size {
        let n = read_byte_with_retry(stream, &mut single).await?;
        if n == 0 {
            break;
        }
        response.push(single[0]);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Parse helpers for HTTP CONNECT and SOCKS5 CONNECT responses.
// These are pure functions (no I/O), testable without a running proxy.
// ---------------------------------------------------------------------------

/// Parse an HTTP CONNECT response, validating HTTP version and 2xx status.
fn parse_http_connect_response(response: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(response);
    let first_line = text.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    if parts.len() < 2 || !parts[0].starts_with("HTTP/1.") {
        return Err(format!("HTTP proxy CONNECT failed: {first_line}"));
    }
    if let Ok(code) = parts[1].parse::<u16>() {
        if (200..300).contains(&code) {
            return Ok(format!("HTTP CONNECT proxy connection successful ({code})"));
        }
        return Err(format!("HTTP proxy CONNECT failed: HTTP {code}"));
    }
    Err(format!("HTTP proxy CONNECT failed: {first_line}"))
}

/// Validate a SOCKS5 CONNECT reply header (first 4 bytes).
fn parse_socks5_connect_header(header: &[u8; 4]) -> Result<(), String> {
    if header[0] != 0x05 {
        return Err(format!("Invalid SOCKS proxy version: {}", header[0]));
    }
    if header[1] != 0x00 {
        return Err(format!("SOCKS proxy connect rejected (code {})", header[1]));
    }
    Ok(())
}

/// Test a proxy endpoint by performing a full HTTP CONNECT or SOCKS5
/// handshake.  Returns a success message on success, an error on failure.
pub async fn test_proxy_endpoint(
    proxy_type: ProxyType,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let start = Instant::now();

    // Strip brackets if user typed IPv6 as [fe80::1]
    let host = host.trim_start_matches('[').trim_end_matches(']');

    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("Proxy connection timed out ({:?})", CONNECT_TIMEOUT))?
        .map_err(|e| format!("Failed to connect to proxy: {e}"))?;

    // IPv6 zone IDs (%en0) are not valid in HTTP URIs (RFC 3986), strip for
    // HTTP CONNECT.  SOCKS5 uses binary encoding and is unaffected.
    let http_host = host.split('%').next().unwrap_or(host);
    let connect_host = if http_host.contains(':') { format!("[{http_host}]") } else { http_host.to_string() };
    let connect_target = format!("{connect_host}:{port}");

    let handshake_result = timeout(CONNECT_TIMEOUT, async {
        match proxy_type {
            ProxyType::Http => {
                let mut request = format!(
                    "CONNECT {connect_target} HTTP/1.1\r\nHost: {connect_target}\r\nUser-Agent: Mozilla/5.0\r\nProxy-Connection: Keep-Alive\r\n"
                );
                if !username.is_empty() || !password.is_empty() {
                    let token = BASE64.encode(format!("{username}:{password}"));
                    request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
                }
                request.push_str("\r\n");

                write_all_retry(&mut stream, request.as_bytes()).await?;

                let response = read_http_response_with_retry(&mut stream, 8192).await?;
                parse_http_connect_response(&response)
            }
            ProxyType::Socks5 => {
                let wants_auth = !username.is_empty() || !password.is_empty();
                let methods: &[u8] = if wants_auth { &[0x00, 0x02] } else { &[0x00] };
                let mut hello = vec![0x05, methods.len() as u8];
                hello.extend_from_slice(methods);

                write_all_retry(&mut stream, &hello).await?;

                let mut method = [0u8; 2];
                read_exact_with_retry(&mut stream, &mut method).await?;
                if method[0] != 0x05 {
                    return Err(format!("Invalid SOCKS proxy version: {}", method[0]));
                }
                match method[1] {
                    0x00 => {}
                    0x02 => {
                        let u = username.as_bytes();
                        let p = password.as_bytes();
                        if u.len() > u8::MAX as usize || p.len() > u8::MAX as usize {
                            return Err("SOCKS username or password is too long".to_string());
                        }
                        let mut req = vec![0x01, u.len() as u8];
                        req.extend_from_slice(u);
                        req.push(p.len() as u8);
                        req.extend_from_slice(p);
                        write_all_retry(&mut stream, &req).await?;
                        let mut res = [0u8; 2];
                        read_exact_with_retry(&mut stream, &mut res).await?;
                        if res != [0x01, 0x00] {
                            return Err("SOCKS proxy authentication failed".to_string());
                        }
                    }
                    0xff => return Err("SOCKS proxy rejected all supported auth methods".to_string()),
                    other => return Err(format!("SOCKS proxy selected unsupported auth method: {other}")),
                }

                // CONNECT to the proxy's own address (self-test, no external connection)
                let host_bytes = host.as_bytes();
                if host_bytes.len() > u8::MAX as usize {
                    return Err("Proxy host too long for SOCKS5 domain address".to_string());
                }
                let mut req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
                req.extend_from_slice(host_bytes);
                req.extend_from_slice(&port.to_be_bytes());
                write_all_retry(&mut stream, &req).await?;

                let mut head = [0u8; 4];
                read_exact_with_retry(&mut stream, &mut head).await?;
                parse_socks5_connect_header(&head)?;

                // Discard remaining bound address bytes
                let addr_len = match head[3] {
                    0x01 => 4,
                    0x03 => {
                        let mut len = [0u8; 1];
                        read_exact_with_retry(&mut stream, &mut len).await?;
                        len[0] as usize
                    }
                    0x04 => 16,
                    other => return Err(format!("Unsupported SOCKS bound address type: {other}")),
                };
                let mut discard = vec![0u8; addr_len + 2];
                read_exact_with_retry(&mut stream, &mut discard).await?;

                Ok("SOCKS5 proxy connection successful".to_string())
            }
        }
    })
    .await;

    match handshake_result {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(e)) => Err(format!("Proxy handshake failed ({:?}): {e}", start.elapsed())),
        Err(_) => Err(format!("Proxy handshake timed out ({:?})", CONNECT_TIMEOUT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::connection::ProxyType;

    // ── HTTP CONNECT response parsing ──────────────────────────────────────

    #[test]
    fn parse_http_success_http11() {
        let resp = parse_http_connect_response(b"HTTP/1.1 200 Connection Established\r\n\r\n");
        assert!(resp.is_ok(), "HTTP/1.1 200 should be success, got: {resp:?}");
        assert!(resp.unwrap().contains("200"));
    }

    #[test]
    fn parse_http_success_http10() {
        let resp = parse_http_connect_response(b"HTTP/1.0 200 OK\r\n\r\n");
        assert!(resp.is_ok(), "HTTP/1.0 200 should be success, got: {resp:?}");
    }

    #[test]
    fn parse_http_error_status() {
        let resp = parse_http_connect_response(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
        assert!(resp.is_err(), "502 should be error");
        assert!(resp.unwrap_err().contains("502"), "error should mention 502");
    }

    #[test]
    fn parse_http_malformed_garbage() {
        let resp = parse_http_connect_response(b"garbage response line");
        assert!(resp.is_err(), "garbage should be error");
        assert!(resp.unwrap_err().contains("garbage"), "error should contain the garbage line");
    }

    #[test]
    fn parse_http_empty_response() {
        let resp = parse_http_connect_response(b"");
        assert!(resp.is_err(), "empty should be error");
    }

    #[test]
    fn parse_http_bad_version() {
        let resp = parse_http_connect_response(b"HTTP/2.0 200 OK\r\n\r\n");
        assert!(resp.is_err(), "HTTP/2.0 should be rejected");
    }

    // ── SOCKS5 CONNECT header parsing ─────────────────────────────────────

    #[test]
    fn parse_socks5_header_success() {
        let result = parse_socks5_connect_header(&[0x05, 0x00, 0x00, 0x01]);
        assert!(result.is_ok(), "0x00 reply should be success");
    }

    #[test]
    fn parse_socks5_header_rejected() {
        let result = parse_socks5_connect_header(&[0x05, 0x03, 0x00, 0x01]);
        assert!(result.is_err(), "code 0x03 should be error");
        assert!(result.unwrap_err().contains("rejected"), "error should mention rejected");
    }

    #[test]
    fn parse_socks5_header_bad_version() {
        let result = parse_socks5_connect_header(&[0x04, 0x00, 0x00, 0x01]);
        assert!(result.is_err(), "version 4 should be error");
        assert!(result.unwrap_err().contains("version"), "error should mention version");
    }

    // ── Existing tunnel lifecycle tests ────────────────────────────────────

    #[tokio::test]
    async fn start_tunnel_reuses_existing_local_port() {
        let manager = ProxyTunnelManager::new();

        let first_port = manager
            .start_tunnel("connection", ProxyType::Http, "127.0.0.1", 8080, "", "", "db.internal", 5432)
            .await
            .expect("first proxy tunnel should start");
        let second_port = manager
            .start_tunnel("connection", ProxyType::Http, "127.0.0.1", 8081, "", "", "other-db.internal", 5433)
            .await
            .expect("existing proxy tunnel should be reused");

        assert_eq!(second_port, first_port);
        assert_eq!(manager.local_port("connection").await, Some(first_port));

        manager.stop_tunnel("connection").await;
    }
}
