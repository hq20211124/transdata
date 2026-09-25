use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::RwLock;

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
    pub note_buf: RwLock<String>,
    /// Monotonic version of the shared note (0 = never written).
    pub note_version: AtomicU64,
    /// Broadcast of JSON payloads ("{\"v\":N,\"text\":\"...\"}") pushed to
    /// every /ws client whenever the note changes.
    pub note_tx: broadcast::Sender<String>,
}

impl Config {
    pub fn new() -> Self {
        // Default: <exe_dir>/uploads
        let mut dir = std::env::current_exe()
            .map(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
            .unwrap_or_else(|| PathBuf::from("."));
        dir = dir.join("uploads");
        std::fs::create_dir_all(&dir).ok();

        let (note_tx, _) = broadcast::channel::<String>(32);

        Self {
            port: 9999,
            upload_dir: RwLock::new(dir),
            running: AtomicUsize::new(0),
            note_buf: RwLock::new(String::new()),
            note_version: AtomicU64::new(0),
            note_tx,
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
        *self.note_buf.write().unwrap() = text.to_string();
        let payload = format!(
            r#"{{"v":{v},"text":"{}"}}"#,
            Self::json_escape(text)
        );
        let _ = self.note_tx.send(payload); // lagged receivers just skip
        v
    }

    /// Read the current shared note and its version.
    pub fn read_note(&self) -> (String, u64) {
        (
            self.note_buf.read().unwrap().clone(),
            self.note_version.load(Ordering::SeqCst),
        )
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
