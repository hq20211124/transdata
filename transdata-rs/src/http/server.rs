//! Minimal, high-concurrency async HTTP/1.1 server.
//!
//! Each connection is handled in its own tokio task, so a large file
//! transfer never blocks another device's requests. Uploads stream to disk
//! in fixed-size chunks (constant memory); downloads stream back from disk.

use std::io::ErrorKind;
use std::sync::Arc;

use sha2::{Digest, Sha256};
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
// 文件操作的实现在 `crate::ops`（GUI 和 HTTP 共用同一套规则与同一把锁）
use crate::ops::{
    give_quota, is_reserved_name, rel_display, resolve_rel, safe_rel, take_quota, UPLOAD_LOCK,
};

const MAX_HEAD: usize = 8 * 1024;
const CHUNK: usize = 256 * 1024;

/// Fixed pairing secret used by the unit tests (the real one is random per
/// run and lives in `Config::pair_secret`).
#[cfg(test)]
pub const TEST_SECRET: &str = "test-secret";
/// Fixed pairing code used by the unit tests.
#[cfg(test)]
pub const TEST_CODE: &str = "12345678";
/// /verify per-request cap (number of manifest entries). The client sends at
/// most 15 000 per batch, so this is only a backstop against a hand-made
/// request. It does *not* limit the number of files on disk any more.
const VERIFY_CAP: usize = 20_000;
/// How many uploads may run at the same time. Without this a single device
/// (or a script) can saturate the disk and starve every other client.
const MAX_CONCURRENT_UPLOADS: usize = 8;

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
        let expected = cfg.pair_secret.read().unwrap();
        if ct_eq(s, expected.as_str()) {
            return true;
        }
    }
    false
}

/// 这台请求方能不能「删除 / 修改」。
///
/// **前端置灰不算权限**，真正的判定在这里：
/// - `X-TD-Secret`（桌面端管理自己）：永远允许 —— 否则关掉某台设备的授权后
///   自己也删不动了；
/// - 会话 Cookie：按该会话所在设备的授权开关。
///
/// 注意上传 / 下载 / 打包**不看**这个开关 ——「关授权不等于断开」就是这条。
fn may_write(cfg: &Config, req: &Request) -> bool {
    if let Some(s) = &req.secret {
        let expected = cfg.pair_secret.read().unwrap();
        if ct_eq(s, expected.as_str()) {
            return true;
        }
    }
    if let Some(c) = &req.cookie {
        if let Some(sid) = cookie_value(c, "td_sid") {
            return cfg.session_can_write(sid);
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
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter(|f| f.path().is_file())
                        // 正在上传的临时文件不算
                        .filter(|f| {
                            !f.file_name().to_string_lossy().starts_with(".td-tmp-")
                        })
                        .count()
                })
                .unwrap_or(0);
            // The scheme has to follow the current mode: with HTTPS switched
            // on, an `http://` address here would send the phone to the
            // "please use https" page (the page copies this value).
            let scheme = if cfg.tls_on() { "https" } else { "http" };
            let url = format!("{scheme}://{}:{}", crate::net::display_ip(), cfg.port);
            let dir_s = dir.to_string_lossy().replace('"', "\\\"");
            let active = cfg
                .active_transfers
                .load(std::sync::atomic::Ordering::SeqCst);
            let quota = cfg.quota_left.load(std::sync::atomic::Ordering::SeqCst);
            let paired = cfg.session_count();
            // Seconds left on the pairing lockout (0 = not locked). The
            // desktop window shows this; it used to have no idea at all.
            let lock = cfg.pair_lock_left().unwrap_or(0);
            // 请求方自己那台设备还能不能「删除 / 修改」：网页据此把两个按钮
            // 置灰并挂黄条。桌面端（X-TD-Secret）永远是 true。
            let can_write = may_write(cfg, &req);
            let body = format!(
                r#"{{"success":true,"url":"{url}","files_count":{count},"save_dir":"{dir_s}","active":{active},"paired":{paired},"quota_left":{quota},"pair_locked":{lock},"can_write":{can_write}}}"#
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
            // 遍历与上限都在 `crate::ops::list` 里（GUI 用同一份，所以两边
            // 看到的是同一个目录）。网页端只要文件，目录行由前端自己拼。
            let snap = crate::ops::list(cfg);
            let items: Vec<String> = snap
                .files()
                .map(|e| {
                    format!(
                        r#"{{"name":"{}","size":{},"modified":{}}}"#,
                        Config::json_escape(&e.rel),
                        e.size,
                        e.mtime
                    )
                })
                .collect();
            let body = format!(
                r#"{{"success":true,"truncated":{},"files":[{}]}}"#,
                snap.truncated,
                items.join(",")
            );
            cfg.store_files_cache(body.clone());
            write_json(&mut sock, &body).await
        }
        // NOTE: `req.path` is *already* percent-decoded (see `parse_request`).
        // Decoding it a second time corrupted every name that happens to
        // contain `%` followed by two hex digits (`50%41.txt` -> `50A.txt`),
        // which made such files impossible to download or delete.
        ("GET", p) if p.starts_with("/download/") => {
            match resolve_rel(cfg, &p["/download/".len()..]) {
                Some(p) => download(&mut sock, cfg, &p, peer).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        ("DELETE", p) if p.starts_with("/delete/") => {
            if !may_write(cfg, &req) {
                return write_json_status(
                    &mut sock,
                    "403 Forbidden",
                    "这台设备已被禁止删除 / 修改",
                )
                .await;
            }
            delete(&mut sock, cfg, &p["/delete/".len()..], peer).await
        }
        ("POST", "/upload") => {
            upload(&mut sock, cfg, &req.boundary, &mut pending, peer).await
        }
        ("POST", "/rename") => {
            if !may_write(cfg, &req) {
                return write_json_status(
                    &mut sock,
                    "403 Forbidden",
                    "这台设备已被禁止删除 / 修改",
                )
                .await;
            }
            rename_path(&mut sock, cfg, &req, &mut pending, peer).await
        }
        ("POST", "/verify") => verify(&mut sock, cfg, &req, &mut pending).await,
        ("GET", p) if p.starts_with("/zip/") => {
            // `req.path` is already decoded — see the note above.
            match resolve_rel(cfg, &p["/zip/".len()..]) {
                Some(p) => zip_dir(&mut sock, cfg, &p, peer).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        // 打包整个传输目录（根目录下有多个文件夹/文件时也能一把全下）
        ("GET", "/zip-all") => {
            let root = cfg.upload_dir();
            zip_dir(&mut sock, cfg, &root, peer).await
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
    // Lockout: temporary and self-clearing. The remaining time is part of the
    // message so the page can tell the user *when* to retry instead of
    // "restart the program".
    if let Some(secs) = cfg.pair_lock_left() {
        return write_json_status(
            sock,
            "403 Forbidden",
            &format!("配对尝试次数过多，{secs} 秒后可重试"),
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
    let expected_code = cfg.pair_code();
    let expected_secret = cfg.pair_secret();
    let ok = (!given_code.is_empty() && ct_eq(given_code, &expected_code))
        || (!given_secret.is_empty() && ct_eq(given_secret, &expected_secret));

    if !ok {
        let fails = cfg.note_pair_failure();
        if fails >= crate::config::PAIR_MAX_FAILURES {
            // Tell the client when it may try again (see `pair_lock_left`).
            let secs = cfg.pair_lock_left().unwrap_or(0);
            return write_json_status(
                sock,
                "401 Unauthorized",
                &format!("配对码不正确，已锁定 {secs} 秒"),
            )
            .await;
        }
        return write_json_status(sock, "401 Unauthorized", "配对码不正确").await;
    }

    let sid = cfg.new_session(peer);
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

/// Frame/message limits for the note socket.
///
/// Without this the socket accepted messages of up to 64 MB (the library
/// default) and stored them as the shared note — the 4 MB cap that
/// `POST /note` enforces was trivially bypassed over WebSocket, and the
/// oversized text was then broadcast to every connected client.
fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    let max = crate::config::NOTE_MAX_BYTES + 64 * 1024;
    WebSocketConfig {
        max_message_size: Some(max),
        max_frame_size: Some(max),
        ..Default::default()
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
    let ws = match tokio_tungstenite::accept_async_with_config(headed, Some(ws_config())).await {
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
                        // Client-side "send" action. The same 4 MB cap as
                        // `POST /note`: a frame that big is refused instead of
                        // being stored and broadcast to everybody.
                        if t.len() <= crate::config::NOTE_MAX_BYTES {
                            let _ = cfg.store_note(&t);
                        }
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
                "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: private, max-age=0, must-revalidate\r\nVary: Accept-Encoding\r\n{}Connection: close\r\n\r\n",
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
    // The body depends on `Accept-Encoding` (plain vs gzip), so a cache must
    // key on it as well — otherwise a gzip body can be handed to a client
    // that never asked for one.
    out.extend_from_slice(b"Vary: Accept-Encoding\r\n");
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

// ---------------------------------------------------------------------------
// download (streamed from disk in chunks)

async fn download(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    path: &std::path::Path,
    peer: &str,
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
    // 记录用：传完才记账，所以这里先把展示用的相对路径准备好。
    let shown = rel_display(cfg, &file);
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
    let mut sent: u64 = 0;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        sock.write_all(&chunk[..n]).await?;
        sent += n as u64;
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    // 传完才记账：一条记录 = 一次真正送出去的下载，取消/断线的不留痕，
    // 大小是实际发出的字节数。浏览器那边也是这么算的（它只列完成的下载）。
    cfg.log_activity(peer, crate::config::ActKind::Download, &shown, sent);
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

// ---------------------------------------------------------------------------
// delete (file or whole subtree)

/// `DELETE /delete/<rel>` —— 薄壳：调 `ops::delete`，把结果写成 JSON。
/// 真正的并发规则（锁、配额归还、缓存失效、活动日志）在 `crate::ops` 里，
/// 和桌面端管理共用同一套。
async fn delete(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    rel: &str,
    peer: &str,
) -> Result<(), std::io::Error> {
    match crate::ops::delete(cfg, rel, peer) {
        Ok(_) => write_json(sock, r#"{"success":true}"#).await,
        Err(msg) => write_json_err(sock, &msg).await,
    }
}

// ---------------------------------------------------------------------------
// rename / move

/// `POST /rename` with `{"from":"a/b.txt","to":"a/c.txt"}`.
///
/// 重命名和移动是同一件事（同一块盘内的 rename 是原子操作）。规则在
/// `crate::ops::rename` 里，这里只负责读 body / 写响应。
async fn rename_path(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    req: &Request,
    pending: &mut Vec<u8>,
    peer: &str,
) -> Result<(), std::io::Error> {
    let len = req.content_length.min(8192);
    let mut raw = Vec::with_capacity(len);
    while raw.len() < len {
        let take = (len - raw.len()).min(4096);
        let mut chunk = vec![0u8; take];
        match read_body_exact(sock, pending, &mut chunk).await {
            Ok(_) => raw.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    let json: serde_json::Value =
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
    let from = json.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let to = json.get("to").and_then(|v| v.as_str()).unwrap_or("");
    match crate::ops::rename(cfg, from, to, peer) {
        Ok(()) => write_json(sock, r#"{"success":true}"#).await,
        Err(msg) => write_json_err(sock, &msg).await,
    }
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

    // Each entry is checked with a direct `stat` of its own path. The old
    // code built a full inventory of the transfer directory first and capped
    // that walk at `VERIFY_CAP`, so a directory with more files than the cap
    // reported every file beyond it as "missing" — a 30 000-file folder could
    // never pass verification no matter how often it was re-uploaded.
    let mut results = Vec::with_capacity(arr.len());
    let mut all_ok = true;
    for f in &arr {
        let item = check_one(cfg, f);
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

fn check_one(cfg: &Arc<Config>, f: &serde_json::Value) -> String {
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
    // `symlink_metadata` on purpose: a symlink must not be followed (it can
    // point outside the transfer directory) and counts as "missing", which is
    // exactly what the old inventory-based check reported.
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(_) => return "missing".to_string(),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return "missing".to_string();
    }
    if meta.len() != size {
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
// zip download of a subdirectory (streamed while it is being built)

/// `GET /zip/<dir>` — streams a whole subdirectory as a zip so the receiver
/// gets the folder structure in one download.
///
/// The archive is compressed **while it is being sent**: the browser gets
/// the first bytes within milliseconds (so it shows a real progress bar),
/// the client may walk away at any time (the build stops at the next chunk)
/// and the transfer directory never needs a second copy of the data on disk.
///
/// The zip format wants a seekable output (it patches the CRC and the sizes
/// into the local file header once the data is written). `new_stream` solves
/// that with a data descriptor, so the archive can be produced in one pass.
async fn zip_dir(
    sock: &mut impl Conn,
    cfg: &Arc<Config>,
    path: &std::path::Path,
    peer: &str,
) -> Result<(), std::io::Error> {
    let dir = path.to_path_buf();
    if !dir.is_dir() {
        return write_json_err(sock, "Not Found").await;
    }
    // The walk compares paths against the canonicalized root (Windows
    // canonical paths carry a `\\?\` prefix), so the directory we walk has
    // to be canonicalized as well — otherwise strip_prefix fails and every
    // entry ends up with an empty name.
    let dir = dir.canonicalize().unwrap_or(dir);
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
    // The size is not known in advance (the archive is still being built),
    // so the body is sent with chunked transfer encoding.
    out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
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
    sock.flush().await?;

    let _active = ActiveGuard::new(cfg);

    // The builder runs on a blocking thread and hands the archive over in
    // chunks. The channel is bounded: a client that stops reading slows the
    // builder down instead of filling up the memory.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);

    let dir_b = dir.clone();
    let root_b = root_canon.clone();
    let build = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut zip = zip::ZipWriter::new_stream(ChunkWriter::new(tx));
        match build_entries(&mut zip, &dir_b, &root_b) {
            Ok(()) => {
                // Writes the central directory, then closes the chunked body.
                let mut w = zip
                    .finish()
                    .map_err(|e| std::io::Error::other(e.to_string()))?
                    .into_inner();
                w.finish()
            }
            Err(e) => {
                // Close the writer instead of dropping it: the crate would
                // otherwise try to write a central directory into a socket
                // that is already gone — and complain about it on stderr.
                let _ = zip.finish();
                Err(e)
            }
        }
    });

    // Pump the archive to the client. If the client disappears the write
    // fails, we drop the receiver and the builder stops at the next chunk.
    let mut client_gone = false;
    let mut sent: u64 = 0;
    while let Some(buf) = rx.recv().await {
        if sock.write_all(&buf).await.is_err() {
            client_gone = true;
            break;
        }
        sent += buf.len() as u64;
    }
    // Must happen before awaiting the builder: a builder that is waiting for
    // room in the channel only notices the dead client when the receiver is
    // gone.
    drop(rx);
    let built = build.await;

    if client_gone {
        // The client is not there anymore — nothing left to do.
        return Ok(());
    }
    if built.is_err() {
        // No terminating chunk: the client sees a truncated download and
        // reports it instead of saving a broken archive.
        let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
        return Ok(());
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(sock).await;
    // 打包也是传完才记，大小是实际发出的 zip 字节数
    cfg.log_activity(
        peer,
        crate::config::ActKind::Download,
        &format!("{zip_name}（打包）"),
        sent,
    );
    Ok(())
}

/// "The client hung up" — the builder uses it to stop early.
fn broken_pipe() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client hung up")
}

/// Add every file below `dir` to the archive.
///
/// `root` is the transfer directory: the names inside the archive are
/// relative to it, so the folder structure survives the round trip.
fn build_entries<W: std::io::Write>(
    zip: &mut zip::ZipWriter<zip::write::StreamWriter<W>>,
    dir: &std::path::Path,
    root: &std::path::Path,
) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};

    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        if depth > crate::ops::MAX_WALK_DEPTH {
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
                    continue;
                }
                // Skip dotfiles.
                if p.file_name()
                    .map(|n| n.to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
                {
                    continue;
                }
                let relname = p
                    .strip_prefix(root)
                    .ok()
                    .map(|s| s.to_string_lossy().replace('\\', "/"));
                let name = relname.unwrap_or_default();
                if zip.start_file(name.as_str(), opts).is_err() {
                    return Err(broken_pipe());
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
                    // The client is gone (or the disk failed): stop the
                    // build instead of compressing a package nobody is
                    // waiting for.
                    return Err(broken_pipe());
                }
            }
        }
    }
    Ok(())
}

/// How much of the archive is buffered before it is handed to the socket.
const ZIP_CHUNK: usize = 256 * 1024;

/// A [`std::io::Write`] that frames the archive as HTTP chunked-transfer
/// encoding and hands the pieces to the async side through a channel.
///
/// The channel is bounded, so a client that stops reading throttles the
/// builder instead of letting it buffer the whole archive in memory.
struct ChunkWriter {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    buf: Vec<u8>,
    broken: bool,
}

impl ChunkWriter {
    fn new(tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        ChunkWriter {
            tx,
            buf: Vec::with_capacity(ZIP_CHUNK + 64),
            broken: false,
        }
    }

    /// Send what is left and close the chunked body.
    fn finish(&mut self) -> std::io::Result<()> {
        self.flush_chunk()?;
        self.tx
            .blocking_send(b"0\r\n\r\n".to_vec())
            .map_err(|_| broken_pipe())
    }

    fn flush_chunk(&mut self) -> std::io::Result<()> {
        if self.broken {
            return Err(broken_pipe());
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut out = Vec::with_capacity(self.buf.len() + 32);
        out.extend_from_slice(format!("{:x}\r\n", self.buf.len()).as_bytes());
        out.extend_from_slice(&self.buf);
        out.extend_from_slice(b"\r\n");
        self.buf.clear();
        if self.tx.blocking_send(out).is_err() {
            // The receiver is gone: the client hung up.
            self.broken = true;
            return Err(broken_pipe());
        }
        Ok(())
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.broken {
            return Err(broken_pipe());
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= ZIP_CHUNK {
            self.flush_chunk()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_chunk()
    }
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
    peer: &str,
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
    // 配额不是启动时的一次性快照：先按当前剩余空间收一次（只收缩不放大）。
    cfg.clamp_quota_to_disk();
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
    // 正在写的这一部分的临时文件 + 最终名字（传完才改名）
    let mut open_part: Option<(std::path::PathBuf, String)> = None;
    // 所有开过的临时文件：中途失败要一起删掉
    let mut temps: Vec<std::path::PathBuf> = Vec::new();
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
                            // 先写临时文件（同目录），传完再改名成最终名字
                            match create_temp_file(&dir, &fname) {
                                Ok((f, tmp)) => {
                                    // 1 MiB write buffer: fewer blocking-pool
                                    // round trips per megabyte.
                                    file = Some(tokio::io::BufWriter::with_capacity(
                                        1 << 20,
                                        tokio::fs::File::from_std(f),
                                    ));
                                    temps.push(tmp.clone());
                                    open_part = Some((tmp, fname));
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
                        // Close the current part's file, then publish it:
                        // the temp file only becomes visible under its real
                        // name once it is complete.
                        if let Some(mut f) = file.take() {
                            tokio::io::AsyncWriteExt::flush(&mut f).await?;
                            drop(f);
                        }
                        if let Some((tmp, fname)) = open_part.take() {
                            match publish_temp(&dir, &tmp, &fname) {
                                Ok(done) => created.push(done),
                                Err(e) => {
                                    let _ = std::fs::remove_file(&tmp);
                                    return write_json_err(
                                        sock,
                                        &format!("finalise file failed: {e}"),
                                    )
                                    .await;
                                }
                            }
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
        for p in &temps {
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
        for p in &temps {
            let _ = std::fs::remove_file(p);
        }
        give_quota(cfg, written);
        return write_json_err(sock, "Upload aborted").await;
    }

    // 上传成功：每个落盘的文件都记一笔（大小用真实字节数）
    for p in &created {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let rel = rel_display(cfg, p);
        cfg.log_activity(peer, crate::config::ActKind::Upload, &rel, size);
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

/// A path for `name` inside `dir` that does not exist yet (`a.txt` →
/// `a_1.txt`, `a_2.txt` …). Parent directories are created as needed and
/// must stay inside the transfer directory.
fn unique_path(
    dir: &std::path::Path,
    name: &str,
) -> std::io::Result<std::path::PathBuf> {
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
        return Ok(path);
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
    Ok(parent.join(format!("{stem}_{i}{ext}")))
}

/// 上传先写同目录下的临时文件 `.td-tmp-<rand>.part`，传完再改名成最终
/// 名字 —— 这样别人（列表 / 下载 / 打包）永远看不到「半个文件」。
fn create_temp_file(
    dir: &std::path::Path,
    name: &str,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    let target = dir.join(name);
    let parent = target
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dir.to_path_buf());
    std::fs::create_dir_all(&parent)?;
    if let (Ok(p), Ok(b)) = (parent.canonicalize(), dir.canonicalize()) {
        if !p.starts_with(&b) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes the transfer directory",
            ));
        }
    }
    let mut bytes = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let tag: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = parent.join(format!(".td-tmp-{tag}.part"));
    Ok((std::fs::File::create(&tmp)?, tmp))
}

/// 临时文件改名成最终名字。同一目录内的 rename 是原子的：别人要么看不到
/// 这个文件，要么看到的就是完整内容。整个过程在 UPLOAD_LOCK 里，重名
/// 检查和改名之间不会被另一个上传插进来。
fn publish_temp(
    dir: &std::path::Path,
    tmp: &std::path::Path,
    name: &str,
) -> std::io::Result<std::path::PathBuf> {
    let _guard = UPLOAD_LOCK.lock().ok();
    let final_path = unique_path(dir, name)?;
    std::fs::rename(tmp, &final_path)?;
    Ok(final_path)
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
        let sid = cfg.new_session("127.0.0.1:5555");
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

        // A certificate that is valid for 127.0.0.1. Written to a temp dir so
        // the test neither pollutes `target/` nor picks up a stale cert.
        let cert_dir = std::env::temp_dir().join(format!("transdata_tls_{}", std::process::id()));
        std::fs::create_dir_all(&cert_dir).ok();
        let material = crate::tls::load_or_create_in(
            &["127.0.0.1".to_string()],
            std::slice::from_ref(&cert_dir),
        )
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
        // 端口由系统分配：硬编码端口在并行测试或本机刚好占用时会 flaky。
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        raw.port = port;
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
            port,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(plain.contains("HTTPS"), "expected a hint page, got: {plain}");
        assert!(plain.contains("https://"), "the page must show the https URL");
        assert!(plain.contains(&format!(":{port}")), "the page must show the port");

        // 2) a real TLS request works and is still protected by the token
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
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
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
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
        // A2：HTTPS 模式下 /info 必须报 https 地址，否则手机上「复制地址」
        // 复制到的是一个打不开的 http 链接。
        assert!(
            text.contains("\"url\":\"https://"),
            "HTTPS 模式下 /info 应该给 https 地址: {text}"
        );
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
        // 上传要记进「设备操作记录」
        let acts = cfg.recent_activity(1);
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].kind, crate::config::ActKind::Upload);
        assert_eq!(acts[0].name, "a.txt");
        assert_eq!(acts[0].size, data.len() as u64);
        assert_eq!(acts[0].ip, "127.0.0.1", "端口要从 IP 里去掉");
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
    pub(super) async fn setup(name: &str, files: &[(&str, &[u8])]) -> (u16, std::path::PathBuf) {
        let (port, dir, _cfg) = setup_cfg(name, files).await;
        (port, dir)
    }

    /// 同 `setup`，但把 `Config` 也交出来：需要检查操作记录的用例用这个。
    pub(super) async fn setup_cfg(
        name: &str,
        files: &[(&str, &[u8])],
    ) -> (u16, std::path::PathBuf, Arc<Config>) {
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
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c2 = cfg2.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, &c2, "127.0.0.1").await;
                });
            }
        });
        (port, dir, cfg)
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

    /// 重命名：能改名/移动，撞名拒绝且不覆盖，越界路径拒绝。
    #[tokio::test]
    async fn rename_moves_and_refuses_overwrite() {
        let (port, dir) = setup("rename", &[("a.txt", b"AAA"), ("b.txt", b"BBB")]).await;
        let post = |body: String| {
            let req = format!(
                "POST /rename HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            async move { http_text(port, &req).await }
        };
        // 改名 + 顺手移动到子目录（子目录会自动建）
        let resp = post(r#"{"from":"a.txt","to":"docs/c.txt"}"#.to_string()).await;
        assert!(resp.contains("\"success\":true"), "{resp}");
        assert!(!dir.join("a.txt").exists(), "原名应该没了");
        assert_eq!(std::fs::read(dir.join("docs/c.txt")).unwrap(), b"AAA");

        // 撞名：拒绝，两边都不能少
        let resp = post(r#"{"from":"b.txt","to":"docs/c.txt"}"#.to_string()).await;
        assert!(resp.contains("同名文件已存在"), "{resp}");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"BBB");
        assert_eq!(std::fs::read(dir.join("docs/c.txt")).unwrap(), b"AAA");

        // 越界：拒绝，原文件还在
        let resp = post(r#"{"from":"b.txt","to":"../escape.txt"}"#.to_string()).await;
        assert!(!resp.contains("\"success\":true"), "{resp}");
        assert!(dir.join("b.txt").exists(), "越界改名不能动原文件");

        // 原文件不存在
        let resp = post(r#"{"from":"nope.txt","to":"x.txt"}"#.to_string()).await;
        assert!(resp.contains("找不到原文件"), "{resp}");
    }

    /// 起一个能用指定 Config 服务的端口（会话由测试直接造，便于模拟不同 IP）。
    async fn serve_cfg(cfg: Arc<Config>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, &c, "127.0.0.1").await;
                });
            }
        });
        port
    }

    fn device_test_cfg(name: &str) -> (Arc<Config>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "transdata_dev_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("a.txt"), b"AAAA").ok();
        std::fs::write(dir.join("b.txt"), b"BBBB").ok();
        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        cfg.set_upload_dir(dir.clone());
        (cfg, dir)
    }

    /// 按设备授权：A 关掉「删除 / 修改」后，A 的 Cookie 删不动，
    /// B 的 Cookie 照常，桌面端（X-TD-Secret）也照常。`/info` 各自正确。
    #[tokio::test]
    async fn device_write_flag_is_per_device() {
        let (cfg, dir) = device_test_cfg("perdevice");
        let sid_a = cfg.new_session("192.168.1.7:5000");
        let sid_b = cfg.new_session("192.168.1.8:6000");
        cfg.set_device_write("192.168.1.7", false);
        let port = serve_cfg(cfg.clone()).await;

        let del = |cookie: &str| {
            format!("DELETE /delete/a.txt HTTP/1.1\r\nHost: t\r\nCookie: td_sid={cookie}\r\n\r\n")
        };
        // A：403，文件必须还在
        let resp = http_text(port, &del(&sid_a)).await;
        assert!(resp.contains("403"), "resp: {resp}");
        assert!(resp.contains("已被禁止删除"), "resp: {resp}");
        assert!(dir.join("a.txt").exists(), "被禁止的设备不能真的删掉文件");

        // /info：各看各的
        let info = |cookie: &str| {
            format!("GET /info HTTP/1.1\r\nHost: t\r\nCookie: td_sid={cookie}\r\n\r\n")
        };
        assert!(http_text(port, &info(&sid_a)).await.contains("\"can_write\":false"));
        assert!(http_text(port, &info(&sid_b)).await.contains("\"can_write\":true"));

        // B：200
        let resp = http_text(port, &del(&sid_b)).await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert!(!dir.join("a.txt").exists());

        // 桌面端豁免：关掉授权后自己还得能删
        let resp = http_text(
            port,
            "DELETE /delete/b.txt HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert!(!dir.join("b.txt").exists());
    }

    /// 「关授权不等于断开」：上传 / 下载 / 打包一律不看那个开关。
    #[tokio::test]
    async fn write_flag_does_not_block_transfer() {
        let (cfg, dir) = device_test_cfg("transfer");
        let sid_a = cfg.new_session("192.168.1.7:5000");
        cfg.set_device_write("192.168.1.7", false);
        let port = serve_cfg(cfg).await;

        // 上传
        let boundary = "Fb";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"up.txt\"\r\n\r\nhello\r\n--{boundary}--\r\n"
        );
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nCookie: td_sid={sid_a}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let resp = http_text(port, &req).await;
        assert!(resp.contains("\"success\":true"), "上传不受授权影响: {resp}");
        assert_eq!(std::fs::read(dir.join("up.txt")).unwrap(), b"hello");

        // 下载
        let resp = http_text(
            port,
            &format!("GET /download/a.txt HTTP/1.1\r\nHost: t\r\nCookie: td_sid={sid_a}\r\n\r\n"),
        )
        .await;
        assert!(resp.contains("200 OK"), "下载不受授权影响: {resp}");
        assert!(resp.contains("AAAA"), "{resp}");

        // 打包
        let resp = http_text(
            port,
            &format!("GET /zip-all HTTP/1.1\r\nHost: t\r\nCookie: td_sid={sid_a}\r\n\r\n"),
        )
        .await;
        assert!(resp.contains("200 OK"), "打包不受授权影响: {resp}");
    }

    /// 上传成功后目录里只能有成品，不能留 `.td-tmp-*` 中间文件。
    #[tokio::test]
    async fn upload_leaves_no_temp_files() {
        let (port, dir) = setup("tmpclean", &[]).await;
        let boundary = "Tb";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"docs/a.txt\"\r\n\r\nhello\r\n--{boundary}--\r\n"
        );
        let req = format!(
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let resp = http_text(port, &format!("{req}{body}")).await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert_eq!(std::fs::read(dir.join("docs/a.txt")).unwrap(), b"hello");
        let leftovers: Vec<String> = std::fs::read_dir(dir.join("docs"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".td-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件没清掉: {leftovers:?}");
    }

    /// 删除要把配额还回来，否则删完还是传不进新东西。
    #[tokio::test]
    async fn delete_returns_quota() {
        let (port, _dir) = setup("quota-back", &[("a.txt", b"0123456789")]).await;
        fn quota_of(r: &str) -> u64 {
            r.split("\"quota_left\":")
                .nth(1)
                .and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
                .unwrap_or(0)
        }
        let info = "GET /info HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n";
        let before = quota_of(&http_text(port, info).await);
        let resp = http_text(
            port,
            "DELETE /delete/a.txt HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await;
        assert!(resp.contains("\"success\":true"), "{resp}");
        let after = quota_of(&http_text(port, info).await);
        assert!(after >= before + 10, "配额没还回来: before={before} after={after}");
    }

    #[tokio::test]
    async fn files_list_is_recursive() {
        let (port, _dir) = setup("recursive", &[("docs/sub/a.txt", b"AAA"), ("top.txt", b"T")]).await;
        let resp = http_text(port, "GET /files HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n").await;
        assert!(resp.contains("docs/sub/a.txt"), "resp: {resp}");
        assert!(resp.contains("top.txt"), "resp: {resp}");
    }

    /// `/files` 现在是 `ops::list` 的薄壳：文件条目数 / 名字 / 大小必须和
    /// 管理端看到的完全一致（否则网页端和桌面端会各说各话）。
    #[tokio::test]
    async fn ops_list_matches_files_endpoint() {
        let (port, dir) = setup(
            "opsmatch",
            &[("docs/sub/a.txt", b"AAA"), ("top.txt", b"T"), ("docs/b.bin", b"12345")],
        )
        .await;
        let resp =
            http_text(port, "GET /files HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n")
                .await;
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        let mut from_http: Vec<(String, u64)> = json["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().to_string(),
                    f["size"].as_u64().unwrap(),
                )
            })
            .collect();
        from_http.sort();

        let cfg = Arc::new(Config::with_secret(TEST_SECRET.to_string(), TEST_CODE.to_string()));
        cfg.set_upload_dir(dir);
        let snap = crate::ops::list(&cfg);
        let mut from_ops: Vec<(String, u64)> =
            snap.files().map(|e| (e.rel.clone(), e.size)).collect();
        from_ops.sort();

        assert_eq!(from_http, from_ops);
        // 目录条目只在 ops 里（GUI 拼树用），网页端不该看到
        assert!(snap.entries.iter().any(|e| e.is_dir && e.rel == "docs"));
        assert!(!from_http.iter().any(|(n, _)| n == "docs"));
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

    /// Split an HTTP response into head and body.
    fn split_response(resp: &[u8]) -> (String, Vec<u8>) {
        let pos = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("no head terminator");
        (
            String::from_utf8_lossy(&resp[..pos]).to_string(),
            resp[pos + 4..].to_vec(),
        )
    }

    /// Decode an HTTP chunked-transfer body.
    fn dechunk(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < body.len() {
            let nl = body[i..]
                .iter()
                .position(|&b| b == b'\n')
                .expect("truncated chunk header")
                + i;
            let line = String::from_utf8_lossy(&body[i..nl]);
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap().trim(), 16)
                .expect("bad chunk size");
            i = nl + 1;
            if size == 0 {
                break;
            }
            assert!(i + size <= body.len(), "truncated chunk body");
            out.extend_from_slice(&body[i..i + size]);
            i += size + 2; // the CRLF after the chunk
        }
        out
    }

    #[tokio::test]
    async fn zip_smoke_test() {
        let (port, dir) = setup("zip", &[("docs/a.txt", b"AAA"), ("docs/sub/b.txt", b"BBBB")]).await;
        let resp = http_bytes(port, "GET /zip/docs HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n").await;
        let (head, body) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(head.contains("Transfer-Encoding: chunked"), "head: {head}");
        assert!(head.contains("filename=\"docs.zip\""), "head: {head}");

        // The body must be a real, readable zip archive.
        let zip = dechunk(&body);
        let mut ar = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("not a valid zip");
        let read_entry = |ar: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str| {
            use std::io::Read as _;
            let mut f = ar.by_name(name).expect("entry missing");
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            s
        };
        assert_eq!(read_entry(&mut ar, "docs/a.txt"), "AAA");
        assert_eq!(read_entry(&mut ar, "docs/sub/b.txt"), "BBBB");

        // The archive is streamed, so nothing is ever written to disk.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp zip not cleaned up");
    }

    /// The browser tab is closed while the archive is still being built:
    /// the server must stop compressing instead of building a package
    /// nobody is waiting for.
    #[tokio::test]
    async fn zip_client_disconnect_stops_the_build() {
        // 48 MB of incompressible data: the deflate takes long enough that
        // the client is gone before the archive is finished.
        let mut big = vec![0u8; 48 * 1024 * 1024];
        let mut seed = 0x1234_5678u32;
        for b in big.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (seed >> 24) as u8;
        }
        let (port, dir) = setup("zipabort", &[("docs/big.bin", &big)]).await;
        drop(big);

        // Send the request and hang up immediately (the browser tab is
        // closed while the download is still pending).
        {
            let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            c.write_all(b"GET /zip/docs HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n")
                .await
                .unwrap();
            c.flush().await.unwrap();
            drop(c);
        }

        // Nothing may be left behind, and the server has to keep working.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temp zip survived the disconnect: {:?}",
                leftovers
                    .iter()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            );
        }

        // The server is still alive and serving other requests.
        let resp = http_text(port, "GET /files HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n")
            .await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
    }
}

// ---------------------------------------------------------------------------
// 回归测试：2026-10-28 review 修掉的缺陷
// ---------------------------------------------------------------------------

#[cfg(test)]
mod review_fix_tests {
    use super::folder_tests::{http_bytes, http_text, setup, setup_cfg};
    use super::*;
    use crate::config::Config;

    /// A3：文件名里的 `%41` 不能被当成转义**再解一次**。
    ///
    /// 客户端把 `50%41.txt` 编码成 `50%2541.txt`；服务端解码一次得到
    /// `50%41.txt`（正确）。以前 handler 又解了一次，变成 `50A.txt`，
    /// 于是这个文件能列出来、却下不了、删不掉、也打不进 zip。
    #[tokio::test]
    async fn percent_in_filename_roundtrips() {
        let (port, dir) = setup("percent_name", &[("50%41.txt", b"hello")]).await;
        let auth = "X-TD-Secret: test-secret\r\n";

        let list = http_text(
            port,
            &format!("GET /files HTTP/1.1\r\nHost: t\r\n{auth}\r\n"),
        )
        .await;
        assert!(list.contains("50%41.txt"), "列表里应该有它: {list}");

        let dl = http_text(
            port,
            &format!("GET /download/50%2541.txt HTTP/1.1\r\nHost: t\r\n{auth}\r\n"),
        )
        .await;
        assert!(dl.starts_with("HTTP/1.1 200 OK"), "下载应该成功: {dl}");
        assert!(dl.ends_with("hello"), "内容应该原样返回: {dl}");

        let del = http_text(
            port,
            &format!("DELETE /delete/50%2541.txt HTTP/1.1\r\nHost: t\r\n{auth}\r\n"),
        )
        .await;
        assert!(del.contains("\"success\":true"), "删除应该成功: {del}");
        assert!(!dir.join("50%41.txt").exists(), "文件应该真的没了");
    }

    /// A3 边界：编码过的斜杠解码后就是子目录分隔符，不能变成穿越。
    #[tokio::test]
    async fn encoded_slash_cannot_escape() {
        let (port, dir) = setup("enc_slash", &[]).await;
        std::fs::write(dir.join("secret.txt"), b"top").ok();
        let resp = http_text(
            port,
            "GET /download/..%2F..%2Fsecret.txt HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("404") || resp.contains("400"),
            "穿越必须被拒: {resp}"
        );
    }

    /// A2：`/info` 的地址必须跟当前协议一致（HTTPS 下给 http 地址，
    /// 手机上「复制地址」就复制到一个打不开的页面）。
    #[tokio::test]
    async fn info_reports_http_scheme_when_plain() {
        let (port, _dir) = setup("info_scheme", &[]).await;
        let resp = http_text(
            port,
            "GET /info HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await;
        assert!(resp.contains("\"url\":\"http://"), "resp: {resp}");
        // 顺带：桌面端要能看到配对锁状态
        assert!(resp.contains("\"pair_locked\":0"), "resp: {resp}");
    }

    /// B6：页面正文随 `Accept-Encoding` 变，就必须带 `Vary`（200 和 304 都要）。
    #[tokio::test]
    async fn page_has_vary_accept_encoding() {
        let (port, _dir) = setup("vary", &[]).await;
        let page = http_text(port, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        assert!(page.contains("Vary: Accept-Encoding"), "200 缺 Vary: {page}");

        // 拿到 ETag 后再发一次带 If-None-Match 的请求，应当 304 且同样带 Vary
        let etag = page
            .lines()
            .find_map(|l| l.strip_prefix("ETag: "))
            .expect("ETag")
            .trim()
            .to_string();
        let not_modified = http_text(
            port,
            &format!(
                "GET / HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\n\r\n"
            ),
        )
        .await;
        assert!(not_modified.contains("304"), "resp: {not_modified}");
        assert!(
            not_modified.contains("Vary: Accept-Encoding"),
            "304 缺 Vary: {not_modified}"
        );
    }

    /// B2：输错达到上限后进入**冷却**，而不是永久锁死；到点自动恢复。
    #[tokio::test]
    async fn pair_lockout_cools_down() {
        let cfg = Config::with_secret(TEST_SECRET.to_string(), TEST_CODE.to_string());
        for _ in 0..crate::config::PAIR_MAX_FAILURES {
            cfg.note_pair_failure();
        }
        let left = cfg.pair_lock_left().expect("达到上限后应该处于锁定状态");
        assert!(left > 0 && left <= crate::config::PAIR_COOLDOWN.as_secs());

        // 把到期时间拨到过去，模拟冷却结束
        *cfg.pair_locked_until.lock().unwrap() = Some(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        assert!(cfg.pair_lock_left().is_none(), "冷却结束后必须自动解锁");
    }

    /// B2：设置面板的「重新生成配对码」要换掉码和密钥，并立刻解锁。
    #[tokio::test]
    async fn reset_pairing_rotates_and_unlocks() {
        let cfg = Config::with_secret(TEST_SECRET.to_string(), TEST_CODE.to_string());
        for _ in 0..crate::config::PAIR_MAX_FAILURES {
            cfg.note_pair_failure();
        }
        assert!(cfg.pair_lock_left().is_some());

        let (code, secret) = cfg.reset_pairing();
        assert_ne!(code, TEST_CODE);
        assert_eq!(code.len(), 8);
        assert_ne!(secret, TEST_SECRET);
        assert_eq!(cfg.pair_code(), code);
        assert_eq!(cfg.pair_secret(), secret);
        assert!(cfg.pair_lock_left().is_none(), "换码后必须立刻可用");
    }

    /// B1：WebSocket 便签必须和 `POST /note` 一样受 4 MB 上限约束。
    #[tokio::test]
    async fn websocket_note_is_size_limited() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderValue;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let cfg = Arc::new(Config::with_secret(
            TEST_SECRET.to_string(),
            TEST_CODE.to_string(),
        ));
        let dir = std::env::temp_dir().join(format!("transdata_ws_size_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c = cfg2.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, &c, "127.0.0.1").await;
                });
            }
        });

        let mut req = format!("ws://127.0.0.1:{port}/ws")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("X-TD-Secret", HeaderValue::from_str(TEST_SECRET).unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

        // 正常大小的便签：应该被接受
        ws.send(WsMsg::Text("small note".into())).await.unwrap();
        for _ in 0..50 {
            if cfg.read_note().0.as_str() == "small note" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(cfg.read_note().0.as_str(), "small note");

        // 超过 4 MB：必须被拒（连接会被库按 max_message_size 断掉，或者被
        // 我们的长度检查丢弃），总之便签内容不能变成那坨大文本。
        let big = "x".repeat(crate::config::NOTE_MAX_BYTES + 1024);
        let _ = ws.send(WsMsg::Text(big.into())).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            cfg.read_note().0.as_str(),
            "small note",
            "超限内容不能覆盖便签"
        );
    }

    /// A4：`/verify` 不再受「目录里文件总数」影响 —— 这条用例要造 2 万+ 文件，
    /// 默认不跑（`cargo test -- --ignored`）。
    #[tokio::test]
    #[ignore]
    async fn verify_handles_more_files_than_the_old_cap() {
        let (port, dir) = setup("verify_many", &[]).await;
        let n = crate::http::server::VERIFY_CAP + 500;
        for i in 0..n {
            let d = dir.join(format!("d{}", i % 100));
            std::fs::create_dir_all(&d).ok();
            std::fs::write(d.join(format!("f{i}.bin")), b"x").ok();
        }
        // 清单里只挑最后 100 个（旧的「先扫全目录再比对」实现会漏掉它们）
        let mut entries = Vec::new();
        for i in (n - 100)..n {
            entries.push(format!(
                r#"{{"path":"d{}/f{i}.bin","size":1}}"#,
                i % 100
            ));
        }
        let body = format!(r#"{{"files":[{}]}}"#, entries.join(","));
        let req = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let resp = http_text(port, &req).await;
        assert!(resp.contains("\"all_ok\":true"), "resp: {}", &resp[..resp.len().min(400)]);
    }

    /// 上传进度用的 `%` 编码路径之外，普通中文/空格文件名也要能下载。
    #[tokio::test]
    async fn unicode_filename_downloads() {
        let (port, _dir) = setup("unicode_name", &[("中文 文件.txt", b"ok")]).await;
        let encoded = "%E4%B8%AD%E6%96%87%20%E6%96%87%E4%BB%B6.txt";
        let resp = http_bytes(
            port,
            &format!(
                "GET /download/{encoded} HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n"
            ),
        )
        .await;
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.ends_with("ok"), "{text}");
    }

    /// 一次完成的下载 = 一条记录，大小是实际发出的字节；同一个文件的重复
    /// 请求（浏览器探测 / 断线重连 / 连点）在 10 秒内并成一条。
    #[tokio::test]
    async fn download_logs_once_per_transfer_and_merges_retries() {
        let (port, _dir, cfg) = setup_cfg(
            "dl_log",
            &[("a.txt", &b"AAAA"[..]), ("b/c.txt", &b"CC"[..])],
        )
        .await;
        let a = "GET /download/a.txt HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n";
        let _ = http_bytes(port, a).await;
        let acts = cfg.recent_activity(10);
        assert_eq!(acts.len(), 1, "一次下载一条记录");
        assert_eq!(acts[0].kind, crate::config::ActKind::Download);
        assert_eq!(acts[0].name, "a.txt");
        assert_eq!(acts[0].size, 4, "大小用实际发出的字节");

        let _ = http_bytes(port, a).await;
        assert_eq!(cfg.recent_activity(10).len(), 1, "10 秒内同一文件合并成一条");

        let _ = http_bytes(
            port,
            "GET /download/b/c.txt HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await;
        assert_eq!(cfg.recent_activity(10).len(), 2, "不同文件各一条");
    }

    /// 传到一半被掐断的下载不记账（浏览器那边也只列完成的下载）。
    #[tokio::test]
    async fn aborted_download_is_not_recorded() {
        let big = vec![7u8; 4 * 1024 * 1024];
        let (port, _dir, cfg) = setup_cfg("dl_abort", &[("big.bin", &big[..])]).await;
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        c.write_all(
            b"GET /download/big.bin HTTP/1.1\r\nHost: t\r\nX-TD-Secret: test-secret\r\n\r\n",
        )
        .await
        .unwrap();
        drop(c);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            cfg.recent_activity(10).is_empty(),
            "中断的下载不能记成一次下载"
        );
    }

    /// 切换 HTTPS 开关前后，同一个端口必须一直能连上。
    #[tokio::test]
    async fn tls_toggle_keeps_port_serving() {
        let cert_dir = std::env::temp_dir()
            .join(format!("transdata_tls_toggle_{}", std::process::id()));
        std::fs::create_dir_all(&cert_dir).ok();
        let material = crate::tls::load_or_create_in(
            &["127.0.0.1".to_string()],
            std::slice::from_ref(&cert_dir),
        )
        .expect("certificate");

        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let dir = std::env::temp_dir().join(format!("transdata_toggle_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let mut raw = Config::with_secret(TEST_SECRET.to_string(), TEST_CODE.to_string());
        raw.port = port;
        raw.set_upload_dir(dir);
        let cfg = Arc::new(raw);
        cfg.set_tls_material(material);
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let _ = crate::http::server::serve(cfg2).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 关：明文能连
        let r = http_text(port, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 200"), "plain before: {}", &r[..r.len().min(200)]);

        // 手机开着页面时会一直占着一条连接（WebSocket），切协议时它还在。
        let held = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        // 开：监听重绑，明文会拿到「请用 https」提示页
        cfg.set_tls(true);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let r = http_text(port, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        assert!(r.contains("HTTPS"), "after tls on: {}", &r[..r.len().min(200)]);

        // 再关：必须还能明文连
        cfg.set_tls(false);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        drop(held);
        let r = http_text(port, "GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        assert!(
            r.starts_with("HTTP/1.1 200"),
            "server_error: {:?}, body: {}",
            cfg.server_error(),
            &r[..r.len().min(200)]
        );
    }
}
