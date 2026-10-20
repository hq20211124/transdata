//! 持久化设置：主题（浅色/深色）、更新源地址。
//!
//! 存在 exe 同目录的 `settings.json`，方便用户直接改更新源（比如指到自己的
//! 服务器），改坏了也不会影响程序启动（解析失败就用默认值）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    pub fn from_str(s: &str) -> Theme {
        if s.eq_ignore_ascii_case("dark") {
            Theme::Dark
        } else {
            Theme::Light
        }
    }
}

/// 当前主题。GUI 每帧取色时读一次，原子读很便宜。
static DARK: AtomicBool = AtomicBool::new(false);

pub fn theme() -> Theme {
    if DARK.load(Ordering::Relaxed) {
        Theme::Dark
    } else {
        Theme::Light
    }
}

pub fn set_theme(t: Theme) {
    DARK.store(t == Theme::Dark, Ordering::Relaxed);
}

/// 设置文件路径：exe 同目录。
pub fn settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("settings.json")))
        .unwrap_or_else(|| PathBuf::from("settings.json"))
}

#[derive(Clone)]
pub struct Settings {
    pub theme: Theme,
    /// 在线更新的检查地址。可以指向 GitHub Release，也可以指向自己的
    /// `latest.json`（见 update.rs 里支持的两种格式）。
    pub update_url: String,
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings {
            theme: Theme::Light,
            update_url: crate::update::DEFAULT_UPDATE_URL.to_string(),
        };
        if let Ok(text) = std::fs::read_to_string(settings_path()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(t) = v.get("theme").and_then(|x| x.as_str()) {
                    s.theme = Theme::from_str(t);
                }
                if let Some(u) = v.get("update_url").and_then(|x| x.as_str()) {
                    if !u.trim().is_empty() {
                        s.update_url = u.trim().to_string();
                    }
                }
            }
        }
        set_theme(s.theme);
        s
    }

    pub fn save(&self) {
        let v = serde_json::json!({
            "theme": self.theme.as_str(),
            "update_url": self.update_url,
        });
        if let Ok(text) = serde_json::to_string_pretty(&v) {
            let _ = std::fs::write(settings_path(), text);
        }
    }
}
