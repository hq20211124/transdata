//! 文件操作 —— 管理端（GUI）和 HTTP 接口共用的一套实现。
//!
//! 为什么要单独一层：桌面端的目录列表要能删、能改名，而这些操作原本只写在
//! `http::server` 的 handler 里，一套并发规则（`UPLOAD_LOCK` 临界区、撞名绝不
//! 覆盖、越界路径拒绝、删除后归还配额、缓存失效、活动日志）都长在 HTTP 的
//! 请求/响应结构上。
//!
//! 如果 GUI 直接调 `std::fs::remove_file`，它和别的设备就**不在同一把锁上**：
//! 可能删掉别人正在写的目标，也可能两边同时改名互相覆盖。所以这里把规则搬
//! 出来，HTTP handler 变成薄壳，GUI 只调这里 —— 两边排同一把锁。
//!
//! 诚实边界：只保证「通过本程序的操作」之间原子。用户拿资源管理器直接动同一个
//! 目录，程序管不了（Windows 权限模型决定的）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use crate::config::{ActKind, Config};

/// 上传发布 / 删除 / 改名共用的一把锁。
///
/// 这些操作的共同点是「先检查再动手」（目标是否已存在、是否正在被写），
/// 中间被另一个操作插进来就会出现「检查完不存在 → 别人刚好建了一个 → 覆盖掉」。
pub static UPLOAD_LOCK: Mutex<()> = Mutex::new(());

/// `/files` 与 GUI 列表的条目上限（文件数）。
pub const FILE_LIST_CAP: usize = 10_000;
/// 目录递归深度上限，防止软链接环（虽然软链接本身会被跳过）。
pub const MAX_WALK_DEPTH: usize = 24;
const MAX_SEGMENT_LEN: usize = 200;
const MAX_PATH_LEN: usize = 1024;

// ---------------------------------------------------------------------------
// 路径：把客户端给的相对路径变成安全的、落在传输目录内的路径

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
pub fn safe_rel(s: &str) -> Option<String> {
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
        if seg
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
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
pub fn is_reserved_name(seg: &str) -> bool {
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
pub fn resolve_rel(cfg: &Config, rel: &str) -> Option<PathBuf> {
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

/// Path as shown in the activity log: relative to the transfer dir, `/`
/// separated. Falls back to the file name if it somehow sits outside.
pub fn rel_display(cfg: &Config, p: &Path) -> String {
    p.strip_prefix(cfg.upload_dir())
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

/// 列表 / 打包时要跳过的上传中间文件。
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with(".td-tmp-") && name.ends_with(".part")
}

// ---------------------------------------------------------------------------
// 配额

/// Take `n` bytes from the run quota. Returns false when the quota is
/// exhausted (the transfer is then aborted instead of filling the disk).
pub fn take_quota(cfg: &Config, n: u64) -> bool {
    cfg.quota_left
        .fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |cur| if cur >= n { Some(cur - n) } else { None },
        )
        .is_ok()
}

/// Give the quota back (used when an aborted upload is cleaned up, and when
/// a file is deleted).
pub fn give_quota(cfg: &Config, n: u64) {
    cfg.quota_left
        .fetch_add(n, std::sync::atomic::Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// 列表

/// 目录里的一项（文件或目录）。
#[derive(Clone, Debug)]
pub struct Entry {
    /// 相对传输目录的路径，`/` 分隔。
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间（Unix 秒），取不到为 0。
    pub mtime: u64,
}

/// 一次目录快照。
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    /// 因为条目上限/深度上限没能列全。
    pub truncated: bool,
}

impl Snapshot {
    /// 只保留文件条目（`/files` 接口用；网页端不需要目录行）。
    pub fn files(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.is_dir)
    }
}

/// 递归列出传输目录。
///
/// 文件和目录都返回（GUI 要拼树），`/files` 只把文件条目序列化成 JSON，
/// 所以网页端看到的仍然是原来的东西。
///
/// 跳过：软链接（可能指向目录外，也能成环）、上传中的 `.td-tmp-*.part`。
/// 上限：文件数 `FILE_LIST_CAP`，目录深度 `MAX_WALK_DEPTH`。
pub fn list(cfg: &Config) -> Snapshot {
    let dir = cfg.upload_dir();
    let mut out = Snapshot::default();
    let mut files = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.clone(), 0)];
    while let Some((d, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH {
            out.truncated = true;
            break;
        }
        let Ok(rd) = d.read_dir() else { continue };
        for e in rd.flatten() {
            let p = e.path();
            // Skip symlinks: they can point outside the transfer directory
            // and can form cycles.
            match e.file_type() {
                Ok(ft) if ft.is_symlink() => continue,
                Err(_) => continue,
                _ => {}
            }
            let rel = p
                .strip_prefix(&dir)
                .ok()
                .map(|s| s.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if p.is_dir() {
                stack.push((p.clone(), depth + 1));
                out.entries.push(Entry {
                    rel,
                    is_dir: true,
                    size: 0,
                    mtime: mtime_of(&p),
                });
            } else {
                // 正在上传的临时文件不算「已有文件」
                if p.file_name()
                    .map(|n| is_temp_name(&n.to_string_lossy()))
                    .unwrap_or(false)
                {
                    continue;
                }
                if files >= FILE_LIST_CAP {
                    out.truncated = true;
                    break;
                }
                files += 1;
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.entries.push(Entry {
                    rel,
                    is_dir: false,
                    size,
                    mtime: mtime_of(&p),
                });
            }
        }
        if out.truncated {
            break;
        }
    }
    out
}

fn mtime_of(p: &Path) -> u64 {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 删除

/// 删掉一个文件或一整棵目录树的结果。
#[derive(Clone, Debug, Default)]
pub struct DeleteSummary {
    pub rel: String,
    pub is_dir: bool,
    pub files: usize,
    /// 目录里删掉的子目录数（GUI 的 toast 暂时只用 files）
    #[allow(dead_code)]
    pub dirs: usize,
    #[allow(dead_code)]
    pub bytes: u64,
}

/// 删文件或整棵目录树。`who` 是活动日志里的操作者（`"本机"` 或设备 IP）。
///
/// 错误串直接是给用户看的中文（HTTP 层原样回给网页，GUI 拿去写状态栏）。
pub fn delete(cfg: &Arc<Config>, rel: &str, who: &str) -> Result<DeleteSummary, String> {
    let Some(p) = resolve_rel(cfg, rel) else {
        return Err("Bad path".to_string());
    };
    let shown = rel_display(cfg, &p);
    if p.is_file() {
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(&p).is_ok() {
            // 删掉的文件把配额还回去（否则删了也传不进新东西）
            give_quota(cfg, size);
            cfg.log_activity(who, ActKind::Delete, &shown, size);
            cfg.invalidate_files();
            return Ok(DeleteSummary {
                rel: shown,
                is_dir: false,
                files: 1,
                dirs: 0,
                bytes: size,
            });
        }
    } else if p.is_dir() {
        // Remove the whole tree: files first, then directories deepest-first.
        let mut files: Vec<PathBuf> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
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
        let freed: u64 = files
            .iter()
            .filter_map(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        let n_files = files.len();
        let n_dirs = dirs.len();
        let _ = files.iter().map(|f| std::fs::remove_file(f)).count();
        let _ = dirs.into_iter().rev().map(|d| std::fs::remove_dir(d)).count();
        let _ = std::fs::remove_dir(&p);
        give_quota(cfg, freed);
        if p.exists() {
            // 目录还在：里面多半有文件正被上传/下载占用（Windows 删不掉打开的文件）
            cfg.invalidate_files();
            return Err("文件夹里还有文件正被上传或下载占用，请稍后再删".to_string());
        }
        cfg.log_activity(who, ActKind::Delete, &format!("{shown}/"), 0);
        cfg.invalidate_files();
        return Ok(DeleteSummary {
            rel: shown,
            is_dir: true,
            files: n_files,
            dirs: n_dirs,
            bytes: freed,
        });
    }
    if p.exists() {
        // 还在，说明删不掉：多半是另一台设备正在下载 / 正在往里传
        return Err("文件正被占用（可能正在下载或上传），请稍后再删".to_string());
    }
    Err("Not Found".to_string())
}

// ---------------------------------------------------------------------------
// 改名 / 移动

/// 重命名（`to` 可以带子目录 —— 改名和移动是同一个操作）。
///
/// 整段在 `UPLOAD_LOCK` 里做：跟上传发布、删除互斥，所以不会出现「检查完
/// 目标不存在 → 别人刚好建了一个 → 覆盖掉」这种竞态。目标已存在一律拒绝，
/// 绝不覆盖。
pub fn rename(cfg: &Arc<Config>, from: &str, to: &str, who: &str) -> Result<(), String> {
    if from.is_empty() || to.is_empty() {
        return Err("参数不完整".to_string());
    }
    let Some(src) = resolve_rel(cfg, from) else {
        return Err("找不到原文件".to_string());
    };
    if !src.exists() {
        return Err("找不到原文件".to_string());
    }
    // 新名字同样要过安全校验（不能跳出传输目录、不能有非法字符）
    let Some(to_rel) = safe_rel(to) else {
        return Err("新名字不合法".to_string());
    };
    let dir = cfg.upload_dir();
    let dst = dir.join(&to_rel);
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("建目录失败：{e}"));
        }
        // 目标目录必须仍在传输目录里（软链接/junction 不能成为出口）
        if let (Ok(p), Ok(b)) = (parent.canonicalize(), dir.canonicalize()) {
            if !p.starts_with(&b) {
                return Err("新路径超出了传输目录".to_string());
            }
        }
    }

    let outcome: Result<(), String> = {
        let _guard = UPLOAD_LOCK.lock().ok();
        // 只差大小写（Windows）算同一个文件，允许改；真的撞名就拒绝，
        // 绝不覆盖别人的文件。
        let same_file = dst.to_string_lossy().to_lowercase()
            == src.to_string_lossy().to_lowercase();
        if dst.exists() && !same_file {
            Err("同名文件已存在".to_string())
        } else {
            match std::fs::rename(&src, &dst) {
                Ok(()) => Ok(()),
                Err(e) => Err(format!("改不了：{e}（可能正在被下载或上传）")),
            }
        }
    };
    outcome?;
    cfg.invalidate_files();
    cfg.log_activity(who, ActKind::Rename, &format!("{from} → {to}"), 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cfg(tag: &str) -> Arc<Config> {
        let dir = std::env::temp_dir().join(format!(
            "transdata_ops_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).ok();
        let cfg = Arc::new(Config::with_secret("s".into(), "12345678".into()));
        cfg.set_upload_dir(dir);
        cfg
    }

    fn write(cfg: &Config, rel: &str, bytes: &[u8]) {
        let p = cfg.upload_dir().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// GUI 要拼树，所以列表里**目录也要有**（`/files` 自己再过滤掉）。
    #[test]
    fn ops_list_includes_dirs() {
        let cfg = tmp_cfg("dirs");
        write(&cfg, "docs/sub/a.txt", b"AAA");
        write(&cfg, "top.txt", b"T");
        std::fs::create_dir_all(cfg.upload_dir().join("empty")).unwrap();

        let snap = list(&cfg);
        let dirs: Vec<&str> = snap
            .entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.rel.as_str())
            .collect();
        assert!(dirs.contains(&"docs"), "{dirs:?}");
        assert!(dirs.contains(&"docs/sub"), "{dirs:?}");
        assert!(dirs.contains(&"empty"), "空目录也要列出来: {dirs:?}");

        let files: Vec<(&str, u64)> = snap.files().map(|e| (e.rel.as_str(), e.size)).collect();
        assert!(files.contains(&("docs/sub/a.txt", 3)), "{files:?}");
        assert!(files.contains(&("top.txt", 1)), "{files:?}");
        assert!(!snap.truncated);
    }

    /// 上传中的临时文件不算「已有文件」，软链接不跟。
    #[test]
    fn ops_list_skips_temp_files() {
        let cfg = tmp_cfg("temp");
        write(&cfg, "a.txt", b"1");
        write(&cfg, ".td-tmp-abc.part", b"2");
        let snap = list(&cfg);
        let names: Vec<&str> = snap.files().map(|e| e.rel.as_str()).collect();
        assert_eq!(names, vec!["a.txt"]);
    }

    /// 管理端删东西和别的设备传东西排在同一把锁上：不会留下半个文件。
    #[test]
    fn desktop_ops_are_atomic_with_upload() {
        let cfg = tmp_cfg("atomic");
        write(&cfg, "docs/a.txt", b"AAAA");

        // 模拟另一个上传正在写这个目录（Windows 上打开的文件删不掉）
        let tmp = cfg.upload_dir().join("docs/.td-tmp-live.part");
        let held = std::fs::File::create(&tmp).unwrap();
        let _ = &held; // 句柄一直拿着，直到函数结束

        let res = delete(&cfg, "docs", "本机");
        match res {
            Ok(_) => {
                // 删掉了：目录必须真的没了，且没有半个文件留下
                assert!(!cfg.upload_dir().join("docs").exists());
            }
            Err(msg) => {
                // 报占用：原文件必须完好
                assert!(msg.contains("占用"), "错误串要说人话: {msg}");
                assert_eq!(std::fs::read(cfg.upload_dir().join("docs/a.txt")).unwrap(), b"AAAA");
            }
        }
        drop(held);
    }

    /// 改名走 ops 和走 HTTP 是同一套规则：撞名拒绝、越界拒绝。
    #[test]
    fn ops_rename_rules() {
        let cfg = tmp_cfg("rename");
        write(&cfg, "a.txt", b"A");
        write(&cfg, "b.txt", b"B");

        rename(&cfg, "a.txt", "docs/c.txt", "本机").unwrap();
        assert_eq!(std::fs::read(cfg.upload_dir().join("docs/c.txt")).unwrap(), b"A");
        assert_eq!(rename(&cfg, "b.txt", "docs/c.txt", "本机"), Err("同名文件已存在".to_string()));
        assert!(rename(&cfg, "b.txt", "../x.txt", "本机").is_err());
        assert!(cfg.upload_dir().join("b.txt").exists());
        // 活动日志记的是「本机」
        let acts = cfg.recent_activity_for("本机", 5);
        assert!(acts.iter().any(|a| a.kind == ActKind::Rename), "{acts:?}");
    }
}
