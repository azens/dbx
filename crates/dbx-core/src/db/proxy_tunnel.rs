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

async fn read_with_retry(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, String> {
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
        let n = read_with_retry(stream, &mut buf[offset..]).await?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        offset += n;
    }
    Ok(())
}

async fn read_http_response_with_retry(stream: &mut TcpStream, max_size: usize) -> Result<Vec<u8>, String> {
    let mut response = Vec::with_capacity(max_size.min(4096));
    let mut buf = [0u8; 4096];
    while !response.ends_with(b"\r\n\r\n") && response.len() < max_size {
        let remaining = max_size - response.len();
        let to_read = buf.len().min(remaining);
        let n = read_with_retry(stream, &mut buf[..to_read]).await?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Parse helpers for HTTP CONNECT and SOCKS5 CONNECT responses.
// These are pure functions (no I/O), testable without a running proxy.
// ---------------------------------------------------------------------------

/// Parse an HTTP CONNECT response, validating HTTP version and 2xx status.
///
/// Rejects truncated responses (missing `\r\n\r\n` terminator), handles
/// 100 Continue (RFC 7231 §6.2) by parsing the final response, and
/// tolerates LF-only line endings (RFC 7230 §3.5). Responses exceeding
/// 8192 bytes are rejected as malformed.
fn parse_http_connect_response(response: &[u8]) -> Result<String, String> {
    if response.len() > 8192 {
        return Err("Proxy response is incomplete or malformed".to_string());
    }
    // Reject truncated responses that lack a complete header terminator.
    let terminator = if response.ends_with(b"\r\n\r\n") {
        "\r\n\r\n"
    } else if response.ends_with(b"\n\n") {
        "\n\n"
    } else {
        return Err("Proxy response is incomplete or malformed".to_string());
    };

    let text = String::from_utf8_lossy(response);

    // Handle 100 Continue: split on the terminator, find the LAST non-1xx
    // response.  This is simpler than line-by-line scanning and correctly
    // handles the case where only a 1xx response is present.
    let sections: Vec<&str> = text.split(terminator).collect();
    for section in sections.iter().rev() {
        let section = section.trim();
        if section.is_empty() {
            continue;
        }
        let first_line = section.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
        if parts.len() < 2 || !parts[0].starts_with("HTTP/1.") {
            return Err(format!("HTTP proxy CONNECT failed: {first_line}"));
        }
        if let Ok(code) = parts[1].parse::<u16>() {
            if (100..200).contains(&code) {
                // 1xx informational — skip, look at the next section.
                continue;
            }
            if (200..300).contains(&code) {
                return Ok(format!("HTTP CONNECT proxy connection successful ({code})"));
            }
            return Err(format!("HTTP proxy CONNECT failed: HTTP {code}"));
        }
        return Err(format!("HTTP proxy CONNECT failed: {first_line}"));
    }

    Err("Proxy response is incomplete or malformed".to_string())
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

/// Parse a `test_target` string (`host:port` or `[ipv6]:port`) into `(String, u16)`.
fn parse_test_target(target: &str) -> Result<(String, u16), String> {
    // IPv6: [fe80::1]:7890 -> split on ']:', strip brackets
    if let Some(rest) = target.strip_prefix('[') {
        let Some((inner, port_str)) = rest.split_once("]:") else {
            return Err("Invalid test target, expected host:port or [ipv6]:port".to_string());
        };
        let port: u16 = port_str.parse().map_err(|_| "Invalid test target port".to_string())?;
        Ok((inner.to_string(), port))
    } else {
        let (host_str, port_str) = target
            .split_once(':')
            .ok_or_else(|| "Invalid test target, expected host:port or [ipv6]:port".to_string())?;
        if host_str.is_empty() || port_str.is_empty() {
            return Err("Invalid test target, expected host:port or [ipv6]:port".to_string());
        }
        let port: u16 = port_str.parse().map_err(|_| "Invalid test target port".to_string())?;
        Ok((host_str.to_string(), port))
    }
}

/// Test a proxy endpoint by performing a full HTTP CONNECT or SOCKS5
/// handshake.  When `test_target` is `Some(host:port)` the probe connects
/// to that target (full tunnel test).  When `None` the probe performs an
/// endpoint-only liveness check that exercises auth but requires no
/// external destination.
pub async fn test_proxy_endpoint(
    proxy_type: ProxyType,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    test_target: Option<&str>,
) -> Result<String, String> {
    let start = Instant::now();

    // Strip brackets if user typed IPv6 as [fe80::1]
    let host = host.trim_start_matches('[').trim_end_matches(']');

    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("Proxy connection timed out ({:?})", CONNECT_TIMEOUT))?
        .map_err(|e| format!("Failed to connect to proxy: {e}"))?;

    let handshake_result = timeout(CONNECT_TIMEOUT, async {
        match proxy_type {
            ProxyType::Http => {
                let connect_target = match test_target.filter(|t| !t.is_empty()) {
                    Some(target) => {
                        let (th, tp) = parse_test_target(target)?;
                        format!("{th}:{tp}")
                    }
                    None => {
                        // Default: CONNECT to the proxy's own address.
                        // Works for standard proxies (loopback returns 200).
                        // ACL proxies that block self-connect return non-2xx
                        // — the user can fill in a whitelisted test_target.
                        let http_host = host.split('%').next().unwrap_or(host);
                        let self_host = if http_host.contains(':') {
                            format!("[{http_host}]")
                        } else {
                            http_host.to_string()
                        };
                        format!("{self_host}:{port}")
                    }
                };

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
                let msg = parse_http_connect_response(&response)?;
                let elapsed = start.elapsed();
                Ok(format!("{msg} — {connect_target} ({elapsed:?})"))
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

                let (target_host, target_port) = match test_target.filter(|t| !t.is_empty()) {
                    Some(target) => parse_test_target(target)?,
                    None => (host.to_string(), port),
                };
                let host_bytes = target_host.as_bytes();
                if host_bytes.len() > u8::MAX as usize {
                    return Err("Proxy target host too long for SOCKS5 domain address".to_string());
                }
                let mut req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
                req.extend_from_slice(host_bytes);
                req.extend_from_slice(&target_port.to_be_bytes());
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

                let elapsed = start.elapsed();
                Ok(format!("SOCKS5 proxy connection successful — {target_host}:{target_port} ({elapsed:?})"))
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
        // No HTTP status line and missing terminator — rejected as incomplete.
        let resp = parse_http_connect_response(b"garbage response line");
        assert!(resp.is_err(), "garbage should be error");
        assert!(resp.unwrap_err().contains("incomplete"), "should mention incomplete");
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

    #[test]
    fn parse_http_truncated_missing_terminator() {
        // Response without \r\n\r\n terminator — truncated/malformed.
        let resp = parse_http_connect_response(b"HTTP/1.1 200 OK");
        assert!(resp.is_err(), "truncated should be error");
        assert!(resp.unwrap_err().contains("incomplete"), "should mention incomplete");
    }

    #[test]
    fn parse_http_truncated_lf_only() {
        // LF-only line ending without double-\n terminator.
        let resp = parse_http_connect_response(b"HTTP/1.1 200 OK\n");
        assert!(resp.is_err(), "truncated LF-only should be error");
    }

    #[test]
    fn parse_http_continue_then_success() {
        // 100 Continue followed by the real 200 response.
        let resp =
            parse_http_connect_response(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection Established\r\n\r\n");
        assert!(resp.is_ok(), "100 Continue + 200 should be success, got: {resp:?}");
        assert!(resp.unwrap().contains("200"));
    }

    #[test]
    fn parse_http_continue_with_headers_then_success() {
        // 100 Continue with extra headers followed by 200.
        let resp =
            parse_http_connect_response(b"HTTP/1.1 100 Continue\r\nServer: Proxy\r\n\r\nHTTP/1.1 200 OK\r\n\r\n");
        assert!(resp.is_ok(), "100 Continue with headers + 200 should be success");
    }

    #[test]
    fn parse_http_continue_only() {
        // Just 100 Continue and nothing else — incomplete.
        let resp = parse_http_connect_response(b"HTTP/1.1 100 Continue\r\n\r\n");
        assert!(resp.is_err(), "100 Continue alone should be error");
    }

    #[test]
    fn parse_http_auth_challenge_407() {
        // 407 Proxy Authentication Required.
        let resp =
            parse_http_connect_response(b"HTTP/1.1 407 Proxy Auth Required\r\nProxy-Authenticate: Basic\r\n\r\n");
        assert!(resp.is_err(), "407 should be error");
        assert!(resp.unwrap_err().contains("407"), "error should mention 407");
    }

    #[test]
    fn parse_http_oversized_response() {
        // Response exceeding 8192 bytes.
        let mut oversized = b"HTTP/1.1 200 OK\r\n".to_vec();
        oversized.resize(8193, b'X');
        let resp = parse_http_connect_response(&oversized);
        assert!(resp.is_err(), "oversized should be error");
        assert!(resp.unwrap_err().contains("incomplete"), "should mention incomplete");
    }

    #[test]
    fn parse_http_lf_only_terminator() {
        // LF-only line endings with \n\n terminator (RFC 7230 §3.5 tolerance).
        let resp = parse_http_connect_response(b"HTTP/1.1 200 OK\n\n");
        assert!(resp.is_ok(), "LF-only with double-LF terminator should be success");
    }

    // ── test_target parsing ───────────────────────────────────────────────

    #[test]
    fn parse_test_target_ipv4() {
        let (host, port) = parse_test_target("192.168.1.1:8080").unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_test_target_ipv6() {
        let (host, port) = parse_test_target("[fe80::1]:7890").unwrap();
        assert_eq!(host, "fe80::1");
        assert_eq!(port, 7890);
    }

    #[test]
    fn parse_test_target_hostname() {
        let (host, port) = parse_test_target("proxy.example.com:3128").unwrap();
        assert_eq!(host, "proxy.example.com");
        assert_eq!(port, 3128);
    }

    #[test]
    fn parse_test_target_missing_port() {
        let err = parse_test_target("192.168.1.1").unwrap_err();
        assert!(err.contains("Invalid test target"), "should mention invalid");
    }

    #[test]
    fn parse_test_target_empty_host() {
        let err = parse_test_target(":8080").unwrap_err();
        assert!(err.contains("Invalid test target"), "should mention invalid");
    }

    #[test]
    fn parse_test_target_bad_port() {
        let err = parse_test_target("host:badport").unwrap_err();
        assert!(err.contains("port"), "should mention port");
    }

    #[test]
    fn parse_test_target_bad_ipv6_missing_bracket() {
        let err = parse_test_target("[fe80::1:7890").unwrap_err();
        assert!(err.contains("Invalid test target"), "malformed IPv6 should fail");
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
