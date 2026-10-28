//! 在线更新：查版本 → 下载新 exe → 校验 → 换掉自己。
//!
//! 支持两种版本清单格式：
//!
//! A) 自定义清单（自己的服务器也行）：
//!    ```json
//!    { "version": "1.2.0", "url": "https://.../transdata-1.2.0.exe",
//!      "sha256": "…", "notes": "更新说明" }
//!    ```
//!
//! B) GitHub Release（默认源）：
//!    `https://api.github.com/repos/<owner>/<repo>/releases/latest`
//!    程序会取 `tag_name` 当版本号、找名字以 .exe 结尾的附件当下载地址。

use std::io::Read;
use std::path::{Path, PathBuf};

/// 当前版本（跟着 Cargo.toml 走）
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 默认的版本检查地址
pub const DEFAULT_UPDATE_URL: &str =
    "https://api.github.com/repos/hq20211124/transdata/releases/latest";

/// 一个可下载的版本
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// 期望的 sha256（小写十六进制），有就校验
    pub sha256: Option<String>,
    pub notes: Option<String>,
}

/// 更新状态机，GUI 靠它显示进度
#[derive(Clone, Debug)]
pub enum UpdateStatus {
    Idle,
    Checking,
    /// 已经是最新版（参数是服务器上的版本号）
    UpToDate(String),
    /// 有新版本
    Available(Release),
    Downloading {
        received: u64,
        total: u64,
    },
    /// 下载并替换完成，重启生效
    Ready,
    Failed(String),
}

/// 比较版本号：`remote` 比 `local` 新返回 true。
///
/// 只比较数字段，`v1.2.3` 和 `1.2.3` 等价；小数点后的段数不同按 0 补齐。
pub fn is_newer(remote: &str, local: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .trim_start_matches('V')
            .split(['.', '-', '+'])
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect()
    }
    let r = parts(remote);
    let l = parts(local);
    for i in 0..r.len().max(l.len()) {
        let a = r.get(i).copied().unwrap_or(0);
        let b = l.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

/// 服务器上一个版本都没有（仓库还没发过 release）时的提示
///
/// 这不算错误 —— 等于"没有新版本"。
pub const NO_RELEASE: &str = "更新服务器上还没有发布新版本";

/// 只接受 https 的更新源。
///
/// 清单和 exe 走明文 http 的话，路径上任何人都能把两者一起换掉——清单里的
/// sha256 会和攻击者的文件「完全匹配」，校验等于没做。
pub fn ensure_https(url: &str) -> Result<(), String> {
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    // 单测用本地假服务器（明文 http）跑「下载→校验→替换」全链路；
    // 仅测试构建、且仅回环地址放行，正式构建里没有这条分支。
    #[cfg(test)]
    if lower.starts_with("http://127.0.0.1:") || lower.starts_with("http://localhost:") {
        return Ok(());
    }
    Err("更新地址必须是 https://（明文 http 会被中间人替换）".to_string())
}

/// 拉取版本清单
pub fn fetch_release(url: &str) -> Result<Release, String> {
    ensure_https(url)?;
    let resp = match ureq::get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Err(NO_RELEASE.to_string()),
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!("更新服务器返回了 {code}"))
        }
        Err(e) => return Err(format!("连不上更新服务器：{e}")),
    };
    let body = resp
        .into_string()
        .map_err(|e| format!("读取更新信息失败：{e}"))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("更新信息格式不对：{e}"))?;
    parse_release(&v)
}

/// 解析两种格式的版本信息
pub fn parse_release(v: &serde_json::Value) -> Result<Release, String> {
    // A) 自定义清单
    if let Some(version) = v.get("version").and_then(|x| x.as_str()) {
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if url.is_empty() {
            return Err("更新信息里没有下载地址".to_string());
        }
        let sha256 = v
            .get("sha256")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let notes = v
            .get("notes")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        return Ok(Release {
            version: version.trim().trim_start_matches('v').to_string(),
            url,
            sha256,
            notes,
        });
    }

    // B) GitHub Release
    let tag = v
        .get("tag_name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "更新信息里找不到版本号".to_string())?;
    let version = tag.trim().trim_start_matches('v').to_string();
    let assets = v.get("assets").and_then(|x| x.as_array());
    // 只认 `.exe`。以前找不到时回退到 `assets.first()`，结果一个 release 里
    // 排在前面的 `.sha256` / `.zip` 会被当成程序下载下来，然后覆盖掉
    // transdata.exe —— 程序当场报废。
    let asset = assets.and_then(|list| {
        list.iter().find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.to_ascii_lowercase().ends_with(".exe"))
                .unwrap_or(false)
        })
    });
    let url = asset
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if url.is_empty() {
        return Err("这个版本没有可下载的 exe".to_string());
    }
    let sha256 = asset
        .and_then(|a| a.get("digest"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim_start_matches("sha256:").trim().to_string())
        .filter(|s| !s.is_empty());
    let notes = v
        .get("body")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(Release {
        version,
        url,
        sha256,
        notes,
    })
}

/// 下载到 `dest`。`on_progress` 每次收到数据块时回调（已收字节 / 总字节）。
pub fn download(
    url: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    ensure_https(url)?;
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;
    let total = resp
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("写文件失败：{e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        file.write_all(&buf[..n]).map_err(|e| format!("写文件失败：{e}"))?;
        received += n as u64;
        on_progress(received, total);
    }
    Ok(())
}

/// 文件的 sha256（小写十六进制）
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| format!("打不开文件：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}

/// 校验下载下来的文件；清单里没写 sha256 就跳过
pub fn verify(dest: &Path, expect: &Option<String>) -> Result<(), String> {
    let Some(want) = expect else { return Ok(()) };
    let got = sha256_file(dest)?;
    if got.eq_ignore_ascii_case(want.trim()) {
        Ok(())
    } else {
        Err("文件校验不通过（sha256 对不上），已放弃更新".to_string())
    }
}

/// 下载到的东西至少得像个 Windows 程序（开头两字节 `MZ`）。
///
/// sha256 是可选的（清单没写就跳过），所以还需要一道「是不是可执行文件」
/// 的兜底：一个 json / zip 就算哈希对上了，装上去也是废的。
pub fn verify_pe(path: &Path) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打不开下载的文件：{e}"))?;
    let mut magic = [0u8; 2];
    match f.read_exact(&mut magic) {
        Ok(()) if &magic == b"MZ" => Ok(()),
        Ok(()) => Err("下载到的文件不是 Windows 程序（缺少 MZ 头），已放弃更新".to_string()),
        Err(_) => Err("下载到的文件不完整，已放弃更新".to_string()),
    }
}

/// 临时下载文件放在 exe 旁边（同盘，替换时能直接改名）
pub fn temp_download_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("transdata.update.exe")))
        .unwrap_or_else(|| PathBuf::from("transdata.update.exe"))
}

/// 用新文件替换正在运行的 exe。
///
/// Windows 上运行中的 exe 不能直接删，但可以改名 —— 所以先把它改成
/// `.old`，再把新文件改名到位；万一第二步失败就把旧的改回去。
pub fn replace_exe(new_file: &Path, exe: &Path) -> Result<(), String> {
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)
        .map_err(|e| format!("无法替换正在运行的程序：{e}\n请把程序放到有写入权限的目录再试。"))?;
    if let Err(e) = std::fs::rename(new_file, exe) {
        // 换不回去的话程序就废了，宁可留着旧文件也不要没有 exe
        let _ = std::fs::rename(&old, exe);
        return Err(format!("替换失败：{e}"));
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// 更新完重启自己
pub fn restart_self() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起一个只服务两个路径的小 HTTP 服务器：
    /// /latest.json 返回版本清单，/app.exe 返回一个假 exe
    fn fake_server() -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                use std::io::Read;
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let (body, ctype): (Vec<u8>, &str) = if path.contains("latest.json") {
                    let json = format!(
                        r#"{{"version":"9.9.9","url":"http://127.0.0.1:{port}/app.exe","sha256":"{sha}","notes":"test"}}"#,
                        port = port,
                        sha = sha256_of(b"MZFAKE-EXE-CONTENT")
                    );
                    (json.into_bytes(), "application/json")
                } else {
                    (b"MZFAKE-EXE-CONTENT".to_vec(), "application/octet-stream")
                };
                let body = String::from_utf8_lossy(&body)
                    .replace("PORT", &port.to_string())
                    .replace("SHA", &sha256_of(b"MZFAKE-EXE-CONTENT"))
                    .into_bytes();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                use std::io::Write;
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        (format!("http://127.0.0.1:{port}/latest.json"), handle)
    }

    fn sha256_of(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 版本检查 → 下载 → 校验 → 换掉 exe，整条链路跑通
    #[test]
    fn update_roundtrip() {
        let (url, _server) = fake_server();

        // 1) 查版本
        let rel = fetch_release(&url).expect("应该能拉到版本清单");
        assert_eq!(rel.version, "9.9.9");
        assert!(is_newer(&rel.version, APP_VERSION));
        assert!(rel.url.ends_with("/app.exe"));
        assert_eq!(rel.sha256.as_deref(), Some(sha256_of(b"MZFAKE-EXE-CONTENT").as_str()));

        // 2) 下载 + 校验
        let dir = std::env::temp_dir().join("td_update_test");
        std::fs::create_dir_all(&dir).unwrap();
        let new_file = dir.join("transdata.update.exe");
        let mut last = (0u64, 0u64);
        download(&rel.url, &new_file, |r, t| last = (r, t))
            .expect("下载应该成功");
        assert!(last.0 > 0, "应该收到数据");
        verify(&new_file, &rel.sha256).expect("校验应该通过");
        verify_pe(&new_file).expect("MZ 头检查应该通过");
        assert!(std::fs::read(&new_file).unwrap().starts_with(b"MZFAKE"));

        // 3) 校验不过要拦下来
        let bad = Some("0".repeat(64));
        assert!(verify(&new_file, &bad).is_err(), "sha256 对不上必须报错");

        // 4) 替换 exe（拿临时文件当替身，不碰真的 exe）
        let exe = dir.join("transdata.exe");
        std::fs::write(&exe, b"OLD").unwrap();
        replace_exe(&new_file, &exe).expect("替换应该成功");
        assert!(std::fs::read(&exe).unwrap().starts_with(b"MZFAKE"));
        assert!(!dir.join("transdata.exe.old").exists(), "旧文件应该清掉");

        // 5) 换一个不存在的文件当来源 → 失败，但原来的 exe 还在
        let exe2 = dir.join("transdata2.exe");
        std::fs::write(&exe2, b"OLD").unwrap();
        assert!(replace_exe(&dir.join("not-exist.exe"), &exe2).is_err());
        assert_eq!(std::fs::read(&exe2).unwrap(), b"OLD", "替换失败时旧文件要保住");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_version_is_detected() {
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("v2.0", "1.9.9"));
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("1.0", "1.0.0"));
    }

    #[test]
    fn custom_manifest_is_parsed() {
        let v = serde_json::json!({
            "version": "1.2.0",
            "url": "https://example.com/a.exe",
            "sha256": "abc",
            "notes": "hello"
        });
        let r = parse_release(&v).unwrap();
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.url, "https://example.com/a.exe");
        assert_eq!(r.sha256.as_deref(), Some("abc"));
        assert_eq!(r.notes.as_deref(), Some("hello"));
    }

    #[test]
    fn github_release_is_parsed() {
        let v = serde_json::json!({
            "tag_name": "v1.3.0",
            "body": "notes",
            "assets": [
                { "name": "transdata-1.3.0.exe", "browser_download_url": "https://example.com/t.exe" }
            ]
        });
        let r = parse_release(&v).unwrap();
        assert_eq!(r.version, "1.3.0");
        assert_eq!(r.url, "https://example.com/t.exe");
    }

    /// 一个 release 里没有 `.exe` 时必须报错。
    ///
    /// 以前会回退到 `assets.first()`：排在前面的 `.sha256` / `.zip` 会被当成
    /// 程序下载下来，然后覆盖掉 transdata.exe —— 程序当场报废。
    #[test]
    fn non_exe_asset_is_rejected() {
        let v = serde_json::json!({
            "tag_name": "v1.4.0",
            "assets": [
                { "name": "transdata-1.4.0.zip", "browser_download_url": "https://example.com/a.zip" },
                { "name": "SHA256SUMS.txt", "browser_download_url": "https://example.com/b.txt" }
            ]
        });
        assert!(parse_release(&v).is_err(), "没有 exe 附件就必须报错，不能乱装");

        let empty = serde_json::json!({ "tag_name": "v1.4.0", "assets": [] });
        assert!(parse_release(&empty).is_err());
    }

    /// 明文 http 的更新源直接拒绝（中间人可以把清单和 exe 一起换掉，
    /// 清单里的 sha256 会和攻击者的文件「完全匹配」）。
    #[test]
    fn http_update_url_is_rejected() {
        assert!(ensure_https("http://example.com/latest.json").is_err());
        assert!(ensure_https("https://example.com/latest.json").is_ok());
        assert!(ensure_https("HTTP://example.com/x").is_err());
    }

    /// 下载到的东西不是 Windows 程序（没有 MZ 头）要拦下来。
    #[test]
    fn non_pe_payload_is_rejected() {
        let dir = std::env::temp_dir().join("td_pe_test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("not-an-exe.bin");
        std::fs::write(&f, b"PK\x03\x04this is a zip").unwrap();
        assert!(verify_pe(&f).is_err(), "zip 不能被当成 exe");

        let ok = dir.join("looks-like-exe.bin");
        std::fs::write(&ok, b"MZ\x90\x00rest").unwrap();
        assert!(verify_pe(&ok).is_ok());

        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(verify_pe(&empty).is_err(), "空文件必须被拒");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
