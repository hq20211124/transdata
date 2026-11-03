//! 单实例锁。
//!
//! 启动时会清理传输目录里的残留文件（`.td-tmp-*.part`）和上一次更新留下的
//! `*.exe.old`。如果允许同时跑两个实例，第二个实例启动时会把第一个实例
//! **正在写**的临时文件当成垃圾删掉 —— 上传会断在中间，而且看起来毫无原因。
//! 所以先拿锁，拿到锁才有资格清理。
//!
//! Windows 上用 `CreateFileW` + `dwShareMode = 0`（独占打开）实现：文件本身
//! 留在盘上无所谓，句柄是进程级的，进程一退出锁就没了，不会留下「上次没正常
//! 关闭所以永远打不开」的僵局。

use std::path::PathBuf;

pub struct InstanceLock {
    #[allow(dead_code)]
    path: PathBuf,
    #[cfg(windows)]
    handle: isize,
}

/// 拿到锁返回 `Some`；已经有实例在跑返回 `None`。
#[cfg(windows)]
pub fn acquire() -> Option<InstanceLock> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut core::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: isize,
        ) -> isize;
    }

    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_ALWAYS: u32 = 4;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const INVALID_HANDLE_VALUE: isize = -1;

    let path = lock_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // share_mode = 0 → 第二个实例会拿到 ERROR_SHARING_VIOLATION。
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    Some(InstanceLock { path, handle })
}

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" {
            fn CloseHandle(handle: isize) -> i32;
        }
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// 非 Windows 平台没有共享语义这么直接的文件锁，退化成「不限制单实例」
/// （本项目只发布 Windows 版；这里只为让 `cargo check` 在别的平台能过）。
#[cfg(not(windows))]
pub fn acquire() -> Option<InstanceLock> {
    Some(InstanceLock {
        path: lock_path(),
    })
}

fn lock_path() -> PathBuf {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("TransData").join("transdata.lock");
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("transdata.lock")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 同一进程里第二次取锁必须失败（等价于第二个实例启动）。
    ///
    /// 注意：这把锁是**全局**的，所以本机只要有人在跑 `C:\TransData\transdata.exe`，
    /// 第一次取锁就会失败 —— 那是环境冲突，不是回归。失败信息要说清这一点，
    /// 否则每次都会当成新 bug 查一遍。
    #[test]
    fn second_instance_cannot_start() {
        let Some(first) = acquire() else {
            panic!(
                "拿不到实例锁：本机有 TransData 正在运行（默认在 C:\\TransData\\transdata.exe）。\
                 先停掉它再跑这个测试 —— 这不是回归。"
            );
        };
        assert!(acquire().is_none(), "第二个实例必须被拒绝");
        drop(first);
        let again = acquire().expect("释放后应该又能拿到");
        drop(again);
    }
}
