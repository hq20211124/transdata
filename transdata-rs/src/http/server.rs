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

use crate::config::Config;

const MAX_HEAD: usize = 8 * 1024;
const CHUNK: usize = 256 * 1024;

/// Fixed session token used by the unit tests (the real one is random per
/// run and lives in `Config::token`).
#[cfg(test)]
pub const TEST_TOKEN: &str = "test-token";
/// /files listing cap; deeper trees are truncated and flagged.
const FILE_LIST_CAP: usize = 10_000;
/// /verify per-request cap.
const VERIFY_CAP: usize = 20_000;

/// Serializes the "check for name -> create file" step so two concurrent
/// uploads of the same folder cannot both create `<x>_1`.
static UPLOAD_LOCK: Mutex<()> = Mutex::new(());

pub async fn serve(cfg: Arc<Config>) {
    cfg.set_running(true);
    let listener = match TcpListener::bind(("0.0.0.0", cfg.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind :{} failed: {e}", cfg.port);
            cfg.set_running(false);
            return;
        }
    };
    println!("listening on 0.0.0.0:{}", cfg.port);
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                let cfg = cfg.clone();
                task::spawn(async move {
                    let _ = handle_conn(sock, &cfg).await;
                });
            }
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// request parsing

struct Request {
    method: String,
    path: String,
    query: String,
    boundary: Option<String>,
    content_length: usize,
    ws: bool,
    /// `X-TD-Token` header (the page's fetch calls send it).
    token: Option<String>,
    /// Raw `Cookie` header.
    cookie: Option<String>,
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
    // The target may carry a query string (`?t=<token>` for download links
    // and the websocket, which cannot set request headers).
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };
    let path = url_decode(raw_path);

    let mut boundary = None;
    let mut content_length = 0usize;
    let mut ws = false;
    let mut token = None;
    let mut cookie = None;
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
        } else if lname == "x-td-token" {
            token = Some(v.to_string());
        } else if lname == "cookie" {
            cookie = Some(v.to_string());
        } else if lname == "accept-encoding" && v.to_ascii_lowercase().contains("gzip") {
            accept_gzip = true;
        } else if lname == "if-none-match" {
            if_none_match = Some(v.to_string());
        }
    }
    Some(Request {
        method,
        path,
        query,
        boundary,
        content_length,
        ws,
        token,
        cookie,
        accept_gzip,
        if_none_match,
    })
}

/// Value of a query parameter, e.g. `t` in `?t=abc`.
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

/// Is this request allowed to talk to the API?
///
/// The page is served by this very server, so it is same-origin and needs no
/// CORS. The session token is what keeps *other* websites out: without it a
/// page open in the phone/PC browser could list, download or delete the files
/// in the transfer directory (the old `Access-Control-Allow-Origin: *` made
/// that trivial for any site on the internet).
///
/// The token may arrive three ways: the `X-TD-Token` header (fetch calls),
/// a `?t=` parameter (download links / websocket) or the `td_token` cookie
/// (set when the page was served).
fn authorized(cfg: &Config, req: &Request) -> bool {
    if req.token.as_deref() == Some(cfg.token.as_str()) {
        return true;
    }
    if let Some(t) = query_param(&req.query, "t") {
        if t == cfg.token {
            return true;
        }
    }
    if let Some(c) = &req.cookie {
        for kv in c.split(';') {
            let kv = kv.trim();
            if kv.strip_prefix("td_token=") == Some(cfg.token.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Read the HTTP request head.
///
/// Reads in 4 KB chunks instead of one byte at a time: the old byte-at-a-time
/// loop cost one syscall + one reactor wake-up *per byte*, which showed up
/// as 15-30 ms of pure latency for a 1 KB header.
async fn read_head(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Result<usize, std::io::Error> {
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
    sock: &mut TcpStream,
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
    sock: &mut TcpStream,
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

async fn handle_conn(mut sock: TcpStream, cfg: &Arc<Config>) -> Result<(), std::io::Error> {
    // TCP_NODELAY: keep large transfers from hitting Nagle+delayed-ACK stalls.
    let _ = sock.set_nodelay(true);
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

    // The page itself is public (it carries the token); every API call has
    // to present the session token.
    let is_page = req.method == "GET" && (req.path == "/" || req.path == "/index.html");
    if !is_page && !authorized(cfg, &req) {
        return write_json_status(&mut sock, "401 Unauthorized", "Token required").await;
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
            // Shared quick-note, unlimited length: stream the body in bounded
            // chunks so memory stays flat no matter how big the text is.
            let total = req.content_length;
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
            write_page(&mut sock, cfg, &req).await
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
            let body = format!(
                r#"{{"success":true,"url":"{url}","files_count":{count},"save_dir":"{dir_s}","active":{active}}}"#
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
            let mut stack = vec![dir.clone()];
            while let Some(d) = stack.pop() {
                if let Ok(rd) = d.read_dir() {
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.is_dir() {
                            stack.push(p.to_path_buf());
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
            match sanitize_rel(&url_decode(&p["/download/".len()..])) {
                Some(rel) => download(&mut sock, cfg, &rel).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        ("DELETE", p) if p.starts_with("/delete/") => {
            match sanitize_rel(&url_decode(&p["/delete/".len()..])) {
                Some(rel) => delete(&mut sock, cfg, &rel).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        ("POST", "/upload") => upload(&mut sock, cfg, &req.boundary, &mut pending).await,
        ("POST", "/verify") => verify(&mut sock, cfg, &req, &mut pending).await,
        ("GET", p) if p.starts_with("/zip/") => {
            match sanitize_rel(&url_decode(&p["/zip/".len()..])) {
                Some(rel) => zip_dir(&mut sock, cfg, &rel).await,
                None => write_json_err(&mut sock, "Bad path").await,
            }
        }
        _ => write_json_err(&mut sock, "Not Found").await,
    }
}

/// TCP stream that first replays already-read HTTP request head bytes, then
/// transparently forwards to the socket. Needed because `accept_async()`
/// wants to parse the upgrade request itself, but we already read the head.
struct HeadedStream {
    head: std::vec::IntoIter<u8>,
    stream: TcpStream,
}

impl tokio::io::AsyncRead for HeadedStream {
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

impl tokio::io::AsyncWrite for HeadedStream {
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
    sock: TcpStream,
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

async fn write_body(
    sock: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &[u8],
) -> Result<(), std::io::Error> {
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-store, max-age=0\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    Ok(())
}

/// Serve the index page.
///
/// - gzip when the client accepts it (30 KB -> ~6 KB, which matters a lot on
///   a phone over Wi-Fi)
/// - ETag + `must-revalidate` so a reload costs a 304 instead of a full page
/// - a `HttpOnly; SameSite=Strict` cookie carrying the session token, so the
///   page can talk to the API without exposing the token to scripts
async fn write_page(
    sock: &mut TcpStream,
    cfg: &Arc<Config>,
    req: &Request,
) -> Result<(), std::io::Error> {
    let cookie = format!(
        "Set-Cookie: td_token={}; Path=/; HttpOnly; SameSite=Strict\r\n",
        cfg.token
    );
    // Not modified? Then the browser can keep its copy.
    if let Some(inm) = &req.if_none_match {
        if inm.split(',').any(|t| t.trim() == cfg.page_etag) {
            let head = format!(
                "HTTP/1.1 304 Not Modified\r\nETag: {}\r\nCache-Control: private, max-age=0, must-revalidate\r\n{}Connection: close\r\n\r\n",
                cfg.page_etag, cookie
            );
            sock.write_all(head.as_bytes()).await?;
            sock.flush().await?;
            return Ok(());
        }
    }
    let gz = req.accept_gzip;
    let body = if gz {
        cfg.page_gz.as_slice()
    } else {
        cfg.page.as_slice()
    };
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
    if gz {
        out.extend_from_slice(b"Content-Encoding: gzip\r\n");
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(format!("ETag: {}\r\n", cfg.page_etag).as_bytes());
    out.extend_from_slice(
        b"Cache-Control: private, max-age=0, must-revalidate\r\n",
    );
    out.extend_from_slice(cookie.as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    sock.write_all(&out).await?;
    sock.flush().await?;
    Ok(())
}

async fn write_json(sock: &mut TcpStream, body: &str) -> Result<(), std::io::Error> {
    write_body(
        sock,
        "200 OK",
        "application/json; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

async fn write_json_err(sock: &mut TcpStream, msg: &str) -> Result<(), std::io::Error> {
    write_json_status(sock, "404 Not Found", msg).await
}

/// JSON error with an explicit HTTP status.
async fn write_json_status(
    sock: &mut TcpStream,
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
/// onto the upload dir: `\` -> `/`; rejects absolute paths, drive letters,
/// `..`/empty segments and control characters. Returns `None` to reject.
fn sanitize_rel(s: &str) -> Option<String> {
    let s = s.replace('\\', "/");
    let mut segs: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        if seg.is_empty() || seg == ".." {
            return None;
        }
        if seg.chars().any(|c| c.is_control()) {
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

// ---------------------------------------------------------------------------
// download (streamed from disk in chunks)

async fn download(
    sock: &mut TcpStream,
    cfg: &Arc<Config>,
    rel: &str,
) -> Result<(), std::io::Error> {
    let file = cfg.upload_dir().join(rel);
    if !file.is_file() {
        return write_json_err(sock, "Not Found").await;
    }
    let mime = guess_mime(&file);
    let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let name = rel
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();

    let encoded = percent_encode(&name);
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

// ---------------------------------------------------------------------------
// delete (file or whole subtree)

async fn delete(
    sock: &mut TcpStream,
    cfg: &Arc<Config>,
    rel: &str,
) -> Result<(), std::io::Error> {
    let dir = cfg.upload_dir();
    let p = dir.join(rel);
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
    sock: &mut TcpStream,
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
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
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
    let rel = match sanitize_rel(path) {
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
    sock: &mut TcpStream,
    cfg: &Arc<Config>,
    rel: &str,
) -> Result<(), std::io::Error> {
    use std::io::{Read as _Read, Write as _Write};

    let dir = cfg.upload_dir().join(rel);
    if !dir.is_dir() {
        return write_json_err(sock, "Not Found").await;
    }
    // The zip crate needs a *seekable* output (it patches each entry's CRC
    // after the data), so build the archive in a temp file inside the
    // transfer dir, then stream it to the socket and delete it. Memory
    // stays flat; disk cost is one compressed copy while zipping.
    let root = cfg.upload_dir();
    let top = rel.split('/').next().unwrap_or("");
    let safe: String = top
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let tmp_name = if safe.is_empty() {
        ".files.zip.tmp".to_string()
    } else {
        format!(".{safe}.zip.tmp")
    };
    let tmp = root.join(tmp_name);

    let dir_b = dir.clone();
    let tmp_b = tmp.clone();
    let root_b = root.clone();
    let build = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
        let mut file = std::fs::File::create(&tmp_b)?;
        let mut zip = zip::ZipWriter::new(&mut file);
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut n = 0usize;
        let mut stack = vec![dir_b.clone()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
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
    sock: &mut TcpStream,
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
        if body_done {
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

    if !body_done {
        // Connection dropped/closed mid-body: remove the partial files we
        // created so a client retry starts clean.
        for p in &created {
            let _ = std::fs::remove_file(p);
        }
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
                return Some(match sanitize_rel(&decoded) {
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
            return Some(match sanitize_rel(&decoded) {
                Some(p) => p,
                None => safe_leaf(&decoded),
            });
        }
    }
    None
}

/// Last-resort file name guaranteed safe: final path component with
/// control characters and colons stripped.
fn safe_leaf(name: &str) -> String {
    let norm = name.replace('\\', "/");
    let last = norm.split('/').last().unwrap_or("upload");
    let c: String = last.chars().filter(|c| !c.is_control() && *c != ':').collect();
    if c.is_empty() {
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
    let i = (1..)
        .find(|i| !dir.join(format!("{stem}_{i}{ext}")).exists())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "name conflict"))?;
    let target = dir.join(format!("{stem}_{i}{ext}"));
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
        let cfg = Arc::new(Config::with_token(TEST_TOKEN.to_string()));
        cfg.set_upload_dir(dir);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2).await.ok();
        });
        (port, cfg)
    }

    /// The session token must be picked up from the header, the query
    /// parameter or the cookie — and a request without it is rejected.
    #[test]
    fn session_token_is_required() {
        let cfg = Config::with_token("secret".to_string());
        let mk = |extra: &str| {
            parse_request(&format!(
                "GET /files HTTP/1.1\r\nHost: t\r\n{extra}\r\n"
            ))
            .unwrap()
        };
        assert!(!authorized(&cfg, &mk("")));
        assert!(authorized(&cfg, &mk("X-TD-Token: secret\r\n")));
        assert!(authorized(&cfg, &mk("Cookie: td_token=secret\r\n")));
        assert!(!authorized(&cfg, &mk("X-TD-Token: wrong\r\n")));
        // query parameter
        let mut q = mk("X-TD-Token: secret\r\n");
        q.query = "t=secret".to_string();
        assert!(authorized(&cfg, &q));
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
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
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
            "POST /upload HTTP/1.0\r\nHost: 127.0.0.1\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
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
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {total}\r\n\r\n"
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
        let cfg = Arc::new(Config::with_token(super::TEST_TOKEN.to_string()));
        cfg.set_upload_dir(dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_conn(sock, &cfg2).await.ok();
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
            "POST /upload HTTP/1.1\r\nHost: 127.0.0.1\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
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
        let cfg = Arc::new(Config::with_token(super::TEST_TOKEN.to_string()));
        cfg.set_upload_dir(dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let c2 = cfg.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, &c2).await;
                });
            }
        });
        (port, dir)
    }

    async fn http_bytes(port: u16, raw: &str) -> Vec<u8> {
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(raw.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        out
    }

    async fn http_text(port: u16, raw: &str) -> String {
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
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
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
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
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
            "POST /upload HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: 99999\r\n\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big/x.bin\"\r\n\r\nAAAA",
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
        let resp = http_text(port, "GET /files HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\n\r\n").await;
        assert!(resp.contains("docs/sub/a.txt"), "resp: {resp}");
        assert!(resp.contains("top.txt"), "resp: {resp}");
    }

    #[tokio::test]
    async fn verify_reports_status() {
        let (port, _dir) = setup("verify", &[("docs/a.txt", b"AAA")]).await;
        let body =
            r#"{"files":[{"path":"docs/a.txt","size":3},{"path":"docs/missing.txt","size":5}]}"#;
        let req = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_text(port, &req).await;
        assert!(resp.contains("\"all_ok\":false"), "resp: {resp}");
        assert!(resp.contains("docs/a.txt\",\"status\":\"ok\""), "resp: {resp}");
        assert!(resp.contains("\"missing\""), "resp: {resp}");

        let body2 = r#"{"files":[{"path":"docs/a.txt","size":3}]}"#;
        let req2 = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body2.len(),
            body2
        );
        let resp2 = http_text(port, &req2).await;
        assert!(resp2.contains("\"all_ok\":true"), "resp2: {resp2}");

        // Wrong size -> size_mismatch
        let body3 = r#"{"files":[{"path":"docs/a.txt","size":999}]}"#;
        let req3 = format!(
            "POST /verify HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
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
        let resp = http_text(port, "DELETE /delete/docs HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\n\r\n").await;
        assert!(resp.contains("\"success\":true"), "resp: {resp}");
        assert!(!dir.join("docs").exists(), "tree must be gone");
    }

    #[tokio::test]
    async fn zip_smoke_test() {
        let (port, dir) = setup("zip", &[("docs/a.txt", b"AAA"), ("docs/sub/b.txt", b"BBBB")]).await;
        let resp = http_bytes(port, "GET /zip/docs HTTP/1.1\r\nHost: t\r\nX-TD-Token: test-token\r\n\r\n").await;
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
