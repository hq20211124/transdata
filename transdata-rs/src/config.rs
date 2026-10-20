use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, watch};

/// How long a paired device stays authorized without re-pairing.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// Per-IP pairing attempts allowed inside the rate-limit window.
pub const PAIR_ATTEMPTS: usize = 10;
/// Length of the rate-limit window for pairing attempts.
pub const PAIR_WINDOW: Duration = Duration::from_secs(300);
/// After this many *failed* pairing attempts the server stops accepting
/// codes for the rest of the run (brute-force lockout).
pub const PAIR_MAX_FAILURES: u32 = 30;

/// Upper bound for the shared note (bytes). The note is meant for pasting
/// text around, not for shipping a database dump.
pub const NOTE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Hard ceiling for the bytes a single run may write into the transfer
/// directory. The real limit is the free space on the volume (see
/// `Config::with_secret`), this is the backstop for other platforms.
pub const QUOTA_FALLBACK: u64 = 50 * 1024 * 1024 * 1024;

/// Keep this much free on the volume, so the machine never runs out of
/// space entirely because of a transfer.
const DISK_RESERVE: u64 = 1024 * 1024 * 1024;

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
    /// Random per-run pairing secret.
    ///
    /// This is *not* handed out to the browser any more. It only travels
    /// inside the QR code (URL fragment, never sent to the server) and
    /// inside the desktop app. Everything a browser gets is a short-lived
    /// session id in an `HttpOnly` cookie.
    pub pair_secret: String,
    /// 8-digit pairing code shown in the desktop window, for devices that
    /// cannot scan the QR code.
    pub pair_code: String,
    /// Live sessions: session id -> last time it was used.
    pub sessions: RwLock<HashMap<String, Instant>>,
    /// Pairing attempts per peer address (ip -> timestamps).
    pub pair_attempts: Mutex<HashMap<String, Vec<Instant>>>,
    /// Set once the brute-force lockout trips.
    pub pair_locked: AtomicBool,
    /// Number of failed pairing attempts since startup.
    pub pair_failures: AtomicU32,
    /// The index page in its two variants (paired / not paired) plus the
    /// static assets. Built once at startup: they never change afterwards.
    pub page_app: Vec<u8>,
    pub page_app_gz: Vec<u8>,
    pub page_app_etag: String,
    pub page_pair: Vec<u8>,
    pub page_pair_gz: Vec<u8>,
    pub page_pair_etag: String,
    pub asset_css: Vec<u8>,
    pub asset_js: Vec<u8>,
    /// ETag of the static assets: the browser must revalidate them (the app
    /// is rebuilt and the assets change with it), but a 304 is cheap.
    pub asset_css_etag: String,
    pub asset_js_etag: String,
    /// Cached `/files` payload: (built_at, json). A short TTL keeps the
    /// download list snappy; uploads/deletes invalidate it immediately.
    pub files_cache: RwLock<Option<(Instant, String)>>,
    /// Number of transfers currently in flight (uploads + downloads).
    pub active_transfers: AtomicUsize,
    /// Number of uploads currently in flight (separate cap, see `Config`).
    pub active_uploads: AtomicUsize,
    /// How many bytes this run may still write into the transfer dir.
    pub quota_left: AtomicU64,
    /// Is the server serving HTTPS right now?
    pub tls_enabled: AtomicBool,
    /// Notifies the server task that the TLS mode changed.
    pub tls_tx: watch::Sender<bool>,
    /// The self-signed certificate (present as soon as it was created, even
    /// while HTTPS is switched off).
    pub tls: RwLock<Option<Arc<TlsMaterial>>>,
}

/// Everything needed to serve HTTPS with the self-signed certificate.
pub struct TlsMaterial {
    /// The certificate in PEM form.
    pub cert_pem: String,
    /// The private key in PEM form.
    pub key_pem: String,
    /// SHA-256 fingerprint of the certificate, hex, colon separated.
    /// The phone shows the same value — that is how a user can check that
    /// the warning really belongs to this program.
    pub fingerprint: String,
    /// The rustls server configuration.
    pub server_config: Arc<rustls::ServerConfig>,
}

impl Config {
    pub fn new() -> Self {
        // Pairing secret: 32 hex chars from the OS RNG.
        let mut bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        // Pairing code: 8 digits, brute force is hopeless even without the
        // rate limit (10^8 combinations).
        let code: u32 = rand::Rng::gen_range(&mut rand::thread_rng(), 0..100_000_000);
        let code = format!("{code:08}");
        Self::with_secret(secret, code)
    }

    /// Same as [`Config::new`] but with caller-provided pairing material
    /// (the tests use fixed values so their raw requests can carry them).
    pub fn with_secret(secret: String, code: String) -> Self {
        // Default: <exe_dir>/uploads
        let mut dir = std::env::current_exe()
            .map(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
            .unwrap_or_else(|| PathBuf::from("."));
        dir = dir.join("uploads");
        std::fs::create_dir_all(&dir).ok();

        let (note_tx, _) = broadcast::channel::<String>(32);
        let (tls_tx, _) = watch::channel(false);

        // The pages are static for the lifetime of the process: build them
        // (and their gzip forms) once instead of per request.
        let page_app = crate::web::page_html("app").into_bytes();
        let page_pair = crate::web::page_html("pair").into_bytes();
        let page_app_gz = gzip(&page_app);
        let page_pair_gz = gzip(&page_pair);
        let page_app_etag = format!("\"{:016x}\"", fnv1a(&page_app));
        let page_pair_etag = format!("\"{:016x}\"", fnv1a(&page_pair));

        // Upload quota: whatever is free on the volume (minus a reserve),
        // capped so a huge disk does not remove the limit entirely.
        let quota = match disk_free(&dir) {
            Some(free) if free > DISK_RESERVE => {
                (free - DISK_RESERVE).min(QUOTA_FALLBACK)
            }
            _ => QUOTA_FALLBACK,
        };

        Self {
            port: 9999,
            upload_dir: RwLock::new(dir),
            running: AtomicUsize::new(0),
            note_buf: RwLock::new(Arc::new(String::new())),
            note_version: AtomicU64::new(0),
            note_tx,
            pair_secret: secret,
            pair_code: code,
            sessions: RwLock::new(HashMap::new()),
            pair_attempts: Mutex::new(HashMap::new()),
            pair_locked: AtomicBool::new(false),
            pair_failures: AtomicU32::new(0),
            page_app,
            page_app_gz,
            page_app_etag,
            page_pair,
            page_pair_gz,
            page_pair_etag,
            asset_css: crate::web::APP_CSS.as_bytes().to_vec(),
            asset_js: crate::web::APP_JS.as_bytes().to_vec(),
            asset_css_etag: format!("\"{:016x}\"", fnv1a(crate::web::APP_CSS.as_bytes())),
            asset_js_etag: format!("\"{:016x}\"", fnv1a(crate::web::APP_JS.as_bytes())),
            files_cache: RwLock::new(None),
            active_transfers: AtomicUsize::new(0),
            active_uploads: AtomicUsize::new(0),
            quota_left: AtomicU64::new(quota),
            tls_enabled: AtomicBool::new(false),
            tls_tx,
            tls: RwLock::new(None),
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

    // ---- HTTPS ------------------------------------------------------------

    /// Switch HTTPS on or off. The server task notices the change and
    /// re-binds the listener (same port, different protocol).
    pub fn set_tls(&self, on: bool) {
        self.tls_enabled.store(on, Ordering::SeqCst);
        let _ = self.tls_tx.send(on);
    }

    /// Is HTTPS currently switched on?
    pub fn tls_on(&self) -> bool {
        self.tls_enabled.load(Ordering::SeqCst)
    }

    /// The certificate material (present even while HTTPS is off).
    pub fn tls_material(&self) -> Option<Arc<TlsMaterial>> {
        self.tls.read().unwrap().clone()
    }

    /// Store the certificate material.
    pub fn set_tls_material(&self, m: Arc<TlsMaterial>) {
        *self.tls.write().unwrap() = Some(m);
    }

    // ---- sessions ---------------------------------------------------------

    /// Create a new session and return its id.
    pub fn new_session(&self) -> String {
        let mut bytes = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        let sid: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut map = self.sessions.write().unwrap();
        map.insert(sid.clone(), Instant::now());
        map.retain(|_, at| at.elapsed() < SESSION_TTL);
        sid
    }

    /// Is this session id still valid? Touches it on success.
    pub fn session_valid(&self, sid: &str) -> bool {
        let mut map = self.sessions.write().unwrap();
        match map.get_mut(sid) {
            Some(at) if at.elapsed() < SESSION_TTL => {
                *at = Instant::now();
                true
            }
            Some(_) => {
                map.remove(sid);
                false
            }
            None => false,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.read().unwrap().len()
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

/// Free bytes on the volume that holds `path` (None if unknown).
#[cfg(windows)]
pub fn disk_free(path: &std::path::Path) -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    // 0 = free for the caller, 1 = total free on the volume.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        None
    } else {
        Some(total_free)
    }
}

#[cfg(not(windows))]
pub fn disk_free(_path: &std::path::Path) -> Option<u64> {
    None
}
