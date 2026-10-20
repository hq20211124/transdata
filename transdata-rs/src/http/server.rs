//! Minimal, high-concurrency async HTTP/1.1 server.
//!
//! Each connection is handled in its own tokio task, so a large file
//! transfer never blocks another device's requests. Uploads stream to disk
//! in fixed-size chunks (constant memory); downloads stream back from disk.

use std::io::ErrorKind;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task;

/// A connection that can carry HTTP: a plain TCP socket or a TLS stream.
///
/// The whole request handler is written against this trait, so the exact
/// same code serves HTTP and HTTPS — the only difference is what happens
/// before the handler is called (TLS handshake or not).
pub trait Conn: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Conn for T {}

use crate::config::Config;

const MAX_HEAD: usize = 8 * 1024;
const CHUNK: usize = 256 * 1024;

/// Fixed pairing secret used by the unit tests (the real one is random per
/// run and lives in `Config::pair_secret`).
#[cfg(test)]
pub const TEST_SECRET: &str = "test-secret";
/// Fixed pairing code used by the unit tests.
#[cfg(test)]
pub const TEST_CODE: &str = "12345678";
/// /files listing cap; deeper trees are truncated and flagged.
const FILE_LIST_CAP: usize = 10_000;
/// /verify per-request cap.
const VERIFY_CAP: usize = 20_000;
/// How many uploads may run at the same time. Without this a single device
/// (or a script) can saturate the disk and starve every other client.
const MAX_CONCURRENT_UPLOADS: usize = 8;
/// Maximum directory depth walked when listing / verifying the transfer dir
/// (also breaks symlink cycles).
const MAX_WALK_DEPTH: usize = 24;
/// Maximum length of one path segment (bytes).
const MAX_SEGMENT_LEN: usize = 200;
/// Maximum length of a whole relative path (bytes).
const MAX_PATH_LEN: usize = 1024;

/// Serializes the "check for name -> create file" step so two concurrent
/// uploads of the same folder cannot both create `<x>_1`.
static UPLOAD_LOCK: Mutex<()> = Mutex::new(());

/// Serve until the process ends.
///
/// The listener is re-created when the user switches HTTPS on or off: the
/// port stays the same, only the protocol on it changes.
pub async fn serve(cfg: Arc<Config>) {
    cfg.set_running(true);
    let mut mode_rx = cfg.tls_tx.subscribe();

    'outer: loop {
        // The atomic flag is the authoritative state; the watch channel
        // only tells us *when* it changed.
        let tls_on = cfg.tls_on();
        let acceptor = if tls_on {
            match cfg.tls_material() {
                Some(m) => Some(tokio_rustls::TlsAcceptor::from(m.server_config.clone())),
                None => {
                    eprintln!("HTTPS requested but no certificate available");
                    None
                }
            }
        } else {
            None
        };

        let listener = match TcpListener::bind(("0.0.0.0", cfg.port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bind :{} failed: {e}", cfg.port);
                // Tell the GUI: a window without a server is worse than no
                // window at all (the QR code would point nowhere).
                cfg.set_server_error(format!("无法监听端口 {}：{}", cfg.port, e));
                cfg.set_running(false);
                return;
            }
        };
        println!(
            "listening on 0.0.0.0:{} ({})",
            cfg.port,
            if tls_on { "https" } else { "http" }
        );

        loop {
            tokio::select! {
                res = listener.accept() => {
                    match res {
                        Ok((sock, addr)) => {
                            let _ = sock.set_nodelay(true);
                            let peer = addr.ip().to_string();
                            let cfg = cfg.clone();
                            let acceptor = acceptor.clone();
                            task::spawn(async move {
                                match acceptor {
                                    Some(acc) => {
                                        let _ = handle_tls_conn(sock, &cfg, acc, &peer).await;
                                    }
                                    None => {
                                        let _ = handle_conn(sock, &cfg, &peer).await;
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            if e.kind() == ErrorKind::WouldBlock {
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
                changed = mode_rx.changed() => {
                    if changed.is_err() {
                        break 'outer; // the sender is gone
                    }
                    println!("TLS mode changed -> rebinding");
                    break;
                }
            }
        }
    }
}

/// A TCP stream that first replays a few already-read bytes and then
/// forwards to the socket. Used to hand the first byte of a TLS
/// ClientHello back to the TLS layer.
struct PrefixedStream {
    prefix: Vec<u8>,
    stream: TcpStream,
}

impl tokio::io::AsyncRead for PrefixedStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.prefix.is_empty() {
            let take = this.prefix.len().min(buf.remaining());
            let chunk: Vec<u8> = this.prefix.drain(..take).collect();
            buf.put_slice(&chunk);
            return std::task::Poll::Ready(Ok(()));
        }
        let stream = std::pin::Pin::new(&mut this.stream);
        stream.poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PrefixedStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_shutdown(cx)
    }
}

/// Handle a connection on a port that serves HTTPS.
///
/// The first byte decides what the client wants: `0x16` is a TLS
/// handshake, everything else is plain HTTP. A plain HTTP request to an
/// HTTPS port does not fail with a cryptic network error — it gets a page
/// that explains what to do instead.
async fn handle_tls_conn(
    mut sock: TcpStream,
    cfg: &Arc<Config>,
    acceptor: tokio_rustls::TlsAcceptor,
    peer: &str,
) -> Result<(), std::io::Error> {
    let mut first = [0u8; 1];
    if sock.read_exact(&mut first).await.is_err() {
        return Ok(());
    }
    if first[0] == 0x16 {
        // TLS handshake.
        let stream = PrefixedStream {
            prefix: first.to_vec(),
            stream: sock,
        };
        let tls = match acceptor.accept(stream).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tls handshake failed: {e}");
                return Ok(());
            }
        };
        handle_conn(tls, cfg, peer).await
    } else {
        // Plain HTTP on an HTTPS port: point the user to the right URL.
        let mut buf = Vec::with_capacity(8192);
        buf.push(first[0]);
        let head_len = read_head(&mut sock, &mut buf).await?;
        let _head = String::from_utf8_lossy(&buf[..head_len]).to_string();
        write_https_only_notice(&mut sock, cfg).await
    }
}

/// The page shown when someone opens `http://…` on an HTTPS port.
async fn write_https_only_notice(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
) -> Result<(), std::io::Error> {
    let url = format!("https://{}:{}", crate::net::display_ip(), cfg.port);
    let body = format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>请使用 HTTPS</title><style>
body{{font:15px/1.6 -apple-system,"Segoe UI","Microsoft YaHei",sans-serif;background:#f4f5f8;color:#171a26;
display:grid;place-items:center;min-height:100vh;margin:0;padding:24px}}
.card{{background:#fff;border:1px solid #e6e8f0;border-radius:16px;padding:28px 26px;max-width:420px;
box-shadow:0 10px 30px rgba(16,24,40,.08)}}
h1{{font-size:19px;margin:0 0 10px}}p{{margin:8px 0;color:#6b7280}}
a{{display:inline-block;margin-top:14px;background:#4f46e5;color:#fff;text-decoration:none;
padding:10px 18px;border-radius:9px;font-weight:600}}
</style></head><body><div class="card">
<h1>这个端口已经启用 HTTPS</h1>
<p>当前地址是 http://，浏览器无法建立加密连接。请改用下面的地址：</p>
<p style="font-family:monospace;color:#4f46e5">{url}</p>
<a href="{url}">打开 {url}</a>
<p style="font-size:12.5px;margin-top:18px">手机首次访问会提示一次证书不受信任，点「高级 → 继续前往」即可。</p>
</div></body></html>"#
    );
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(sec_headers().as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body.as_bytes());
    sock.write_all(&out).await?;
    sock.flush().await?;
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// request parsing

struct Request {
    method: String,
    path: String,
    boundary: Option<String>,
    content_length: usize,
    ws: bool,
    /// `X-TD-Secret` header. Only the desktop app on the *same machine*
    /// uses this; browsers authenticate with the session cookie.
    secret: Option<String>,
    /// Raw `Cookie` header.
    cookie: Option<String>,
    /// `Host` header (used for the same-origin check).
    host: Option<String>,
    /// `Origin` header (used for the same-origin check).
    origin: Option<String>,
    /// Does the client accept a gzip-encoded body?
    accept_gzip: bool,
    /// `If-None-Match` header (for the page's ETag / 304).
    if_none_match: Option<String>,
}

fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.lines();
    let start = lines.next()?;
    let mut parts = start.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    // The target may carry a query string (ignored: no credential ever
    // travels in the URL any more).
    let (raw_path, _query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };
    let path = url_decode(raw_path);

    let mut boundary = None;
    let mut content_length = 0usize;
    let mut ws = false;
    let mut secret = None;
    let mut cookie = None;
    let mut host = None;
    let mut origin = None;
    let mut accept_gzip = false;
    let mut if_none_match = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        let lname = name.trim().to_ascii_lowercase();
        let v = value.trim();
        if lname == "content-type" {
            // Header names are case-insensitive, but the boundary *value* must
            // keep its original casing (browsers use case-sensitive random
            // strings like `----WebKitFormBoundaryQzXfKq3a`). Lowercasing the
            // whole line would corrupt it and the delimiters would never match.
            if let Some(b) = v.split_once("boundary=") {
                boundary = Some(
                    b.1.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_matches('"')
                        .to_string(),
                );
            }
        } else if lname == "content-length" {
            content_length = v.parse().unwrap_or(0);
        } else if lname == "upgrade" && v.eq_ignore_ascii_case("websocket") {
            ws = true;
        } else if lname == "x-td-secret" {
            secret = Some(v.to_string());
        } else if lname == "cookie" {
            cookie = Some(v.to_string());
        } else if lname == "host" {
            host = Some(v.to_string());
        } else if lname == "origin" {
            origin = Some(v.to_string());
        } else if lname == "accept-encoding" && v.to_ascii_lowercase().contains("gzip") {
            accept_gzip = true;
        } else if lname == "if-none-match" {
            if_none_match = Some(v.to_string());
        }
    }
    Some(Request {
        method,
        path,
        boundary,
        content_length,
        ws,
        secret,
        cookie,
        host,
        origin,
        accept_gzip,
        if_none_match,
    })
}

/// Value of a cookie by name.
fn cookie_value<'a>(cookie: &'a str, name: &str) -> Option<&'a str> {
    for kv in cookie.split(';') {
        let kv = kv.trim();
        if let Some((k, v)) = kv.split_once('=') {
            if k.trim() == name {
                return Some(v.trim());
            }
        }
    }
    None
}

/// Constant-time string comparison (no early exit on the first difference).
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Is this request allowed to talk to the API?
///
/// Two ways in:
///
/// 1. A **session cookie** (`td_sid`) issued by `/pair` after the device
///    proved it knows the pairing code / secret. The cookie is `HttpOnly`
///    and `SameSite=Strict`, so scripts on other sites cannot read it and
///    it is not sent on cross-site requests.
/// 2. The `X-TD-Secret` header carrying the pairing secret. Only the
///    desktop app on the same machine uses this path (it pairs once at
///    startup and then behaves like any other client).
///
/// The old design handed the token to *everybody* who asked for the index
/// page, which meant any device on the LAN could list, download and
/// delete files. The secret is now only ever displayed on the desktop
/// screen or embedded in the QR code.
fn authorized(cfg: &Config, req: &Request) -> bool {
    if let Some(c) = &req.cookie {
        if let Some(sid) = cookie_value(c, "td_sid") {
            if cfg.session_valid(sid) {
                return true;
            }
        }
    }
    if let Some(s) = &req.secret {
        if ct_eq(s, &cfg.pair_secret) {
            return true;
        }
    }
    false
}

/// Same-origin check for state-changing requests.
///
/// A browser always sends `Origin` on cross-origin POST/DELETE. If it is
/// present and does not match the `Host` we serve, the request comes from
/// another site and is rejected. This is the second line of defence next
/// to `SameSite=Strict` on the session cookie.
///
/// The scheme has to match the one the server is currently using: `http`
/// normally, `https` when the user switched HTTPS on.
fn same_origin(req: &Request, cfg: &Config) -> bool {
    let origin = match &req.origin {
        Some(o) if !o.is_empty() => o,
        _ => return true, // not a browser request
    };
    let host = match &req.host {
        Some(h) if !h.is_empty() => h,
        _ => return true,
    };
    // `Origin: http://192.168.1.10:9999` -> compare scheme + authority.
    let (scheme, authority) = match origin.split_once("://") {
        Some((s, rest)) => (s, rest.split('/').next().unwrap_or("")),
        None => return true,
    };
    let expected = if cfg.tls_on() { "https" } else { "http" };
    if !scheme.eq_ignore_ascii_case(expected) {
        return false;
    }
    authority.eq_ignore_ascii_case(host)
}

/// Read the HTTP request head.
///
/// Reads in 4 KB chunks instead of one byte at a time: the old byte-at-a-time
/// loop cost one syscall + one reactor wake-up *per byte*, which showed up
/// as 15-30 ms of pure latency for a 1 KB header.
async fn read_head(sock: &mut impl Conn, buf: &mut Vec<u8>) -> Result<usize, std::io::Error> {
    let mut tmp = [0u8; 4096];
    loop {
        // Terminator already in the buffer?
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(pos + 4);
        }
        if buf.len() >= MAX_HEAD {
            return Ok(buf.len());
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(buf.len())
}

// ---------------------------------------------------------------------------
// connection handler

/// Read body bytes: serve the bytes that were read ahead together with the
/// request head first, then fall back to the socket.
async fn read_body(
    sock: &mut impl Conn,
    pending: &mut Vec<u8>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    if !pending.is_empty() {
        let n = pending.len().min(buf.len());
        buf[..n].copy_from_slice(&pending[..n]);
        pending.drain(..n);
        return Ok(n);
    }
    sock.read(buf).await
}

/// Like `read_body`, but fills the whole buffer.
async fn read_body_exact(
    sock: &mut impl Conn,
    pending: &mut Vec<u8>,
    buf: &mut [u8],
) -> std::io::Result<()> {
    let mut filled = 0usize;
    if !pending.is_empty() {
        let n = pending.len().min(buf.len());
        buf[..n].copy_from_slice(&pending[..n]);
        pending.drain(..n);
        filled = n;
    }
    if filled < buf.len() {
        sock.read_exact(&mut buf[filled..]).await?;
    }
    Ok(())
}

async fn handle_conn(
    mut sock: impl Conn,
    cfg: &Arc<Config>,
    peer: &str,
) -> Result<(), std::io::Error> {
    let mut buf = Vec::with_capacity(8192);
    let head_len = read_head(&mut sock, &mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..head_len]).to_string();
    // The head read may have pulled in the beginning of the body as well
    // (they usually arrive in one TCP segment). Those bytes belong to the
    // body, not to the head, so hand them to the body readers first.
    let head_bytes = buf[..head_len].to_vec();
    let mut pending: Vec<u8> = buf[head_len..].to_vec();
    drop(buf);

    let req = match parse_request(&head) {
        Some(r) => r,
        None => {
            return write_body(&mut sock, "400 Bad Request", "text/plain", b"Bad Request")
                .await;
        }
    };

    if req.method == "OPTIONS" {
        // No CORS headers on purpose: the page is same-origin. A preflight
        // from another site therefore gets no permission to read anything.
        write_body(&mut sock, "204 No Content", "text/plain", b"").await?;
        return Ok(());
    }

    // Static assets carry no secrets, so they are public.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/app.css") => {
            return write_asset(
                &mut sock,
                &req,
                "text/css; charset=utf-8",
                &cfg.asset_css,
                &cfg.asset_css_etag,
            )
            .await
        }
        ("GET", "/app.js") => {
            return write_asset(
                &mut sock,
                &req,
                "text/javascript; charset=utf-8",
                &cfg.asset_js,
                &cfg.asset_js_etag,
            )
            .await
        }
        _ => {}
    }

    // Pairing: the only public API. Proves the device knows the code (or
    // scanned the QR) and hands out a session cookie.
    if req.method == "POST" && req.path == "/pair" {
        if !same_origin(&req, cfg) {
            return write_json_status(&mut sock, "403 Forbidden", "Cross-origin request rejected").await;
        }
        return pair(&mut sock, cfg, &req, &peer, &mut pending).await;
    }

    // State-changing requests must come from our own page.
    if matches!(req.method.as_str(), "POST" | "DELETE" | "PUT" | "PATCH")
        && !same_origin(&req, cfg)
    {
        return write_json_status(&mut sock, "403 Forbidden", "Cross-origin request rejected").await;
    }

    // The page itself is public: it shows the pairing screen to devices
    // that are not authorized yet. Every API call needs a session.
    let is_page = req.method == "GET" && (req.path == "/" || req.path == "/index.html");
    if !is_page && !authorized(cfg, &req) {
        return write_json_status(&mut sock, "401 Unauthorized", "Not paired").await;
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/ws") if req.ws => {
            ws_conn(sock, cfg, head_bytes).await
        }
        ("GET", "/note") => {
            let (text, v) = cfg.read_note();
            let body = format!(
                r#"{{"success":true,"v":{v},"text":"{}"}}"#,
                Config::json_escape(&text)
            );
            write_json(&mut sock, &body).await
        }
        ("POST", "/note") => {
            // Shared quick-note: stream the body in bounded chunks so memory
            // stays flat. Hard cap: a peer must not be able to OOM the box.
            let total = req.content_length.min(crate::config::NOTE_MAX_BYTES);
            if req.content_length > crate::config::NOTE_MAX_BYTES {
                return write_json_status(
                    &mut sock,
                    "413 Payload Too Large",
                    "便签内容过长",
                )
                .await;
            }
            let mut body: Vec<u8> = Vec::with_capacity(total.min(65536));
            while body.len() < total {
                let take = total - body.len();
                let mut chunk = vec![0u8; take.min(65536)];
                match read_body_exact(&mut sock, &mut pending, &mut chunk).await {
                    Ok(_) => body.extend_from_slice(&chunk),
                    Err(e) => {
                        eprintln!("/note body read failed: {e}");
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&body).into_owned();
            let v = cfg.store_note(&text);
            write_json(
                &mut sock,
                &format!(r#"{{"success":true,"v":{v}}}"#),
            )
            .await
        }
        ("GET", "/") | ("GET", "/index.html") => {
            let paired = authorized(cfg, &req);
            write_page(&mut sock, cfg, &req, paired).await
        }
        ("GET", "/info") => {
            let dir = cfg.upload_dir();
            let count = dir
                .read_dir()
                .map(|d| d.filter_map(|e| e.ok()).filter(|f| f.path().is_file()).count())
                .unwrap_or(0);
            let url = format!("http://{}:{}", crate::net::display_ip(), cfg.port);
            let dir_s = dir.to_string_lossy().replace('"', "\\\"");
            let active = cfg
                .active_transfers
                .load(std::sync::atomic::Ordering::SeqCst);
            let quota = cfg.quota_left.load(std::sync::atomic::Ordering::SeqCst);
            let paired = cfg.session_count();
            let body = format!(
                r#"{{"success":true,"url":"{url}","files_count":{count},"save_dir":"{dir_s}","active":{active},"paired":{paired},"quota_left":{quota}}}"#
            );
            write_json(&mut sock, &body).await
        }
        ("GET", "/files") => {
            // Recursive listing so uploaded folder trees are visible, each
            // entry's `name` is the slash-separated relative path.
            //
            // Cached for 2 s: the directory walk + JSON build cost ~0.5 s for
            // 2 000 files and the download list asks for it on every tab
            // switch. Uploads and deletes invalidate the cache at once.
            if let Some(cached) = cfg.cached_files() {
                return write_json(&mut sock, &cached).await;
            }
            let dir = cfg.upload_dir();
            let mut items: Vec<String> = Vec::new();
            let mut truncated = false;
            let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(dir.clone(), 0)];
            while let Some((d, depth)) = stack.pop() {
                if depth > MAX_WALK_DEPTH {
                    truncated = true;
                    break;
                }
                if let Ok(rd) = d.read_dir() {
                    for e in rd.flatten() {
                        let p = e.path();
                        // Skip symlinks: they can point outside the transfer
                        // directory and can form cycles.
                        match e.file_type() {
                            Ok(ft) if ft.is_symlink() => continue,
                            Err(_) => continue,
                            _ => {}
                        }
                        if p.is_dir() {
                            stack.push((p.to_path_buf(), depth + 1));
                        } else {
                            let rel = p
                                .strip_prefix(&dir)
                                .ok()
                                .map(|s| s.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_default();
                            if items.len() >= FILE_LIST_CAP {
                                truncated = true;
                                break;
                            }
                            let meta = e.metadata().ok();
                            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                            let mtime = meta
                                .as_ref()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            items.push(format!(
                                r#"{{"name":"{}","size":{},"modified":{}}}"#,
                                Config::json_escape(&rel),
                                size,
                                mtime
                            ));
                        }
                    }
                }
                if truncated {
                    break;
                }
            }
            let body = format!(
                r#"{{"success":true,"truncated":{},"files":[{}]}}"#,
                truncated,
                items.join(",")
            );
            cfg.store_files_cache(body.clone());
            write_json(&mut sock, &body).await
        }
        ("GET", p) if p.starts_with("/download/") => {
            match resolve_rel(cfg, &url_decode(&p["/download/".len()..])) {
                Some(p) => download(&mut sock, cfg, &p).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        ("DELETE", p) if p.starts_with("/delete/") => {
            match resolve_rel(cfg, &url_decode(&p["/delete/".len()..])) {
                Some(p) => delete(&mut sock, cfg, &p).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        ("POST", "/upload") => upload(&mut sock, cfg, &req.boundary, &mut pending).await,
        ("POST", "/verify") => verify(&mut sock, cfg, &req, &mut pending).await,
        ("GET", p) if p.starts_with("/zip/") => {
            match resolve_rel(cfg, &url_decode(&p["/zip/".len()..])) {
                Some(p) => zip_dir(&mut sock, cfg, &p).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        // 打包整个传输目录（根目录下有多个文件夹/文件时也能一把全下）
        ("GET", "/zip-all") => {
            let root = cfg.upload_dir();
            zip_dir(&mut sock, cfg, &root).await
        }
        _ => write_json_err(&mut sock, "Not Found").await,
    }
}

// ---------------------------------------------------------------------------
// pairing
// ---------------------------------------------------------------------------

/// `POST /pair` with `{"code":"12345678"}` (the code shown in the desktop
/// window) or `{"secret":"<hex>"}` (the secret embedded in the QR code).
///
/// On success a session id is issued in an `HttpOnly; SameSite=Strict`
/// cookie. Attempts are rate limited per device and locked out completely
/// after too many failures, so the 8-digit code cannot be brute forced.
async fn pair(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    req: &Request,
    peer: &str,
    pending: &mut Vec<u8>,
) -> Result<(), std::io::Error> {
    if cfg.pair_locked.load(std::sync::atomic::Ordering::SeqCst) {
        return write_json_status(
            sock,
            "403 Forbidden",
            "配对失败次数过多，已锁定，请重启程序",
        )
        .await;
    }

    // Per-device rate limit. The lock must not be held across an await.
    let too_many = {
        let mut map = cfg.pair_attempts.lock().unwrap();
        let entry = map.entry(peer.to_string()).or_default();
        entry.retain(|t| t.elapsed() < crate::config::PAIR_WINDOW);
        let too_many = entry.len() >= crate::config::PAIR_ATTEMPTS;
        entry.push(std::time::Instant::now());
        too_many
    };
    if too_many {
        return write_json_status(
            sock,
            "429 Too Many Requests",
            "尝试次数过多，请稍后再试",
        )
        .await;
    }

    // Small JSON body.
    let len = req.content_length.min(4096);
    let mut raw = Vec::with_capacity(len);
    while raw.len() < len {
        let take = (len - raw.len()).min(4096);
        let mut chunk = vec![0u8; take];
        match read_body_exact(sock, pending, &mut chunk).await {
            Ok(_) => raw.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    let json: serde_json::Value = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);

    let given_code = json.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let given_secret = json.get("secret").and_then(|v| v.as_str()).unwrap_or("");
    let ok = (!given_code.is_empty() && ct_eq(given_code, &cfg.pair_code))
        || (!given_secret.is_empty() && ct_eq(given_secret, &cfg.pair_secret));

    if !ok {
        let fails = cfg.pair_failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if fails >= crate::config::PAIR_MAX_FAILURES {
            cfg.pair_locked.store(true, std::sync::atomic::Ordering::SeqCst);
            eprintln!("pairing locked after {fails} failed attempts");
        }
        return write_json_status(sock, "401 Unauthorized", "配对码不正确").await;
    }

    let sid = cfg.new_session();
    // `Secure` when the server speaks HTTPS: the session id must never
    // travel in clear text.
    let secure = if cfg.tls_on() { "; Secure" } else { "" };
    let cookie = format!(
        "Set-Cookie: td_sid={sid}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age={}\r\n",
        crate::config::SESSION_TTL.as_secs()
    );
    let body = b"{\"success\":true}";
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: application/json; charset=utf-8\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(cookie.as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    Ok(())
}

/// TCP stream that first replays already-read HTTP request head bytes, then
/// transparently forwards to the socket. Needed because `accept_async()`
/// wants to parse the upgrade request itself, but we already read the head.
struct HeadedStream<S> {
    head: std::vec::IntoIter<u8>,
    stream: S,
}

impl<S: Conn> tokio::io::AsyncRead for HeadedStream<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let take = this.head.len().min(buf.remaining());
        if take > 0 {
            let chunk: Vec<u8> = this.head.by_ref().take(take).collect();
            buf.put_slice(&chunk);
            return std::task::Poll::Ready(Ok(()));
        }
        let stream = std::pin::Pin::new(&mut this.stream);
        stream.poll_read(cx, buf)
    }
}

impl<S: Conn> tokio::io::AsyncWrite for HeadedStream<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let stream = std::pin::Pin::new(&mut self.get_mut().stream);
        stream.poll_shutdown(cx)
    }
}

/// Serve one WebSocket connection: send the current note state, then forward
/// live note updates from the broadcast channel. Text frames received from
/// the client act as a "send" action (same as POST /note).
async fn ws_conn(
    sock: impl Conn,
    cfg: &Arc<Config>,
    head: Vec<u8>,
) -> Result<(), std::io::Error> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let headed = HeadedStream {
        head: head.into_iter(),
        stream: sock,
    };
    let ws = match tokio_tungstenite::accept_async(headed).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("ws handshake failed: {e}");
            return Ok(());
        }
    };
    let (mut wtx, mut wrx) = ws.split();

    // Initial state for the client.
    let (text, v) = cfg.read_note();
    let init = format!(
        r#"{{"v":{v},"text":"{}"}}"#,
        Config::json_escape(&text)
    );
    let _ = wtx.send(WsMsg::Text(init)).await;

    let mut rx = cfg.note_rx();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        if wtx.send(WsMsg::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break, // all senders dropped (shutting down)
                }
            }
            inc = wrx.next() => {
                match inc {
                    None => break,
                    Some(Err(_)) => break,
                    Some(Ok(WsMsg::Text(t))) => {
                        // Client-side "send" action.
                        let _ = cfg.store_note(&t);
                    }
                    Some(Ok(WsMsg::Ping(p))) => {
                        let _ = wtx.send(WsMsg::Pong(p)).await;
                    }
                    Some(Ok(WsMsg::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    let _ = wtx.close().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// response writers

/// Security headers that go on every response.
///
/// The page uses a strict CSP: no inline scripts, no inline styles, no
/// third-party anything. That way a single HTML injection bug cannot turn
/// into script execution, and the page can never phone home.
fn sec_headers() -> &'static str {
    "X-Content-Type-Options: nosniff\r\n\
     X-Frame-Options: DENY\r\n\
     Referrer-Policy: no-referrer\r\n\
     Cross-Origin-Opener-Policy: same-origin\r\n\
     Cross-Origin-Resource-Policy: same-origin\r\n\
     Permissions-Policy: geolocation=(), camera=(), microphone=(), usb=()\r\n"
}

/// Content-Security-Policy for the HTML page.
///
/// `script-src 'self'` / `style-src 'self'` means the page may not run a
/// single inline handler, so an injected `<script>` or `onclick=` does
/// nothing. `connect-src 'self' ws: wss:` is what allows the note socket.
fn csp_header() -> &'static str {
    "Content-Security-Policy: default-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'; object-src 'none'; img-src 'self' data:; \
     connect-src 'self' ws: wss:; style-src 'self'; script-src 'self'\r\n"
}

async fn write_body(
    sock: &mut impl Conn,
    status: &str,
    ctype: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(format!("HTTP/1.1 {status}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(sec_headers().as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    // Close politely: for TLS this sends close_notify, so clients do not
    // see an "unexpected EOF" error.
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    Ok(())
}

/// Serve a static asset (CSS / JS).
///
/// The cache header matters more than it looks: with max-age=3600 a phone
/// keeps the *old* CSS/JS for an hour **without revalidating**, so a rebuilt
/// app serves new HTML together with stale styles - and the page then
/// behaves weirdly (lists that no longer scroll, buttons that do nothing).
/// no-cache + ETag means the browser always asks and gets a cheap 304
/// when nothing changed.
async fn write_asset(
    sock: &mut impl Conn,
    req: &Request,
    ctype: &str,
    body: &[u8],
    etag: &str,
) -> Result<(), std::io::Error> {
    if let Some(inm) = &req.if_none_match {
        if inm.split(',').any(|t| t.trim() == etag) {
            let head = format!(
                "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: no-cache, must-revalidate\r\n{}Connection: close\r\n\r\n",
                sec_headers()
            );
            sock.write_all(head.as_bytes()).await?;
            sock.flush().await?;
            return Ok(());
        }
    }
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(format!("ETag: {etag}\r\n").as_bytes());
    out.extend_from_slice(b"Cache-Control: no-cache, must-revalidate\r\n");
    out.extend_from_slice(sec_headers().as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    Ok(())
}

/// Serve the index page.
///
/// Two variants:
/// - paired: the full app
/// - not paired: the pairing screen (no secret in the markup, ever)
///
/// - gzip when the client accepts it (30 KB -> ~6 KB, which matters a lot on
///   a phone over Wi-Fi)
/// - ETag + `must-revalidate` so a reload costs a 304 instead of a full page
async fn write_page(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    req: &Request,
    paired: bool,
) -> Result<(), std::io::Error> {
    let (page, page_gz, etag) = if paired {
        (&cfg.page_app, &cfg.page_app_gz, &cfg.page_app_etag)
    } else {
        (&cfg.page_pair, &cfg.page_pair_gz, &cfg.page_pair_etag)
    };
    // Not modified? Then the browser can keep its copy.
    if let Some(inm) = &req.if_none_match {
        if inm.split(',').any(|t| t.trim() == etag) {
            let head = format!(
                "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: private, max-age=0, must-revalidate\r\n{}Connection: close\r\n\r\n",
                sec_headers()
            );
            sock.write_all(head.as_bytes()).await?;
            sock.flush().await?;
            return Ok(());
        }
    }
    let gz = req.accept_gzip;
    let body = if gz { page_gz.as_slice() } else { page.as_slice() };
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
    if gz {
        out.extend_from_slice(b"Content-Encoding: gzip\r\n");
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(format!("ETag: {etag}\r\n").as_bytes());
    out.extend_from_slice(b"Cache-Control: private, max-age=0, must-revalidate\r\n");
    out.extend_from_slice(csp_header().as_bytes());
    out.extend_from_slice(sec_headers().as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    Ok(())
}

async fn write_json(sock: &mut impl Conn, body: &str) -> Result<(), std::io::Error> {
    write_body(
        sock,
        "200 OK",
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

async fn write_json_err(sock: &mut impl Conn, msg: &str) -> Result<(), std::io::Error> {
    write_json_status(sock, "404 Not Found", msg).await
}

/// JSON error with an explicit HTTP status.
async fn write_json_status(
    sock: &mut impl Conn,
    status: &str,
    msg: &str,
) -> Result<(), std::io::Error> {
    let body = format!(
        r#"{{"success":false,"error":"{}"}}"#,
        msg.replace('"', "\\\"")
    );
    write_body(
        sock,
        status,
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

/// Normalize a client-supplied relative path into one that is safe to join
/// onto the upload dir.
///
/// Rejects: absolute paths, drive letters, UNC paths, `.` / `..` segments,
/// empty segments, control characters, the Windows-illegal characters
/// `< > : " | ? *` (a colon would open an NTFS alternate data stream!), names
/// ending in a dot or a space, the reserved device names (`CON`, `NUL`,
/// `COM1`, ...) and over-long segments.
///
/// `\` -> `/` so a Windows-style path cannot sneak in either.
fn safe_rel(s: &str) -> Option<String> {
    let s = s.replace('\\', "/");
    if s.len() > MAX_PATH_LEN {
        return None;
    }
    let mut segs: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
        if seg.chars().any(|c| c.is_control()) {
            return None;
        }
        // Illegal on Windows; `:` in particular opens an ADS, `*`/`?` are
        // wildcards, `"`/`|`/`<`/`>` are shell metacharacters.
        if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
            return None;
        }
        // Windows silently strips trailing dots and spaces, which would let
        // "evil.exe." and "evil.exe" collide and ".. " behave like "..".
        if seg.ends_with('.') || seg.ends_with(' ') {
            return None;
        }
        if seg.len() > MAX_SEGMENT_LEN {
            return None;
        }
        if is_reserved_name(seg) {
            return None;
        }
        segs.push(seg);
    }
    if segs.is_empty() {
        return None;
    }
    if segs[0].len() == 2 && segs[0].as_bytes()[1] == b':' {
        return None; // Windows drive letter
    }
    Some(segs.join("/"))
}

/// Windows reserved device names: `CON`, `PRN`, `AUX`, `NUL`, `COM1`..`COM9`,
/// `LPT1`..`LPT9` — with or without an extension (`CON.txt` is still `CON`).
fn is_reserved_name(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        s if s.len() == 4
            && (s.starts_with("COM") || s.starts_with("LPT"))
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0' =>
        {
            true
        }
        _ => false,
    }
}

/// Resolve a client-supplied relative path to an absolute path that is
/// guaranteed to live inside the transfer directory.
///
/// On top of the lexical checks of [`safe_rel`] this resolves symlinks:
/// the longest existing ancestor is canonicalized and must still be inside
/// the canonicalized transfer dir, so a junction/symlink inside the
/// directory can never be used to read or delete files outside of it.
fn resolve_rel(cfg: &Config, rel: &str) -> Option<std::path::PathBuf> {
    let rel = safe_rel(rel)?;
    let base = cfg.upload_dir();
    let base_canon = base.canonicalize().ok()?;
    let joined = base.join(&rel);

    let mut probe = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = probe.canonicalize() {
            if !c.starts_with(&base_canon) {
                return None; // escapes the transfer directory
            }
            let mut out = c;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return Some(out);
        }
        match probe.file_name() {
            Some(n) => {
                tail.push(n.to_os_string());
                if !probe.pop() {
                    return None;
                }
            }
            None => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// download (streamed from disk in chunks)

async fn download(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    path: &std::path::Path,
) -> Result<(), std::io::Error> {
    let file = path.to_path_buf();
    if !file.is_file() {
        return write_json_err(sock, "Not Found").await;
    }
    let mime = guess_mime(&file);
    let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let raw_name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    // The quoted form must not contain quotes or backslashes.
    let name: String = raw_name
        .chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .collect();
    let encoded = percent_encode(&raw_name);
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {len}\r\n").as_bytes());
    out.extend_from_slice(
        format!(
            "Content-Disposition: attachment; filename=\"{name}\"; filename*=UTF-8''{encoded}\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    sock.write_all(&out).await?;

    // Async disk reads: no blocking tokio workers, no per-chunk flush
    // (TCP writes are already coalesced by the kernel).
    let _active = ActiveGuard::new(cfg);
    let file = tokio::fs::File::open(&file).await?;
    let mut reader = BufReader::with_capacity(CHUNK, file);
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        sock.write_all(&chunk[..n]).await?;
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    Ok(())
}

/// Counts a transfer as "in flight" for as long as it runs, so the desktop
/// app can show whether something is currently being transferred.
pub struct ActiveGuard<'a>(&'a Config);

impl<'a> ActiveGuard<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        cfg.active_transfers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ActiveGuard(cfg)
    }
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0
            .active_transfers
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Counts an in-flight upload (separate from the generic transfer counter)
/// and releases the slot when the upload ends.
pub struct UploadGuard<'a>(&'a Config);

impl<'a> UploadGuard<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        UploadGuard(cfg)
    }
}

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.0
            .active_uploads
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Take `n` bytes from the run quota. Returns false when the quota is
/// exhausted (the transfer is then aborted instead of filling the disk).
fn take_quota(cfg: &Config, n: u64) -> bool {
    cfg.quota_left
        .fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |cur| if cur >= n { Some(cur - n) } else { None },
        )
        .is_ok()
}

/// Give the quota back (used when an aborted upload is cleaned up).
fn give_quota(cfg: &Config, n: u64) {
    cfg.quota_left
        .fetch_add(n, std::sync::atomic::Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// delete (file or whole subtree)

async fn delete(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    path: &std::path::Path,
) -> Result<(), std::io::Error> {
    let p = path.to_path_buf();
    if p.is_file() {
        if std::fs::remove_file(&p).is_ok() {
            cfg.invalidate_files();
            return write_json(sock, r#"{"success":true}"#).await;
        }
    } else if p.is_dir() {
        // Remove the whole tree: files first, then directories deepest-first.
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![p.clone()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        dirs.push(e.path());
                        stack.push(e.path());
                    } else {
                        files.push(e.path());
                    }
                }
            }
        }
        let _ = files.iter().map(|f| std::fs::remove_file(f)).count();
        let _ = dirs.into_iter().rev().map(|d| std::fs::remove_dir(d)).count();
        let _ = std::fs::remove_dir(&p);
        cfg.invalidate_files();
        return write_json(sock, r#"{"success":true}"#).await;
    }
    write_json_err(sock, "Not Found").await
}

// ---------------------------------------------------------------------------
// verify (client manifest cross-check after a batch / whole-folder upload)

/// `POST /verify` with `{"files":[{"path":"a/b.txt","size":123,"sha256":".."}]}`.
/// Per-file `ok | missing | size_mismatch | hash_mismatch`.
async fn verify(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    req: &Request,
    pending: &mut Vec<u8>,
) -> Result<(), std::io::Error> {
    let mut raw = Vec::with_capacity(req.content_length.min(1 << 20));
    while raw.len() < req.content_length {
        let take = (req.content_length - raw.len()).min(1 << 20);
        let mut chunk = vec![0u8; take];
        match read_body_exact(sock, pending, &mut chunk).await {
            Ok(_) => raw.extend_from_slice(&chunk),
            Err(e) => {
                eprintln!("/verify body read failed: {e}");
                break;
            }
        }
    }
    let arr = match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(v) if v.get("files").map(|a| a.is_array()).unwrap_or(false) => {
            v.get("files").unwrap().as_array().unwrap().clone()
        }
        _ => return write_json_err(sock, "Bad request").await,
    };
    let arr: Vec<serde_json::Value> = arr.iter().take(VERIFY_CAP).cloned().collect();

    // On-disk inventory, once per request.
    let dir = cfg.upload_dir();
    let mut on_disk: std::collections::HashMap<std::path::PathBuf, u64> =
        std::collections::HashMap::new();
    let mut cap = 0usize;
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(dir.clone(), 0)];
    while let Some((d, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH {
            break;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                // Skip symlinks: they must not be reachable through the API.
                match e.file_type() {
                    Ok(ft) if ft.is_symlink() => continue,
                    Err(_) => continue,
                    _ => {}
                }
                if e.path().is_dir() {
                    stack.push((e.path(), depth + 1));
                } else {
                    on_disk.insert(
                        e.path().to_path_buf(),
                        e.metadata().map(|m| m.len()).unwrap_or(0),
                    );
                    cap += 1;
                    if cap >= VERIFY_CAP {
                        break;
                    }
                }
            }
        }
        if cap >= VERIFY_CAP {
            break;
        }
    }

    let mut results = Vec::with_capacity(arr.len());
    let mut all_ok = true;
    for f in &arr {
        let item = check_one(cfg, f, &on_disk);
        if item != "ok" {
            all_ok = false;
        }
        let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
        results.push(format!(
            r#"{{"path":"{}","status":"{}"}}"#,
            Config::json_escape(path),
            item
        ));
    }
    let body = format!(
        r#"{{"success":true,"all_ok":{},"results":[{}]}}"#,
        all_ok,
        results.join(",")
    );
    write_json(sock, &body).await
}

fn check_one(
    cfg: &Arc<Config>,
    f: &serde_json::Value,
    on_disk: &std::collections::HashMap<std::path::PathBuf, u64>,
) -> String {
    let path = f.get("path").and_then(|v| v.as_str());
    let size = f.get("size").and_then(|v| v.as_u64());
    let hash = f
        .get("sha256")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());
    let (path, size) = match (path, size) {
        (Some(p), Some(s)) => (p, s),
        _ => return "bad_path".to_string(),
    };
    let rel = match safe_rel(path) {
        Some(r) => r,
        None => return "bad_path".to_string(),
    };
    let p = cfg.upload_dir().join(&rel);
    let disk_size = match on_disk.get(&p) {
        Some(&s) => s,
        None => return "missing".to_string(),
    };
    if disk_size != size {
        return "size_mismatch".to_string();
    }
    if let Some(expect) = hash {
        let file = match std::fs::File::open(&p) {
            Ok(f) => f,
            Err(_) => return "unreadable".to_string(),
        };
        let mut h = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        {
            use std::io::Read;
            let mut f = file;
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => h.update(&buf[..n]),
                    Err(_) => return "unreadable".to_string(),
                }
            }
        }
        let hex = hex_lower(&h.finalize());
        if hex != expect {
            return "hash_mismatch".to_string();
        }
    }
    "ok".to_string()
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// zip download of a subdirectory (streamed from a blocking thread)

/// `GET /zip/<dir>` — streams a whole subdirectory as a zip so the receiver
/// gets the folder structure in one download. Memory stays flat: each file is
/// read into 1 MiB chunks, compressed, and pushed to the socket.
async fn zip_dir(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    path: &std::path::Path,
) -> Result<(), std::io::Error> {
    use std::io::{Read as _Read, Write as _Write};

    let dir = path.to_path_buf();
    if !dir.is_dir() {
        return write_json_err(sock, "Not Found").await;
    }
    // The walk compares paths against the canonicalized root (Windows
    // canonical paths carry a `\\?\` prefix), so the directory we walk has
    // to be canonicalized as well — otherwise strip_prefix fails and every
    // entry ends up with an empty name.
    let dir = dir.canonicalize().unwrap_or(dir);
    // The zip crate needs a *seekable* output (it patches each entry's CRC
    // after the data), so build the archive in a temp file inside the
    // transfer dir, then stream it to the socket and delete it. Memory
    // stays flat; disk cost is one compressed copy while zipping.
    let root = cfg.upload_dir();
    // The walk starts at a canonicalized path (see `resolve_rel`), so the
    // root has to be canonicalized as well for `strip_prefix` to work
    // (on Windows canonical paths carry a `\\?\` prefix).
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    let rel = dir
        .strip_prefix(&root_canon)
        .ok()
        .map(|s| s.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let top = rel.split('/').next().unwrap_or("");
    let safe: String = top
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // Unique per request: two parallel zips must not share a temp file.
    let mut rnd = [0u8; 6];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut rnd);
    let suffix: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let tmp_name = if safe.is_empty() {
        format!(".files-{suffix}.zip.tmp")
    } else {
        format!(".{safe}-{suffix}.zip.tmp")
    };
    let tmp = root.join(tmp_name);

    let dir_b = dir.clone();
    let tmp_b = tmp.clone();
    let root_b = root_canon.clone();
    let build = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
        let mut file = std::fs::File::create(&tmp_b)?;
        let mut zip = zip::ZipWriter::new(&mut file);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut n = 0usize;
        let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(dir_b.clone(), 0)];
        while let Some((d, depth)) = stack.pop() {
            if depth > MAX_WALK_DEPTH {
                break;
            }
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    // Skip symlinks (they can point outside the dir or form
                    // cycles).
                    match e.file_type() {
                        Ok(ft) if ft.is_symlink() => continue,
                        Err(_) => continue,
                        _ => {}
                    }
                    if p.is_dir() {
                        // Skip dot-directories too (our own .trash recycle bin
                        // must not end up in the archive).
                        if p.file_name()
                            .map(|n| n.to_string_lossy().starts_with('.'))
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        stack.push((p, depth + 1));
                    } else {
                        // Skip dotfiles (incl. our own temp archives).
                        if p.file_name()
                            .map(|n| n.to_string_lossy().starts_with('.'))
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        let relname = p
                            .strip_prefix(&root_b)
                            .ok()
                            .map(|s| s.to_string_lossy().replace('\\', "/"));
                        let name = relname.unwrap_or_default();
                        if zip.start_file(name.as_str(), opts).is_err() {
                            continue;
                        }
                        let mut buf = vec![0u8; 1 << 20];
                        let mut fh = match std::fs::File::open(&p) {
                            Ok(f) => f,
                            Err(_) => continue,
                        };
                        let mut ok = true;
                        loop {
                            match fh.read(&mut buf) {
                                Ok(0) => break,
                                Ok(k) => {
                                    if zip.write_all(&buf[..k]).is_err() {
                                        ok = false;
                                        break;
                                    }
                                }
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if !ok {
                            break;
                        }
                        n += 1;
                    }
                }
            }
        }
        zip.finish()?;
        Ok(n)
    })
    .await;
    match build {
        Ok(Ok(_)) => {}
        _ => {
            let _ = std::fs::remove_file(&tmp);
            return write_json_err(sock, "zip build failed").await;
        }
    }

    let fname = rel.replace('/', ".");
    let zip_name = if fname.is_empty() {
        "files.zip".to_string()
    } else {
        format!("{fname}.zip")
    };
    let encoded = percent_encode(&zip_name);
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: application/zip\r\n");
    out.extend_from_slice(
        format!(
            "Content-Disposition: attachment; filename=\"{zip_name}\"; filename*=UTF-8''{encoded}\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    sock.write_all(&out).await?;

    // Stream the finished archive to the client, then clean up.
    let f = match tokio::fs::File::open(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let mut reader = BufReader::with_capacity(1 << 20, f);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(k) => {
                if sock.write_all(&chunk[..k]).await.is_err() {
                    break; // client left mid-transfer
                }
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    Ok(())
}
// ---------------------------------------------------------------------------
// url_decode

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &bytes[i + 1 .. i + 3];
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// percent_encode (RFC 3986, for filenames in Content-Disposition)

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// guess_mime (minimal extension-based MIME)

fn guess_mime(path: &std::path::Path) -> &str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js"  => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/avi",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "apk" => "application/vnd.android.package-archive",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "7z"  => "application/x-7z-compressed",
        "rar" => "application/x-rar-compressed",
        "tar" => "application/x-tar",
        "gz"  => "application/gzip",
        "exe" => "application/x-msdownload",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "yml" | "yaml" => "application/x-yaml",
        _ => "application/octet-stream",
    }
}
// ---------------------------------------------------------------------------
// upload (streaming multipart, constant memory)
// ---------------------------------------------------------------------------
// upload (streaming multipart, constant memory)
//
// Reads the socket in CHUNK-sized pieces and walks the multipart body with a
// small sliding window, streaming each part payload straight to disk so memory
// stays bounded regardless of file size.

async fn upload(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    boundary: &Option<String>,
    pending: &mut Vec<u8>,
) -> Result<(), std::io::Error> {
    let boundary = match boundary {
        Some(b) => b,
        None => return write_json_err(sock, "Missing boundary").await,
    };
    // The multipart delimiter is `--<boundary>`; the closing marker is
    // `--<boundary>--`.
    // We need to match either delimiter, and remember which we hit so we know
    // whether the body is finished.
    let delim = format!("--{boundary}").into_bytes(); // intermediate delimiter
    let close = format!("--{boundary}--").into_bytes(); // terminal delimiter
    // SIMD-accelerated substring search. The old `w.windows(n).position(...)`
    // compared a slice at *every* offset (~20 ns per byte) and was the
    // single biggest cost of an upload: 256 MB used ~5 s of CPU.
    let f_delim = memchr::memmem::Finder::new(&delim);
    let f_close = memchr::memmem::Finder::new(&close);

    let dir = cfg.upload_dir();
    std::fs::create_dir_all(&dir).ok();
    let _active = ActiveGuard::new(cfg);

    // Concurrency cap: a single device (or a script) must not be able to
    // saturate the disk for everybody else.
    if cfg.active_uploads.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        >= MAX_CONCURRENT_UPLOADS
    {
        cfg.active_uploads.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return write_json_status(
            sock,
            "503 Service Unavailable",
            "同时进行的传输过多，请稍后再试",
        )
        .await;
    }
    let _upload_guard = UploadGuard::new(cfg);

    // Sliding window: holds unread multipart data not yet written to disk.
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK * 2 + close.len() + 8);
    let mut buf = vec![0u8; CHUNK];

    // Which part are we in?
    //   0 = before the first part header / between parts
    //   1 = inside a part, streaming its body
    let mut in_part: bool = false;
    // `tokio::fs::File` so the disk writes go through the blocking pool
    // instead of stalling a runtime worker thread.
    let mut file: Option<tokio::io::BufWriter<tokio::fs::File>> = None;
    let mut wrote_any = false;
    let mut body_done = false; // set when we hit the terminal delimiter
    // Bytes written in this request (for the quota bookkeeping).
    let mut written: u64 = 0;
    // Set when the transfer dir has no room left any more.
    let mut quota_hit = false;
    // Files created by this connection (removed if the upload aborts).
    let mut created: Vec<std::path::PathBuf> = Vec::new();

    // Find the earliest delimiter position (and whether it is the terminal one)
    fn find_delim(
        w: &[u8],
        f_delim: &memchr::memmem::Finder<'_>,
        f_close: &memchr::memmem::Finder<'_>,
    ) -> Option<(usize, bool)> {
        let d = f_delim.find(w);
        let c = f_close.find(w);
        match (d, c) {
            (Some(dp), Some(cp)) if cp < dp => Some((cp, true)),
            (Some(dp), Some(cp)) => Some((dp, cp == dp)), // dp <= cp; if equal prefer close
            (Some(dp), None) => Some((dp, false)),
            (None, Some(cp)) => Some((cp, true)),
            _ => None,
        }
    }

    loop {
        if body_done || quota_hit {
            break;
        }
        let n = match read_body(sock, pending, &mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // Client aborted or reset: finalise what we have so far.
                eprintln!("upload read error: {e}");
                break;
            }
        };
        window.extend_from_slice(&buf[..n]);

        // Process the window: write out as much as we can.
        loop {
            if !in_part {
                // Looking for the end of a part header block: \r\n\r\n.
                match find_sub(&window, b"\r\n\r\n") {
                    Some(pos) => {
                        let headers = String::from_utf8_lossy(&window[..pos]);
                        let fname = extract_file_path(&headers);
                        in_part = true;
                        if let Some(fname) = fname {
                            // Hold the lock only across the existence check +
                            // create (a Send-able scope; the guard must not
                            // live across an await below).
                            let create_res: std::io::Result<(
                                std::fs::File,
                                std::path::PathBuf,
                            )> = {
                                let _guard = UPLOAD_LOCK.lock().ok();
                                create_unique_file(&dir, &fname)
                            };
                            match create_res {
                                Ok((f, p)) => {
                                    // 1 MiB write buffer: fewer blocking-pool
                                    // round trips per megabyte.
                                    file = Some(tokio::io::BufWriter::with_capacity(
                                        1 << 20,
                                        tokio::fs::File::from_std(f),
                                    ));
                                    created.push(p);
                                }
                                Err(e) => {
                                    return write_json_err(
                                        sock,
                                        &format!("create file failed: {e}"),
                                    )
                                    .await;
                                }
                            }
                        }
                        window.drain(..pos + 4);
                        // Stay in the same loop iteration to process body bytes
                        // already in the window.
                    }
                    None => {
                        // Not enough data yet; keep a bounded tail and wait for
                        // more.
                        let keep = CHUNK;
                        if window.len() > keep {
                            let cut = window.len() - keep;
                            window.drain(..cut);
                        }
                        break;
                    }
                }
            }

            // Now (or still) in a part body: stream until the next delimiter.
            if in_part {
                match find_delim(&window, &f_delim, &f_close) {
                    Some((pos, is_close)) => {
                        // The body of this part is window[0..pos] MINUS a
                        // trailing \r\n (the CRLF that precedes the delimiter).
                        let mut body_end = pos;
                        // In standard multipart the delimiter is preceded by
                        // \r\n which belongs to the framing, not the file.
                        if body_end >= 2 && &window[body_end - 2..body_end] == b"\r\n" {
                            body_end -= 2;
                        }
                        // If we only matched the intermediate `--boundary` and
                        // the window ends within 2 bytes of it, we cannot yet
                        // rule out that this is really the terminal
                        // `--boundary--` split across reads. Committing to it
                        // would leave us waiting for a next part that never
                        // arrives -> deadlock. Stream the confirmed data now
                        // (the CRLF before the candidate is framing and is
                        // dropped) and hold only the candidate itself.
                        let tentative = !is_close && pos + close.len() > window.len();
                        if tentative {
                            let payload = &window[..body_end];
                            if !payload.is_empty() {
                                if !take_quota(cfg, payload.len() as u64) {
                                    quota_hit = true;
                                    break;
                                }
                                written += payload.len() as u64;
                                if let Some(ref mut f) = file {
                                    tokio::io::AsyncWriteExt::write_all(f, payload).await?;
                                }
                                wrote_any = true;
                            }
                            window.drain(..pos);
                            break;
                        }
                        let payload = &window[..body_end];
                        if !payload.is_empty() {
                            if !take_quota(cfg, payload.len() as u64) {
                                quota_hit = true;
                                break;
                            }
                            written += payload.len() as u64;
                            if let Some(ref mut f) = file {
                                tokio::io::AsyncWriteExt::write_all(f, payload).await?;
                            }
                            wrote_any = true;
                        }
                        // Move past the delimiter (and its trailing \r\n if any).
                        let mut after = pos + delim.len();
                        if is_close {
                            after = pos + close.len();
                        }
                        // A delimiter is always followed by \r\n (body or close).
                        if window.len() >= after + 2 && &window[after..after + 2] == b"\r\n" {
                            after += 2;
                        }
                        window.drain(..after);
                        // Close the current part's file.
                        if let Some(mut f) = file.take() {
                            tokio::io::AsyncWriteExt::flush(&mut f).await?;
                        }
                        if is_close {
                            body_done = true;
                            in_part = false;
                            break;
                        }
                        // Next part: wait for its headers.
                        in_part = false;
                        // Loop to process any already-available header bytes.
                    }
                    None => {
                        // No delimiter yet. If we have a file open, stream the
                        // safe portion to disk but keep a tail long enough to
                        // still catch a delimiter split across reads.
                        let keep = close.len() + 4;
                        if file.is_some() && window.len() > keep {
                            let cut = window.len() - keep;
                            let payload = &window[..cut];
                            if !take_quota(cfg, payload.len() as u64) {
                                quota_hit = true;
                                break;
                            }
                            written += payload.len() as u64;
                            if let Some(ref mut f) = file {
                                tokio::io::AsyncWriteExt::write_all(f, payload).await?;
                            }
                            wrote_any = true;
                            window.drain(..cut);
                        }
                        break;
                    }
                }
            }
        }
    }

    // Finalise any open part.
    if let Some(mut f) = file.take() {
        let _ = tokio::io::AsyncWriteExt::flush(&mut f).await;
    }

    if quota_hit {
        // Out of space: drop what we just wrote and tell the client.
        for p in &created {
            let _ = std::fs::remove_file(p);
        }
        give_quota(cfg, written);
        return write_json_status(
            sock,
            "507 Insufficient Storage",
            "传输目录空间不足，已停止上传",
        )
        .await;
    }

    if !body_done {
        // Connection dropped/closed mid-body: remove the partial files we
        // created so a client retry starts clean.
        for p in &created {
            let _ = std::fs::remove_file(p);
        }
        give_quota(cfg, written);
        return write_json_err(sock, "Upload aborted").await;
    }

    if wrote_any {
        // The file list changed: drop the cached /files payload.
        cfg.invalidate_files();
        write_json(sock, r#"{"success":true}"#).await
    } else {
        write_json_err(sock, "No files").await
    }
}

// ---------------------------------------------------------------------------
// upload helpers

/// Returns the byte-offset of `needle` within `haystack`, if present.
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Pulls the file's *relative* path out of a part's `Content-Disposition`
/// header block. Parameter values keep their original casing (lowercasing
/// would corrupt mixed-case and percent-encoded names). For folder uploads
/// (client does `formData.append('file', f, relPath)`) the value is a
/// subdir path like `Photos/2026/a.jpg`; for plain uploads it is the bare
/// name. Untrusted input falls back to its last component only.
fn extract_file_path(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        if !name.trim().eq_ignore_ascii_case("content-disposition") {
            continue;
        }
        let value = value.trim();
        let lower = value.to_ascii_lowercase();
        // RFC 5987 `filename*=` takes precedence over `filename=`.
        if let Some(i) = lower.find("filename*=") {
            let rest = value[i + "filename*=".len()..].trim();
            if let Some(rfc) = rest.strip_prefix("UTF-8''") {
                let decoded = url_decode(rfc);
                return Some(match safe_rel(&decoded) {
                    Some(p) => p,
                    None => safe_leaf(&decoded),
                });
            }
            continue;
        }
        if let Some(i) = lower.find("filename=") {
            let rest = &value[i + "filename=".len()..];
            let fname = rest.trim().trim_matches('"').to_string();
            let decoded = url_decode(&fname);
            return Some(match safe_rel(&decoded) {
                Some(p) => p,
                None => safe_leaf(&decoded),
            });
        }
    }
    None
}

/// Last-resort file name guaranteed safe: final path component with
/// control characters and the Windows-illegal characters stripped, trailing
/// dots/spaces removed and reserved device names replaced.
fn safe_leaf(name: &str) -> String {
    let norm = name.replace('\\', "/");
    let last = norm.split('/').last().unwrap_or("upload");
    let c: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/'))
        .collect();
    let c = c.trim_end_matches(|c: char| c == '.' || c == ' ').to_string();
    if c.is_empty() || is_reserved_name(&c) {
        "upload".to_string()
    } else {
        c
    }
}

/// Create a unique file in `dir` with the given (relative) name, appending
/// `_1`, `_2`, ... if the name already exists. Parent directories are
/// created as needed. Returns (file, absolute path).
fn create_unique_file(
    dir: &std::path::Path,
    name: &str,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // The parent directory must still be inside the transfer dir: a
        // symlink/junction in there must not become an escape hatch.
        if let (Ok(p), Ok(b)) = (parent.canonicalize(), dir.canonicalize()) {
            if !p.starts_with(&b) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "path escapes the transfer directory",
                ));
            }
        }
    }
    if !path.exists() {
        return Ok((std::fs::File::create(&path)?, path));
    }
    let p = std::path::Path::new(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    // The fallback name lives in the *same* directory as the original
    // (the old code looked in the transfer root, so `docs/a.txt` collided
    // with a top-level `a_1.txt`).
    let parent = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dir.to_path_buf());
    let i = (1..)
        .find(|i| !parent.join(format!("{stem}_{i}{ext}")).exists())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "name conflict"))?;
    let target = parent.join(format!("{stem}_{i}{ext}"));
    Ok((std::fs::File::create(&target)?, target))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn run_server_once() -> (u16, Arc<Config>) {
        let dir = std::env::temp_dir().join(format!("transdata_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let cfg = Arc::new(Config::with_secret(TEST_SECRET.to_string(), TEST_CODE.to_string()));
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2, "127.0.0.1").await.ok();
        });
        (port, cfg)
    }

    /// Nothing but a session cookie (or the pairing secret for the local
    /// desktop app) may talk to the API. In particular the pairing code in
    /// the URL must not work any more.
    #[test]
    fn authorization_requires_a_session() {
        let cfg = Config::with_secret("secret".to_string(), "87654321".to_string());
        let mk = |extra: &str| {
            parse_request(&format!(
                "GET /files HTTP/1.1\r\nHost: t\r\n{extra}\r\n"
            ))
            .unwrap()
        };
        assert!(!authorized(&cfg, &mk("")));
        assert!(!authorized(&cfg, &mk("X-TD-Secret: wrong\r\n")));
        // The secret works (that is how the desktop app talks to itself).
        assert!(authorized(&cfg, &mk("X-TD-Secret: secret\r\n")));
        // A session cookie works.
        let sid = cfg.new_session();
        assert!(authorized(&cfg, &mk(&format!("Cookie: td_sid={sid}\r\n"))));
        // A pairing *code* is not a session credential.
        assert!(!authorized(&cfg, &mk("Cookie: td_sid=87654321\r\n")));
        // An unknown session id does not work.
        assert!(!authorized(&cfg, &mk("Cookie: td_sid=deadbeef\r\n")));
    }

    /// The URL must never carry a credential: `?t=` used to be accepted.
    #[test]
    fn query_parameters_do_not_authorize() {
        let cfg = Config::with_secret("secret".to_string(), "87654321".to_string());
        let req = parse_request(
            "GET /files?t=secret HTTP/1.1\r\nHost: t\r\n\r\n",
        )
        .unwrap();
        assert!(!authorized(&cfg, &req));
    }

    /// Cross-site requests must be rejected: a page on another origin
    /// cannot drive the API even if it somehow got hold of a session.
    #[test]
    fn cross_origin_requests_are_rejected() {
        let cfg = Config::with_secret("secret".to_string(), "87654321".to_string());
        let mk = |origin: &str| {
            parse_request(&format!(
                "POST /note HTTP/1.1\r\nHost: 192.168.1.9:9999\r\nOrigin: {origin}\r\n\r\n"
            ))
            .unwrap()
        };
        assert!(same_origin(&mk("http://192.168.1.9:9999"), &cfg));
        assert!(!same_origin(&mk("http://evil.example"), &cfg));
        // A different scheme is a different origin.
        assert!(!same_origin(&mk("https://192.168.1.9:9999"), &cfg));
        // With HTTPS switched on, only https is accepted.
        cfg.set_tls(true);
        assert!(same_origin(&mk("https://192.168.1.9:9999"), &cfg));
        assert!(!same_origin(&mk("http://192.168.1.9:9999"), &cfg));
        // No Origin header at all (not a browser) is allowed through.
        let no_origin = parse_request("POST /note HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
        assert!(same_origin(&no_origin, &cfg));
    }

    /// Path hardening: Windows-illegal characters, reserved device names
    /// and traversal must all be rejected.
    #[test]
    fn unsafe_paths_are_rejected() {
        assert!(safe_rel("docs/a.txt").is_some());
        assert!(safe_rel("a/b/c d.txt").is_some());
        for bad in [
            "../secret",
            "..\\secret",
            "/etc/passwd",
            "C:\\windows\\system32",
            "\\\\server\\share\\x",
            "a/../../b",
            "a//b",
            "a/./b",
            "CON",
            "con.txt",
            "NUL",
            "COM1",
            "aux.jpg",
            "a:b.txt",     // NTFS alternate data stream
            "a*b.txt",
            "a?b.txt",
            "a|b.txt",
            "a\"b.txt",
            "a<b.txt",
            "a>b.txt",
            "trailing.",   // Windows strips trailing dots
            "trailing ",
            "",
        ] {
            assert!(safe_rel(bad).is_none(), "must be rejected: {bad:?}");
        }
    }

    /// The pairing code is compared in constant time and a wrong code does
    /// not create a session.
    #[tokio::test]
    async fn wrong_pairing_code_is_rejected() {
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_pair_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2, "127.0.0.1").await.ok();
        });

        let body = r#"{"code":"00000000"}"#;
        let req = format!(
            "POST /pair HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = super::folder_tests::http_text(port, &req).await;
        assert!(resp.contains("401"), "resp: {resp}");
        assert_eq!(cfg.session_count(), 0, "no session may be created");
    }

    /// The right pairing code hands out a session cookie.
    #[tokio::test]
    async fn pairing_with_the_code_issues_a_session() {
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_pair2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg2.clone();
                tokio::spawn(async move { let _ = handle_conn(sock, &c, "127.0.0.1").await; });
            }
        });

        let body = format!(r#"{{"code":"{TEST_CODE}"}}"#);
        let req = format!(
            "POST /pair HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = super::folder_tests::http_text(port, &req).await;
        assert!(resp.contains("200 OK"), "resp: {resp}");
        assert!(resp.contains("Set-Cookie: td_sid="), "resp: {resp}");
        assert!(resp.contains("HttpOnly"), "resp: {resp}");
        assert!(resp.contains("SameSite=Strict"), "resp: {resp}");
        assert_eq!(cfg.session_count(), 1);
        // The declared length must match the body exactly (a wrong
        // Content-Length truncates the JSON and breaks the browser).
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        let declared: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(declared, body.len(), "Content-Length mismatch");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("valid JSON body");
        assert!(parsed["success"].as_bool().unwrap_or(false));

        // The index page of an unpaired device must not contain any secret.
        let page = super::folder_tests::http_text(port, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(!page.contains(TEST_SECRET), "the secret must never be in the page");
        assert!(!page.contains(TEST_CODE), "the code must never be in the page");
    }

    /// Without a session every API call is refused — including from a
    /// device on the same LAN. This is the hole the old design had.
    #[tokio::test]
    async fn api_requires_a_session() {
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_auth_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg2.clone();
                tokio::spawn(async move { let _ = handle_conn(sock, &c, "127.0.0.1").await; });
            }
        });

        for req in [
            "GET /files HTTP/1.1\r\nHost: t\r\n\r\n",
            "GET /info HTTP/1.1\r\nHost: t\r\n\r\n",
            "GET /note HTTP/1.1\r\nHost: t\r\n\r\n",
            "DELETE /delete/x HTTP/1.1\r\nHost: t\r\n\r\n",
            "GET /download/x HTTP/1.1\r\nHost: t\r\n\r\n",
        ] {
            let resp = super::folder_tests::http_text(port, req).await;
            assert!(
                resp.contains("401"),
                "must be refused without a session: {req:?} -> {resp}"
            );
        }
    }

    /// The page must come with a strict CSP and the usual hardening
    /// headers.
    #[tokio::test]
    async fn page_carries_security_headers() {
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_hdr_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2, "127.0.0.1").await.ok();
        });

        let resp = super::folder_tests::http_text(port, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        assert!(resp.contains("Content-Security-Policy:"), "resp: {resp}");
        assert!(resp.contains("script-src 'self'"), "resp: {resp}");
        assert!(resp.contains("X-Content-Type-Options: nosniff"), "resp: {resp}");
        assert!(resp.contains("X-Frame-Options: DENY"), "resp: {resp}");
        assert!(resp.contains("Referrer-Policy: no-referrer"), "resp: {resp}");
    }

    /// The websocket needs authorization too: the browser authenticates with
    /// the session cookie, the desktop app with the pairing secret.
    #[tokio::test]
    async fn websocket_requires_authorization() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_ws_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg2.clone();
                tokio::spawn(async move { let _ = handle_conn(sock, &c, "127.0.0.1").await; });
            }
        });

        // 1) without credentials the handshake must be refused
        let bad = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws")).await;
        assert!(bad.is_err(), "an unauthenticated websocket must be refused");

        // 2) with the pairing secret it works and the current note arrives
        let mut req = format!("ws://127.0.0.1:{port}/ws")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("X-TD-Secret", HeaderValue::from_str(TEST_SECRET).unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("no initial note state")
            .unwrap()
            .unwrap();
        let text = match msg {
            WsMsg::Text(t) => t,
            other => panic!("unexpected frame: {other:?}"),
        };
        assert!(text.contains("\"v\":"), "unexpected init frame: {text}");

        // 3) a note update is pushed to the connected client
        cfg.store_note("hello from the test");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("no note update")
            .unwrap()
            .unwrap();
        let text = match msg {
            WsMsg::Text(t) => t,
            other => panic!("unexpected frame: {other:?}"),
        };
        assert!(text.contains("hello from the test"), "got: {text}");
    }

    /// HTTPS mode: the very same API, but over TLS. A plain HTTP request
    /// to the TLS port gets a friendly "use https" page instead of a
    /// cryptic network error.
    #[tokio::test]
    async fn https_mode_serves_the_api() {
        use std::io::BufReader;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A certificate that is valid for 127.0.0.1.
        let material = crate::tls::load_or_create(&["127.0.0.1".to_string()])
            .expect("certificate");
        assert!(material.fingerprint.contains(':'), "fingerprint looks wrong");

        // Client that trusts exactly this certificate.
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut BufReader::new(material.cert_pem.as_bytes())) {
            roots.add(c.expect("cert")).expect("add cert");
        }
        let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let mut raw = Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        );
        raw.port = 19876;
        let cfg = Arc::new(raw);
        cfg.set_tls_material(material);
        cfg.set_tls(true);
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let _ = crate::http::server::serve(cfg2).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 1) plain HTTP on the TLS port -> the "please use https" page
        let plain = super::folder_tests::http_text(
            19876,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(plain.contains("HTTPS"), "expected a hint page, got: {plain}");
        assert!(plain.contains("https://"), "the page must show the https URL");
        assert!(plain.contains(":19876"), "the page must show the port");

        // 2) a real TLS request works and is still protected by the token
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", 19876)).await.unwrap();
        let name = rustls::pki_types::ServerName::IpAddress(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
        );
        let mut tls = connector.connect(name, tcp).await.expect("tls handshake");

        // Without a token: refused.
        tls.write_all(b"GET /files HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("401"), "unauthenticated TLS request: {text}");

        // With the secret: allowed.
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", 19876)).await.unwrap();
        let name = rustls::pki_types::ServerName::IpAddress(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).into(),
        );
        let mut tls = connector.connect(name, tcp).await.expect("tls handshake");
        tls.write_all(
            b"GET /info HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        loop {
            let n = tls.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("200 OK"), "got: {text}");
        assert!(text.contains("\"success\":true"), "got: {text}");
    }

    /// The whole browser flow in one go: pair -> get the app page -> upload
    /// with the session cookie -> list -> download -> delete.
    #[tokio::test]
    async fn end_to_end_browser_flow() {
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg2.clone();
                tokio::spawn(async move { let _ = handle_conn(sock, &c, "127.0.0.1").await; });
            }
        });

        // 1) pair
        let body = format!(r#"{{"code":"{TEST_CODE}"}}"#);
        let req = format!(
            "POST /pair HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = super::folder_tests::http_text(port, &req).await;
        assert!(resp.contains("Set-Cookie: td_sid="), "resp: {resp}");
        let sid = resp
            .split("Set-Cookie: td_sid=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let cookie = format!("Cookie: td_sid={sid}");

        // 2) the page is now the app (not the pairing screen)
        let page = super::folder_tests::http_text(
            port,
            &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}\r\n\r\n"),
        )
        .await;
        assert!(page.contains("data-mode=\"app\""), "page: {}", &page[..400.min(page.len())]);
        assert!(page.contains("Content-Security-Policy:"), "page: {page}");

        // 3) upload with the cookie
        let boundary = "E2";
        let data = b"hello from the browser";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\n\r\n{}\r\n--{boundary}--\r\n",
            String::from_utf8_lossy(data)
        );
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = super::folder_tests::http_text(port, &req).await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert_eq!(std::fs::read(dir.join("note.txt")).unwrap(), data);

        // 4) list
        let resp = super::folder_tests::http_text(
            port,
            &format!("GET /files HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}\r\n\r\n"),
        )
        .await;
        assert!(resp.contains("note.txt"), "resp: {resp}");

        // 5) download
        let resp = super::folder_tests::http_text(
            port,
            &format!("GET /download/note.txt HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}\r\n\r\n"),
        )
        .await;
        assert!(resp.contains("hello from the browser"), "resp: {resp}");

        // 6) delete
        let resp = super::folder_tests::http_text(
            port,
            &format!("DELETE /delete/note.txt HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}\r\n\r\n"),
        )
        .await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert!(!dir.join("note.txt").exists());
    }

    /// The upload quota is enforced: once it is used up the transfer stops
    /// instead of filling the disk.
    #[tokio::test]
    async fn upload_quota_is_enforced() {        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_quota_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir.clone());
        // 10 bytes of budget for a 1 KB file.
        cfg.quota_left
            .store(10, std::sync::atomic::Ordering::SeqCst);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2, "127.0.0.1").await.ok();
        });

        let boundary = "Qb";
        let data = vec![b'x'; 1024];
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\r\n"
        ) + &String::from_utf8_lossy(&data)
            + &format!("\r\n--{boundary}--\r\n");
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = super::folder_tests::http_text(port, &req).await;
        assert!(resp.contains("507"), "resp: {resp}");
        assert!(!dir.join("big.bin").exists(), "the file must be removed again");
    }

    /// Read from the client socket until EOF, returning the response body.
    async fn read_response(mut sock: tokio::net::TcpStream) -> Result<String, ()> {        let mut raw = Vec::new();
        let mut one = [0u8; 1];
        while sock.read(&mut one).await.map_err(|_| ())? > 0 {
            raw.push(one[0]);
        }
        let s = String::from_utf8_lossy(&raw);
        Ok(s.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
    }

    /// Upload one file, sending the body in two writes with a pause so the
    /// terminal delimiter straddles read boundaries.
    async fn upload_split_body(port: u16, boundary: &str, data: &[u8]) -> Result<String, String> {
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let mut client = client;
        let prefix = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut full = prefix.into_bytes();
        full.extend_from_slice(data);
        full.extend_from_slice(tail.as_bytes());
        // Keep the last 4 bytes so the 6-byte close marker `--Bb--` is
        // split across the two writes: the 1st write ends with `--Bb`
        // (which looks like an intermediate delimiter) and the 2nd write
        // supplies the trailing `--` that completes `--Bb--`.
        let split_point = full.len() - 4;
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            full.len()
        );
        client.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        client.write_all(&full[..split_point]).await.map_err(|e| e.to_string())?;
        client.flush().await.map_err(|e| e.to_string())?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await; // force split
        client.write_all(&full[split_point..]).await.map_err(|e| e.to_string())?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), read_response(client))
            .await
            .map_err(|_| "timeout: server did not respond (deadlock?)".to_string())?;
        resp.map_err(|_| "connection closed without response".to_string())
    }

    #[tokio::test]
    async fn upload_single_write_ok() {
        let (port, cfg) = run_server_once().await;
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut client = client;
        let boundary = "Xb";
        let data = b"hello world";
        let prefix = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n"
        );
        let mut full = prefix.into_bytes();
        full.extend_from_slice(data);
        full.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = format!(
            "POST /upload HTTP/1.0\r\nHost: 127.0.0.1\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            full.len()
        );
        client.write_all(req.as_bytes()).await.unwrap();
        client.write_all(&full).await.unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), read_response(client))
            .await
            .expect("deadlock: no response to simple upload");
        let resp = resp.unwrap();
        assert!(resp.contains("success\":true"), "unexpected response: {resp}");
        let saved = std::fs::read(cfg.upload_dir().join("a.txt")).unwrap();
        assert_eq!(saved, data);
    }

    #[tokio::test]
    async fn upload_split_closing_delim_no_deadlock() {
        let (port, _cfg) = run_server_once().await;
        let data: Vec<u8> = vec![b'z'; 1024];
        let resp = upload_split_body(port, "Bb", &data).await;
        let resp = resp.expect("server hung on split closing delimiter (deadlock)");
        assert!(resp.contains("success\":true"), "unexpected response: {resp}");
    }

    #[tokio::test]
    async fn upload_large_split_chunks() {
        // 700KB file fed in 64KB pieces; the terminal delimiter routinely
        // lands mid-stream under the 256KB window.
        let (port, _cfg) = run_server_once().await;
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut client = client;
        let boundary = "Lb";
        let data: Vec<u8> = (0..700 * 1024).map(|i| (i % 251) as u8).collect();
        let prefix = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let total = prefix.len() + data.len() + tail.len();
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {total}\r\n\r\n"
        );
        client.write_all(req.as_bytes()).await.unwrap();
        client.write_all(prefix.as_bytes()).await.unwrap();
        let mut i = 0;
        while i < data.len() {
            let j = (i + 64 * 1024).min(data.len());
            client.write_all(&data[i..j]).await.unwrap();
            client.flush().await.unwrap();
            i = j;
        }
        client.write_all(tail.as_bytes()).await.unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), read_response(client))
            .await
            .expect("deadlock on large file upload");
        let resp = resp.unwrap();
        assert!(resp.contains("success\":true"), "unexpected response: {resp}");
    }
}

#[cfg(test)]
mod multi_part_tests {
    use super::*;
    use crate::config::Config;

    /// Two-file multipart upload where the read boundary falls inside the
    /// *intermediate* delimiter (`--Bb` arrives, then `--\r\n<next headers>`
    /// arrives later). Both files must be saved intact.
    #[tokio::test]
    async fn upload_two_files_split_intermediate_delim() {
        let dir = std::env::temp_dir().join(format!("transdata_multi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        let cfg = Arc::new(Config::with_secret(super::TEST_SECRET.to_string(), super::TEST_CODE.to_string()));
        cfg.set_upload_dir(dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2, "127.0.0.1").await.ok();
        });

        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut client = client;
        let boundary = "Bb";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"one.txt\"\r\n\r\nONE\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"two.txt\"\r\n\r\nTWO\r\n--{boundary}--\r\n"
        );
        let full = body.as_bytes().to_vec();
        // Cut right after the first complete `--Bb` so the 1st write ends
        // exactly on the intermediate delimiter (tentative-hold scenario).
        let idx = full
            .windows(4)
            .position(|w| w == format!("--{boundary}").as_bytes())
            .unwrap();
        let split1 = idx + 4;
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            full.len()
        );
        client.write_all(req.as_bytes()).await.unwrap();
        client.write_all(&full[..split1]).await.unwrap();
        client.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        client.write_all(&full[split1..]).await.unwrap();
        let mut raw = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut one = [0u8; 1];
            while client.read(&mut one).await.unwrap() > 0 {
                raw.push(one[0]);
            }
        })
        .await
        .expect("deadlock on split intermediate delimiter");
        let resp = String::from_utf8_lossy(&raw);
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(
            resp_body.contains("\"success\":true"),
            "unexpected: {resp_body}"
        );
        assert_eq!(std::fs::read(dir.join("one.txt")).unwrap(), b"ONE");
        assert_eq!(std::fs::read(dir.join("two.txt")).unwrap(), b"TWO");
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    use crate::config::Config;

    /// A server that accepts any number of sequential connections.
    /// Each test must pass a unique `name` — parallel tests share the pid,
    /// and the temp dir is wiped at setup.
    async fn setup(name: &str, files: &[(&str, &[u8])]) -> (u16, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "transdata_folder_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, data) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&p, data).ok();
        }
        let cfg = Arc::new(Config::with_secret(super::TEST_SECRET.to_string(), super::TEST_CODE.to_string()));
        cfg.set_upload_dir(dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c2 = cfg.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, &c2, "127.0.0.1").await;
                });
            }
        });
        (port, dir)
    }

    pub(super) async fn http_bytes(port: u16, raw: &str) -> Vec<u8> {
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(raw.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        out
    }

    pub(super) async fn http_text(port: u16, raw: &str) -> String {
        String::from_utf8_lossy(&http_bytes(port, raw).await).to_string()
    }

    #[tokio::test]
    async fn subdir_upload_preserves_structure() {
        let (port, dir) = setup("empty", &[]).await;
        let boundary = "Fd";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"docs/sub/a.txt\"\r\n\r\nAAA\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"docs/sub/b.txt\"\r\n\r\nBBBB\r\n--{boundary}--\r\n"
        );
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let resp = http_text(port, &format!("{req}{body}")).await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert_eq!(std::fs::read(dir.join("docs/sub/a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(dir.join("docs/sub/b.txt")).unwrap(), b"BBBB");
    }

    #[tokio::test]
    async fn traversal_and_absolute_paths_fall_back_to_leaf() {
        let (port, dir) = setup("traversal", &[]).await;
        // percent-encoded ../ components -> rejected, leaf kept at the root
        let boundary = "T1";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"..%2f..%2fsecret.txt\"\r\n\r\nS1\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"/etc/passwd\"\r\n\r\nP\r\n--{boundary}--\r\n"
        );
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let resp = http_text(port, &format!("{req}{body}")).await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert_eq!(std::fs::read(dir.join("secret.txt")).unwrap(), b"S1");
        assert_eq!(std::fs::read(dir.join("passwd")).unwrap(), b"P");
        // Nothing may have been written outside the transfer dir:
        assert!(!dir.join("..secret").exists());
    }

    #[tokio::test]
    async fn aborted_upload_removes_partial_files() {
        let (port, dir) = setup("abort", &[]).await;
        let boundary = "Ab";
        // Header promises more body than we will actually send: close early.
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: 99999\r\n\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big/x.bin\"\r\n\r\nAAAA",
        );
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(req.as_bytes()).await.unwrap();
        drop(c);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(!dir.join("big/x.bin").exists(), "partial file must be removed");
    }

    #[tokio::test]
    async fn files_list_is_recursive() {
        let (port, _dir) = setup("recursive", &[("docs/sub/a.txt", b"AAA"), ("top.txt", b"T")]).await;
        let resp = http_text(port, "GET /files HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n").await;
        assert!(resp.contains("docs/sub/a.txt"), "resp: {resp}");
        assert!(resp.contains("top.txt"), "resp: {resp}");
    }

    #[tokio::test]
    async fn verify_reports_status() {
        let (port, _dir) = setup("verify", &[("docs/a.txt", b"AAA")]).await;
        let body =
            r#"{"files":[{"path":"docs/a.txt","size":3},{"path":"docs/missing.txt","size":5}]}"#;
        let req = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_text(port, &req).await;
        assert!(resp.contains("\"all_ok\":false"), "resp: {resp}");
        assert!(resp.contains("docs/a.txt\",\"status\":\"ok\""), "resp: {resp}");
        assert!(resp.contains("\"missing\""), "resp: {resp}");

        let body2 = r#"{"files":[{"path":"docs/a.txt","size":3}]}"#;
        let req2 = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body2.len(),
            body2
        );
        let resp2 = http_text(port, &req2).await;
        assert!(resp2.contains("\"all_ok\":true"), "resp2: {resp2}");

        // Wrong size -> size_mismatch
        let body3 = r#"{"files":[{"path":"docs/a.txt","size":999}]}"#;
        let req3 = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body3.len(),
            body3
        );
        let resp3 = http_text(port, &req3).await;
        assert!(resp3.contains("\"size_mismatch\""), "resp3: {resp3}");
    }

    #[tokio::test]
    async fn delete_removes_directory_tree() {
        let (port, dir) = setup("deltree", &[
            ("docs/sub/a.txt", b"A"),
            ("docs/sub/b.txt", b"B"),
            ("docs/c.txt", b"C"),
        ])
        .await;
        let resp = http_text(port, "DELETE /delete/docs HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n").await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert!(!dir.join("docs").exists(), "tree must be gone");
    }

    #[tokio::test]
    async fn zip_smoke_test() {
        let (port, dir) = setup("zip", &[("docs/a.txt", b"AAA"), ("docs/sub/b.txt", b"BBBB")]).await;
        let resp = http_bytes(port, "GET /zip/docs HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n").await;
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200"), "head: {}", &text[..text.len().min(60)]);
        assert!(resp.windows(4).any(|w| w == b"PK\x03\x04"), "no zip local header");
        assert!(text.contains("docs/a.txt"), "entry a missing");
        assert!(text.contains("docs/sub/b.txt"), "entry b missing");
        // temp archive cleaned up
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp zip not cleaned up");
    }
}
