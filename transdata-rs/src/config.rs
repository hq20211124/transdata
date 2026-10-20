use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::broadcast;

/// Shared, runtime-mutable server configuration.
///
/// The HTTP handler reads these atomically per request, so the GUI can
/// change the transfer directory live without restarting the server.
pub struct Config {
    pub port: u16,
    /// Where files are stored / served from. Movable at runtime.
    pub upload_dir: RwLock<PathBuf>,
    /// Whether the server is currently running.
    pub running: AtomicUsize,
    /// Shared quick-note text (unlimited length), last-writer-wins.
    ///
    /// Shared as an `Arc<String>` so the GUI can render it every frame
    /// without copying the whole text 60 times a second.
    pub note_buf: RwLock<Arc<String>>,
    /// Monotonic version of the shared note (0 = never written).
    pub note_version: AtomicU64,
    /// Broadcast of JSON payloads ("{\"v\":N,\"text\":\"...\"}") pushed to
    /// every /ws client whenever the note changes.
    pub note_tx: broadcast::Sender<String>,
    /// Random per-run session token.
    ///
    /// The web page is served from this server, so it is same-origin and
    /// needs no CORS. The token is what stops *other* websites: without it
    /// a page in the phone/PC browser could read `/files`, delete files or
    /// fetch the note from this server (the old `Access-Control-Allow-Origin: *`
    /// allowed exactly that).
    pub token: String,
    /// The index page with the token injected, plus its gzip form and ETag.
    /// Built once at startup: the page never changes afterwards.
    pub page: Vec<u8>,
    pub page_gz: Vec<u8>,
    pub page_etag: String,
    /// Cached `/files` payload: (built_at, json). A short TTL keeps the
    /// download list snappy; uploads/deletes invalidate it immediately.
    pub files_cache: RwLock<Option<(Instant, String)>>,
    /// Number of transfers currently in flight (uploads + downloads).
    pub active_transfers: AtomicUsize,
}

impl Config {
    pub fn new() -> Self {
        // Session token: 32 hex chars from the OS RNG.
        let mut bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self::with_token(token)
    }

    /// Same as [`Config::new`] but with a caller-provided session token
    /// (the tests use a fixed one so their raw requests can carry it).
    pub fn with_token(token: String) -> Self {
        // Default: <exe_dir>/uploads
        let mut dir = std::env::current_exe()
            .map(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
            .unwrap_or_else(|| PathBuf::from("."));
        dir = dir.join("uploads");
        std::fs::create_dir_all(&dir).ok();

        let (note_tx, _) = broadcast::channel::<String>(32);

        // The page is static for the lifetime of the process: build it (and
        // its gzip form) once instead of per request.
        let page = crate::web::page_html(&token).into_bytes();
        let page_gz = gzip(&page);
        let page_etag = format!("\"{:016x}\"", fnv1a(&page));

        Self {
            port: 9999,
            upload_dir: RwLock::new(dir),
            running: AtomicUsize::new(0),
            note_buf: RwLock::new(Arc::new(String::new())),
            note_version: AtomicU64::new(0),
            note_tx,
            token,
            page,
            page_gz,
            page_etag,
            files_cache: RwLock::new(None),
            active_transfers: AtomicUsize::new(0),
        }
    }

    pub fn set_upload_dir(&self, dir: PathBuf) {
        std::fs::create_dir_all(&dir).ok();
        *self.upload_dir.write().unwrap() = dir;
    }

    pub fn upload_dir(&self) -> PathBuf {
        self.upload_dir.read().unwrap().clone()
    }

    pub fn set_running(&self, v: bool) {
        self.running.store(v as usize, Ordering::SeqCst);
    }

    // ---- shared quick-note -------------------------------------------------

    /// Store new shared-note text, bump the version, and broadcast it to all
    /// connected /ws clients. Returns the new version.
    pub fn store_note(&self, text: &str) -> u64 {
        let v = self.note_version.fetch_add(1, Ordering::SeqCst) + 1;
        *self.note_buf.write().unwrap() = Arc::new(text.to_string());
        let payload = format!(
            r#"{{"v":{v},"text":"{}"}}"#,
            Self::json_escape(text)
        );
        let _ = self.note_tx.send(payload); // lagged receivers just skip
        v
    }

    /// Read the current shared note and its version.
    ///
    /// The text is returned as an `Arc<String>` so callers (the GUI renders
    /// it every frame) can hold it without copying.
    pub fn read_note(&self) -> (Arc<String>, u64) {
        (
            self.note_buf.read().unwrap().clone(),
            self.note_version.load(Ordering::SeqCst),
        )
    }

    /// Drop the cached `/files` payload (after an upload or a delete).
    pub fn invalidate_files(&self) {
        *self.files_cache.write().unwrap() = None;
    }

    /// Cached `/files` JSON, if it is still fresh (2 s TTL).
    pub fn cached_files(&self) -> Option<String> {
        let guard = self.files_cache.read().unwrap();
        match guard.as_ref() {
            Some((at, body)) if at.elapsed().as_secs_f64() < 2.0 => Some(body.clone()),
            _ => None,
        }
    }

    /// Store a freshly built `/files` JSON.
    pub fn store_files_cache(&self, body: String) {
        *self.files_cache.write().unwrap() = Some((Instant::now(), body));
    }

    /// Subscribe to live note updates (one receiver per /ws connection).
    pub fn note_rx(&self) -> broadcast::Receiver<String> {
        self.note_tx.subscribe()
    }

    /// Minimal JSON string escaper for the hand-written JSON payloads.
    pub fn json_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32))
                }
                _ => out.push(c),
            }
        }
        out
    }
}

/// gzip-compress a buffer (used for the index page).
pub fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 3), Compression::fast());
    if enc.write_all(data).is_err() {
        return data.to_vec();
    }
    enc.finish().unwrap_or_else(|_| data.to_vec())
}

/// FNV-1a: cheap, stable content hash (used for the page ETag).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
