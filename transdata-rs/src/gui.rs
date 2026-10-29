use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::RichText;
use eframe::{App, Frame};

use crate::config::Config;

/// v1.3 管理端的三栏界面（目录 / 设备 / 详情）
mod workspace;

/// 主窗口尺寸：v1.3 起是**管理端**，一屏三栏（目录 / 连接+设备 / 设备详情）。
/// 尺寸是算出来的：内宽 1168 = 目录 516 + 设备 296 + 详情 336 + 两处 10px 间距。
const WIN_W: f32 = 1200.0;
const WIN_H: f32 = 760.0;
/// 最小尺寸：再小就放不下三栏，右两栏收起，标题栏出现「设备 (n)」抽屉按钮。
const WIN_MIN_W: f32 = 640.0;
const WIN_MIN_H: f32 = 470.0;

/// 三栏的固定宽度与间距
const COL_DEVICE_W: f32 = 296.0;
const COL_DETAIL_W: f32 = 336.0;
const COL_GAP: f32 = 10.0;
/// 窄于这个宽度就收起右两栏（三栏放不下，不做横向滚动）
const WIDE_MIN_W: f32 = 1000.0;
/// 目录行高 / 设备行高 / 详情行高
const ROW_H: f32 = 30.0;
const DEV_ROW_H: f32 = 30.0;
const ACT_ROW_H: f32 = 26.0;
/// 渲染上限（防手机/大目录把界面拖死）
const RENDER_CAP: usize = 500;
/// 便签条高度（两行输入 + 按钮）
const NOTE_STRIP_H: f32 = 76.0;
/// 便签提示自动消失的时间
const NOTE_STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(10);
/// 自绘标题栏的高度（原来这块是系统标题栏，深色模式下是一条黑边）
const TITLEBAR_H: f32 = 42.0;

// ---------------------------------------------------------------------------
// 窗口圆角（Windows）
//
// 系统标题栏关掉以后，窗口默认是直角，跟下面自己画的圆角卡片对不上。
// 做法：把窗口区域（HRGN）裁成一个圆角矩形，圆角外的地方直接透出桌面，
// DWM 的阴影也跟着这个区域走，看起来就是个正常的圆角窗口。
// 最大化时取消裁剪（最大化本来就是满屏直角）。
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win_shape {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::Mutex;

    static TARGET_PID: AtomicU32 = AtomicU32::new(0);
    static FOUND_HWND: AtomicIsize = AtomicIsize::new(0);
    /// 本进程的所有顶层窗口（句柄 + 面积），用来挑出主窗口
    static CANDIDATES: Mutex<Vec<(isize, i64)>> = Mutex::new(Vec::new());

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowRect(hwnd: isize, rect: *mut RectRaw) -> i32;
        fn SetWindowRgn(hwnd: isize, rgn: *mut c_void, redraw: i32) -> i32;
        fn GetClientRect(hwnd: isize, rect: *mut RectRaw) -> i32;
        fn GetDpiForWindow(hwnd: isize) -> u32;
        fn IsZoomed(hwnd: isize) -> i32;
        fn MonitorFromPoint(pt: PointRaw, flags: u32) -> isize;
        fn GetMonitorInfoW(mon: isize, mi: *mut MonitorInfoRaw) -> i32;
        fn SetWindowPos(
            hwnd: isize,
            after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetCursorPos(pt: *mut PointRaw) -> i32;
        fn SetCapture(hwnd: isize) -> i32;
        fn ReleaseCapture() -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateRoundRectRgn(l: i32, t: i32, r: i32, b: i32, w: i32, h: i32) -> *mut c_void;
    }

    #[repr(C)]
    struct RectRaw {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct PointRaw {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct MonitorInfoRaw {
        cb_size: u32,
        rc_monitor: RectRaw,
        rc_work: RectRaw,
        dw_flags: u32,
    }

    const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;

    extern "system" fn enum_cb(hwnd: isize, _lparam: isize) -> i32 {
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != TARGET_PID.load(Ordering::Relaxed) {
            return 1;
        }
        // 注意：这里只能用不发消息的 API（GetWindowRect）。
        // GetWindowTextW 会给同线程的窗口发 WM_GETTEXT，
        // winit 的事件代理在等自己处理 → 死锁，窗口根本起不来。
        let mut r = RectRaw {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            GetWindowRect(hwnd, &mut r);
        }
        let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
        if let Ok(mut c) = CANDIDATES.lock() {
            c.push((hwnd, area));
        }
        1
    }

    /// 找到本进程的主窗口句柄（eframe 0.28 不直接暴露 HWND）。
    /// 本进程里还有 wgpu 的辅助窗口和 IME 窗口，按面积最大的那个认。
    pub fn find_main_window() -> isize {
        TARGET_PID.store(std::process::id(), Ordering::Relaxed);
        if let Ok(mut c) = CANDIDATES.lock() {
            c.clear();
        }
        unsafe {
            EnumWindows(enum_cb, 0);
        }
        let best = CANDIDATES
            .lock()
            .ok()
            .and_then(|c| c.iter().copied().max_by_key(|(_, a)| *a))
            .map(|(h, _)| h)
            .unwrap_or(0);
        FOUND_HWND.store(best, Ordering::Relaxed);
        best
    }

    /// 按当前客户区大小重新裁剪窗口区域；`radius` 是逻辑像素
    pub fn apply(hwnd: isize, radius_px: i32) {
        if hwnd == 0 {
            return;
        }
        let mut rect = RectRaw {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            if GetClientRect(hwnd, &mut rect) == 0 {
                return;
            }
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if IsZoomed(hwnd) != 0 {
                // 最大化：去掉圆角
                SetWindowRgn(hwnd, std::ptr::null_mut(), 1);
                return;
            }
            let _dpi = GetDpiForWindow(hwnd);
            let r = radius_px.max(1);
            // 区域比窗口大一像素，否则右下角会被切掉
            let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, r * 2, r * 2);
            if !rgn.is_null() {
                // 成功后区域归系统所有，不用 DeleteObject
                SetWindowRgn(hwnd, rgn, 1);
            }
        }
    }

    pub fn client_size_px(hwnd: isize) -> (i32, i32) {
        if hwnd == 0 {
            return (0, 0);
        }
        let mut rect = RectRaw {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            if GetClientRect(hwnd, &mut rect) == 0 {
                return (0, 0);
            }
        }
        (
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }

    pub fn dpi(hwnd: isize) -> u32 {
        if hwnd == 0 {
            return 96;
        }
        unsafe {
            let d = GetDpiForWindow(hwnd);
            if d == 0 {
                96
            } else {
                d
            }
        }
    }

    /// 窗口现在是最大化状态吗
    pub fn is_zoomed(hwnd: isize) -> bool {
        if hwnd == 0 {
            return false;
        }
        unsafe { IsZoomed(hwnd) != 0 }
    }

    /// 光标在屏幕上的位置（物理像素）
    pub fn cursor_pos() -> Option<(i32, i32)> {
        let mut pt = PointRaw { x: 0, y: 0 };
        unsafe {
            if GetCursorPos(&mut pt) == 0 {
                return None;
            }
        }
        Some((pt.x, pt.y))
    }

    /// 窗口在屏幕上的矩形（左上右下，物理像素）
    pub fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
        if hwnd == 0 {
            return None;
        }
        let mut r = RectRaw {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            if GetWindowRect(hwnd, &mut r) == 0 {
                return None;
            }
        }
        Some((r.left, r.top, r.right, r.bottom))
    }

    /// 移动并调整窗口大小（物理像素）
    pub fn set_window_rect(hwnd: isize, x: i32, y: i32, w: i32, h: i32) -> bool {
        if hwnd == 0 {
            return false;
        }
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                w.max(1),
                h.max(1),
                SWP_NOZORDER | SWP_NOACTIVATE,
            ) != 0
        }
    }

    /// 把鼠标抓到自己窗口里（拖到窗口外面也能继续收到移动事件）
    pub fn set_capture(hwnd: isize) {
        unsafe {
            SetCapture(hwnd);
        }
    }

    pub fn release_capture() {
        unsafe {
            ReleaseCapture();
        }
    }

    /// 把窗口挪到主显示器工作区正中。
    ///
    /// 必须用物理像素算：每显示器 DPI 感知的进程里 GetSystemMetrics 给的是
    /// 物理像素，而窗口尺寸是逻辑点，两者混用窗口会开在屏幕外面。
    pub fn center_on_primary(hwnd: isize) {
        if hwnd == 0 {
            return;
        }
        unsafe {
            let mut wr = RectRaw {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut wr) == 0 {
                return;
            }
            let w = wr.right - wr.left;
            let h = wr.bottom - wr.top;
            let mon = MonitorFromPoint(
                PointRaw { x: 0, y: 0 },
                MONITOR_DEFAULTTOPRIMARY,
            );
            if mon == 0 {
                return;
            }
            let mut mi = MonitorInfoRaw {
                cb_size: std::mem::size_of::<MonitorInfoRaw>() as u32,
                rc_monitor: RectRaw {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                rc_work: RectRaw {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                dw_flags: 0,
            };
            if GetMonitorInfoW(mon, &mut mi) == 0 {
                return;
            }
            let x = mi.rc_work.left + (mi.rc_work.right - mi.rc_work.left - w) / 2;
            let y = mi.rc_work.top + (mi.rc_work.bottom - mi.rc_work.top - h) / 2;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(windows)]
const WIN_CORNER_RADIUS: f32 = 12.0;

#[cfg(windows)]
fn update_window_shape(hwnd: isize, last: &mut (i32, i32)) {
    let (w, h) = win_shape::client_size_px(hwnd);
    if (w, h) == *last && w > 0 {
        return;
    }
    *last = (w, h);
    let dpi = win_shape::dpi(hwnd) as f32 / 96.0;
    win_shape::apply(hwnd, (WIN_CORNER_RADIUS * dpi) as i32);
}

/// Load Microsoft YaHei (微软雅黑) so Chinese text renders instead of tofu
/// boxes. Falls back to SimSun, then nothing (app still runs).
fn cjk_font() -> Option<(Vec<u8>, u32)> {    for (path, index) in [
        (r"C:\Windows\Fonts\msyh.ttc", 0u32),
        (r"C:\Windows\Fonts\msyhbd.ttc", 0),
        (r"C:\Windows\Fonts\simsun.ttc", 0),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            return Some((bytes, index));
        }
    }
    None
}

/// Query the primary monitor size so we can center the window on launch.
/// The values are converted to the same units as the window size (points):
/// in a per-monitor DPI-aware process GetSystemMetrics reports *physical*
/// pixels, so on a 150% screen the window would start half off-screen.
fn primary_monitor_size() -> (f32, f32) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn GetSystemMetrics(index: i32) -> i32;
            fn GetDpiForSystem() -> u32;
        }
        let w = unsafe { GetSystemMetrics(0) };
        let h = unsafe { GetSystemMetrics(1) };
        if w > 0 && h > 0 {
            let dpi = unsafe { GetDpiForSystem() }.max(96);
            let scale = dpi as f32 / 96.0;
            return (w as f32 / scale, h as f32 / scale);
        }
    }
    (1920.0, 1080.0)
}

/// Runtime window/taskbar icon: the app icon, decoded once at startup from
/// the embedded PNG (avoids any disk lookup for the icon itself).
fn window_icon() -> std::sync::Arc<eframe::egui::IconData> {
    let bytes = include_bytes!("../icon_32.png");
    if let Ok(buf) = image::load_from_memory(bytes) {
        let buf = buf.into_rgba8();
        let (w, h) = (buf.width() as u32, buf.height() as u32);
        return std::sync::Arc::new(eframe::egui::IconData {
            rgba: buf.into_raw(),
            width: w,
            height: h,
        });
    }
    // Degenerate fallback: a 1x1 transparent icon keeps the API happy.
    std::sync::Arc::new(eframe::egui::IconData {
        rgba: vec![0, 0, 0, 0],
        width: 1,
        height: 1,
    })
}

/// 弹一个错误框。GUI 程序没有 stderr，错误只写进日志的话用户根本看不到
/// （表现为"双击了没反应"）。
#[cfg(windows)]
pub fn show_error(msg: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, flags: u32) -> i32;
    }
    const MB_ICONERROR: u32 = 0x00000010;
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let text = wide(msg);
    let caption = wide("TransData");
    unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
pub fn show_error(msg: &str) {
    eprintln!("{msg}");
}

pub fn run_gui(cfg: Arc<Config>, port: u16) {
    // 先探一下端口：被占用时直接弹窗说明，别开出一个"看起来正常"、
    // 其实连不上的窗口（那是最难查的失败方式）。
    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => drop(l),
        Err(e) => {
            show_error(&format!(
                "无法监听端口 {port}：{e}\n\n\
                 端口可能已经被另一个 TransData 占用了（重复启动？）。\n\
                 请关掉已经在运行的 TransData，或用任务管理器结束占用它的进程。"
            ));
            return;
        }
    }

    let cfg_server = cfg.clone();
    let cfg_gui = cfg.clone();
    // 持久化设置：主题（浅色/深色）、在线更新源
    let settings = crate::settings::Settings::load();
    // 写回一份：用户能直接打开 settings.json 改更新地址
    settings.save();
    let theme = settings.theme;
    // 在线更新的状态：只有用户点「检查更新」时才会去问更新服务器
    //
    // 故意**完全不做主动检测**：这个工具可能几年不更新，每次启动都去 GitHub
    // 问一次版本纯属浪费（还会把本机 IP 暴露给 GitHub）。平时它是纯离线的。
    let update_slot: Arc<std::sync::Mutex<crate::update::UpdateStatus>> =
        Arc::new(std::sync::Mutex::new(crate::update::UpdateStatus::Idle));
    // egui Context, captured when eframe creates the window; the WS note
    // client below uses it to repaint when remote content arrives.
    let ctx_holder: Arc<std::sync::Mutex<Option<eframe::egui::Context>>> =
        Arc::new(std::sync::Mutex::new(None));
    let ctx_holder_gui = ctx_holder.clone();
    // Number of transfers currently in flight (the status dot in the window).
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let active_gui = active.clone();
    // Number of paired devices (shown in the status line).
    let paired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let paired_gui = paired.clone();

    // The desktop app shares its state with the server in-process, so it
    // does not need to be an HTTP client at all. This thread only watches
    // the shared state and asks the window to repaint when something
    // changed (a note arrived, a transfer started, a device paired).
    {
        let cfg_watch = cfg.clone();
        let ctx_holder_watch = ctx_holder.clone();
        std::thread::spawn(move || {
            let gui_ctx = loop {
                if let Some(ctx) = ctx_holder_watch.lock().unwrap().clone() {
                    break ctx;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            let mut last_v = 0u64;
            let mut last_active = 0usize;
            let mut last_paired = 0usize;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let (_, v) = cfg_watch.read_note();
                let a = cfg_watch
                    .active_transfers
                    .load(std::sync::atomic::Ordering::SeqCst);
                let p = cfg_watch.session_count();
                if v != last_v || a != last_active || p != last_paired {
                    last_v = v;
                    last_active = a;
                    last_paired = p;
                    let _ = gui_ctx.request_repaint();
                }
            }
        });
    }

    // ---- 目录列表的后台刷新 -------------------------------------------
    //
    // `ops::list` 在 1 万文件的目录上要几十到上百毫秒，放在 UI 线程里每帧
    // 跑一遍会把界面拖卡。所以单独一个线程读目录，结果通过 mpsc 交回主线程
    // （和便签 / `/info` 的轮询是同一个模式）：3 秒节流，最小化时不刷，
    // 删改后可以立刻催一次。
    let (files_tx, files_rx) = std::sync::mpsc::channel::<crate::ops::Snapshot>();
    let refresh_now = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let window_visible = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let cfg_files = cfg.clone();
        let ctx_holder_files = ctx_holder.clone();
        let refresh_flag = refresh_now.clone();
        let visible_flag = window_visible.clone();
        std::thread::spawn(move || {
            let gui_ctx = loop {
                if let Some(ctx) = ctx_holder_files.lock().unwrap().clone() {
                    break ctx;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(10);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(300));
                let forced = refresh_flag.swap(false, std::sync::atomic::Ordering::SeqCst);
                let due = last.elapsed() >= std::time::Duration::from_secs(3);
                if !forced && !due {
                    continue;
                }
                if !visible_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    // 最小化：不刷，但别把 forced 丢了 —— 一恢复就立刻读
                    if forced {
                        refresh_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    continue;
                }
                last = std::time::Instant::now();
                let snap = crate::ops::list(&cfg_files);
                if files_tx.send(snap).is_err() {
                    return; // 窗口没了
                }
                let _ = gui_ctx.request_repaint();
            }
        });
    }

    // The self-signed certificate is created once in the background so the
    // window can show its fingerprint right away.
    {
        let cfg_tls = cfg.clone();
        std::thread::spawn(move || {
            let ips = crate::net::all_ipv4s();
            match crate::tls::load_or_create(&ips) {
                Ok(m) => cfg_tls.set_tls_material(m),
                Err(e) => eprintln!("tls: {e}"),
            }
        });
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(crate::http::server::serve(cfg_server));
    });

    let (screen_w, screen_h) = primary_monitor_size();
    let start_x = (screen_w - WIN_W) / 2.0;
    let start_y = (screen_h - WIN_H) / 2.0;

    let options = eframe::NativeOptions {
        // wgpu instead of the default glow/OpenGL backend: on an Intel iGPU with a
        // second (virtual) display adapter, the OpenGL path is what causes the
        // window to flicker on some machines. wgpu handles it correctly.
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("TransData")
            .with_inner_size(eframe::egui::vec2(WIN_W, WIN_H))
            // 无系统标题栏：自己画标题栏（图标 + 窗口按钮），避免深色系统下
            // 顶部那条突兀的黑边。
            .with_decorations(false)
            // 系统标题栏关掉后没有可拖的边框，改为自己实现边缘拖拽缩放
            // （见 GuiApp::handle_resize），所以这里保持 resizable = false，
            // 免得系统边框和自绘缩放打架。
            .with_resizable(false)
            .with_min_inner_size(eframe::egui::vec2(WIN_MIN_W, WIN_MIN_H))
            .with_icon(window_icon())
            .with_position(eframe::egui::Pos2::new(start_x, start_y)),
        ..Default::default()
    };

    eframe::run_native(
        "TransData",
        options,
        Box::new(move |cc| {
            *ctx_holder_gui.lock().unwrap() = Some(cc.egui_ctx.clone());
            // 主题跟着设置走（深浅两套配色在 pal::now() 里）
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals = match theme {
                crate::settings::Theme::Light => eframe::egui::Visuals::light(),
                crate::settings::Theme::Dark => eframe::egui::Visuals::dark(),
            };
            cc.egui_ctx.set_style(style);
            if let Some((bytes, index)) = cjk_font() {
                let font_data = eframe::egui::FontData {
                    font: std::borrow::Cow::Owned(bytes),
                    index,
                    tweak: eframe::egui::FontTweak::default(),
                };
                let mut definitions = eframe::egui::FontDefinitions::default();
                definitions.font_data.insert("cjk".to_owned(), font_data);
                definitions.families.insert(
                    eframe::egui::FontFamily::Proportional,
                    vec!["cjk".to_owned()],
                );
                cc.egui_ctx.set_fonts(definitions);
            }
            Ok(Box::new({
                #[cfg(windows)]
                {
                    // 窗口建好后按主显示器工作区重新居中（每显示器 DPI 下
                    // 初始位置容易算偏，窗口会开在屏幕外）
                    let hwnd = win_shape::find_main_window();
                    win_shape::center_on_primary(hwnd);
                }
                GuiApp::new(
                    cfg_gui,
                    port,
                    active_gui,
                    paired_gui,
                    settings,
                    update_slot,
                    files_rx,
                    refresh_now,
                    window_visible,
                    #[cfg(windows)]
                    win_shape::find_main_window(),
                )
            }))
        }),
    )
    .expect("eframe run failed");
}

struct GuiApp {
    cfg: Arc<Config>,
    port: u16,
    /// 8-digit pairing code (for devices that cannot scan the QR code).
    pair_code: String,
    /// Per-run pairing secret (embedded in the QR code).
    pair_secret: String,
    qr_handle: Option<eframe::egui::TextureHandle>,
    /// 标题栏左侧的应用图标（复用窗口图标那张 PNG）
    icon_handle: Option<eframe::egui::TextureHandle>,
    qr_dirty: bool,
    /// 二维码 / 配对码是否被隐藏（点眼睛按钮切换）。
    /// 远程协助 / 投屏时按一下，别人就扫不到码、也看不到数字。
    pair_hidden: bool,
    /// 窗口是否已最大化（自绘标题栏的按钮图标要用）
    maximized: bool,
    /// 配对码点击复制后的提示
    pair_msg: String,
    /// 窗口句柄（自己画的标题栏没有系统边框，这里用来裁圆角）
    #[cfg(windows)]
    hwnd: isize,
    /// 上次裁剪窗口区域时的客户区大小（变了才需要重新裁）
    #[cfg(windows)]
    shape_size: (i32, i32),
    /// 快捷文本输入框当前是否有焦点（决定边框颜色，避免 egui 默认的黑框）
    note_focused: bool,
    /// 便签是否铺满内容区（只存内存，重启回到收起态）
    note_expanded: bool,
    /// 便签提示是什么时候写的（10 秒后自动消失）
    note_status_at: Option<std::time::Instant>,
    /// 便签提示是不是警示（远端覆盖本地草稿）
    note_status_warn: bool,
    /// 服务器已经挂了（弹过窗了，别重复弹）
    server_dead_reported: bool,
    /// Cached `ipconfig` result — fetched once at startup and only when the
    /// transfer dir changes. Never re-run per frame (that spawned a visible
    /// cmd console window every second on some machines).
    ips: Vec<String>,
    /// 快捷文本：Some = 本地有未发送的编辑；None = 跟随共享文本。
    /// 远端内容到达时直接覆盖（note_local 被丢弃）。
    note_local: Option<String>,
    /// 编辑框里正在显示的内容。单独存一份，避免每帧克隆整段文本
    /// （打字时光标闪烁 60fps，长文本会被反复拷贝）。
    text_buf: String,
    note_last_v: u64,
    note_status: String,
    /// 正在进行的传输数量（由后台线程每 2 秒轮询 /info 更新）。
    active_transfers: Arc<std::sync::atomic::AtomicUsize>,
    /// 已配对的设备数量。
    paired_devices: Arc<std::sync::atomic::AtomicUsize>,
    /// 持久化设置（主题 / 更新源）
    settings: crate::settings::Settings,
    /// 设置面板是否打开
    settings_open: bool,
    /// 在线更新的状态（后台线程写，GUI 读）
    update: Arc<std::sync::Mutex<crate::update::UpdateStatus>>,
    /// 正在拖动窗口边缘改大小时的状态
    resize: Option<ResizeState>,

    // ---- v1.3 管理端 ------------------------------------------------------
    /// 后台线程送来的目录快照（`ops::list`，3 秒节流）
    files_rx: std::sync::mpsc::Receiver<crate::ops::Snapshot>,
    /// 最近一次拿到的目录快照
    files: crate::ops::Snapshot,
    /// 是否正在等第一次列表（工具栏显示「正在读取…」）
    files_loading: bool,
    /// 折叠的目录（只在内存里，重启清空；新出现的目录默认折叠）
    collapsed: std::collections::HashSet<String>,
    /// 已经见过的目录（判断哪些是新目录，好让它们默认折叠）
    known_dirs: std::collections::HashSet<String>,
    /// 选中的条目（相对路径）
    selected: Option<String>,
    /// 正在改名的那一行：路径 + 编辑中的文字
    renaming: Option<(String, String)>,
    /// 改名输入框刚打开时要「只选中主文件名」（第一帧做一次）
    rename_fresh: bool,
    /// 删除确认窗：路径 / 名字 / 是否目录 / 里面有几个项目
    confirm_delete: Option<ConfirmDelete>,
    /// 右栏显示的是哪台设备（点设备表切换）
    detail_ip: Option<String>,
    /// 底部状态栏的一句话（错误红字 / 操作结果）
    status_msg: String,
    status_is_err: bool,
    /// 这句话是什么时候写的（6 秒后让位给「已选中」）
    status_at: Option<std::time::Instant>,
    /// 窄窗口时右两栏的抽屉是否打开
    drawer_open: bool,
    /// 目录里一共有多少个文件夹 / 文件 / 总字节（状态栏用）
    counts: (usize, usize, u64),
    /// 叫后台线程立刻重读一次目录（删/改名后马上看到结果）
    refresh_now: Arc<std::sync::atomic::AtomicBool>,
    /// 窗口是否可见（最小化时后台不刷目录，省 CPU）
    window_visible: Arc<std::sync::atomic::AtomicBool>,
}

/// 删除确认窗要显示的东西
struct ConfirmDelete {
    rel: String,
    name: String,
    is_dir: bool,
    /// 目录里有多少个项目（文件 + 子目录），文件为 0
    items: usize,
}

/// 拖边缘改大小时记下的东西：起始光标位置 + 起始窗口矩形（物理像素）
#[derive(Clone, Copy)]
struct ResizeState {
    edge: ResizeEdge,
    start_cursor: (i32, i32),
    start_rect: (i32, i32, i32, i32),
}

/// 窗口的哪条边被拖住了
#[derive(Clone, Copy, PartialEq, Debug)]
enum ResizeEdge {
    Left,
    Right,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl GuiApp {
    fn new(
        cfg: Arc<Config>,
        port: u16,
        active_transfers: Arc<std::sync::atomic::AtomicUsize>,
        paired_devices: Arc<std::sync::atomic::AtomicUsize>,
        settings: crate::settings::Settings,
        update: Arc<std::sync::Mutex<crate::update::UpdateStatus>>,
        files_rx: std::sync::mpsc::Receiver<crate::ops::Snapshot>,
        refresh_now: Arc<std::sync::atomic::AtomicBool>,
        window_visible: Arc<std::sync::atomic::AtomicBool>,
        #[cfg(windows)] hwnd: isize,
    ) -> Self {
        let (_, last_v) = cfg.read_note();
        let pair_code = cfg.pair_code();
        let pair_secret = cfg.pair_secret();
        Self {
            cfg,
            port,
            pair_code,
            pair_secret,
            qr_handle: None,
            icon_handle: None,
            qr_dirty: true,
            pair_hidden: false,
            maximized: false,
            pair_msg: String::new(),
            note_focused: false,
            note_expanded: false,
            note_status_at: None,
            note_status_warn: false,
            server_dead_reported: false,
            #[cfg(windows)]
            hwnd,
            #[cfg(windows)]
            shape_size: (0, 0),
            ips: crate::net::all_ipv4s(),
            note_local: None,
            text_buf: String::new(),
            note_last_v: last_v,
            note_status: String::new(),
            active_transfers,
            paired_devices,
            settings,
            settings_open: false,
            update,
            resize: None,
            files_rx,
            files: crate::ops::Snapshot::default(),
            files_loading: true,
            collapsed: std::collections::HashSet::new(),
            known_dirs: std::collections::HashSet::new(),
            selected: None,
            renaming: None,
            rename_fresh: false,
            confirm_delete: None,
            detail_ip: None,
            status_msg: String::new(),
            status_is_err: false,
            status_at: None,
            drawer_open: false,
            counts: (0, 0, 0),
            refresh_now,
            window_visible,
        }
    }

    /// The address the phone has to open (protocol depends on HTTPS).
    fn base_url(&self) -> String {
        let scheme = if self.cfg.tls_on() { "https" } else { "http" };
        format!("{}://{}:{}", scheme, crate::net::display_ip(), self.port)
    }

    fn refresh_qr(&mut self, ctx: &eframe::egui::Context) {
        // The QR carries the pairing secret in the URL *fragment*: fragments
        // are never sent to the server, so the secret only reaches the
        // device that scans the code.
        let payload = format!("{}#k={}", self.base_url(), self.pair_secret);
        if let Ok(qr) = qrcode::QrCode::new(&payload) {
            // Keep the texture small: the card displays the QR at 132px, so
            // ~360px is already 2.7x supersampled. 60px/module produced a
            // 9900x9900 texture that exceeds wgpu's 8192 limit and panicked.
            let modules = qr.version().width() as u32;
            let scale = ((360u32) / modules.max(1)).clamp(1, 12);
            let buf = qr
                .render::<image::Rgba<u8>>()
                .module_dimensions(scale, scale)
                .quiet_zone(true)
                .build();
            let w: usize = buf.width() as usize;
            let h: usize = buf.height() as usize;
            let pixels: Vec<u8> = buf.into_vec();
            let img = eframe::egui::ColorImage::from_rgba_unmultiplied([w, h], &pixels);
            // Always reload: the payload changes when HTTPS is switched on
            // or off.
            let handle = ctx.load_texture("qr", img, eframe::egui::TextureOptions::default());
            self.qr_handle = Some(handle);
        }
        self.qr_dirty = false;
    }

    /// Switch HTTPS on or off. The server re-binds the port, so the
    /// address and the QR code change as well.
    fn set_tls(&mut self, on: bool) {
        if on && self.cfg.tls_material().is_none() {
            // No certificate yet (first run, or the file could not be
            // written): create one now.
            let ips = crate::net::all_ipv4s();
            match crate::tls::load_or_create(&ips) {
                Ok(m) => self.cfg.set_tls_material(m),
                Err(e) => {
                    eprintln!("tls: {e}");
                    return;
                }
            }
        }
        self.cfg.set_tls(on);
        self.qr_dirty = true;
    }

    fn set_dir(&mut self, d: PathBuf) {
        self.cfg.set_upload_dir(d);
        self.qr_dirty = true;
        self.ips = crate::net::all_ipv4s();
    }

    /// 自绘标题栏：图标 + 名称 + 运行状态 + 最小化/最大化/关闭。
    /// 系统标题栏已关闭（`with_decorations(false)`），所以这块是唯一的拖动区。
    fn draw_titlebar(&mut self, ctx: &eframe::egui::Context) {
        eframe::egui::TopBottomPanel::top("titlebar")
            .exact_height(TITLEBAR_H)
            .frame(
                eframe::egui::Frame::none()
                    .fill(pal::now().surface)
                    .inner_margin(eframe::egui::Margin::symmetric(10.0, 0.0)),
            )
            .show(ctx, |ui| {
                let bar = ui.max_rect();
                // 底部分隔线
                ui.painter().line_segment(
                    [
                        eframe::egui::pos2(bar.left(), bar.bottom()),
                        eframe::egui::pos2(bar.right(), bar.bottom()),
                    ],
                    eframe::egui::Stroke::new(1.0_f32, pal::now().card_border),
                );

                // 窗口按钮固定在右上角（绝对定位，不参与布局）
                // 从右往左：关闭 / 最大化 / 最小化 ‖ 设置 / 主题（月亮太阳）
                let btn_w = 36.0_f32;
                let btn_h = TITLEBAR_H - 2.0;
                let win_base_x = bar.right() - btn_w * 3.0;
                let gear_x = win_base_x - 10.0 - btn_w;
                let moon_x = gear_x - btn_w;
                let btn_y = bar.top() + 1.0;
                let rect_at = |x: f32| {
                    eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(x, btn_y),
                        eframe::egui::vec2(btn_w, btn_h),
                    )
                };

                // 左侧：图标 + 名称（垂直居中）
                ui.allocate_ui_at_rect(bar, |ui| {
                    ui.with_layout(eframe::egui::Layout::top_down(eframe::egui::Align::Min), |ui| {
                        // 18px 的图标行，上下各留一半空白，视觉上居中
                        ui.add_space((TITLEBAR_H - 20.0) * 0.5);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            if let Some(icon) = &self.icon_handle {
                                ui.add(
                                    eframe::egui::widgets::Image::from_texture(icon)
                                        .max_size(eframe::egui::vec2(18.0, 18.0)),
                                );
                            }
                            ui.label(
                                RichText::new("TransData")
                                    .size(14.0)
                                    .strong()
                                    .color(pal::now().fg),
                            );
                            ui.label(
                                RichText::new(format!("v{}", crate::update::APP_VERSION))
                                    .size(10.5)
                                    .color(pal::now().muted),
                            );
                        });
                    });
                });

                // 运行状态：右对齐到窗口按钮左侧
                let status_rect = eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(bar.left(), bar.top()),
                    eframe::egui::pos2(moon_x - 10.0, bar.bottom()),
                );
                ui.allocate_ui_at_rect(status_rect, |ui| {
                    ui.with_layout(
                        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            // 传输中：手机/浏览器在传文件时亮起来
                            let active = self
                                .active_transfers
                                .load(std::sync::atomic::Ordering::SeqCst);
                            if active > 0 {
                                ui.label(
                                    RichText::new(format!("● 传输中 ({active})"))
                                        .size(11.5)
                                        .color(pal::now().accent),
                                );
                            }
                            ui.label(
                                RichText::new("●").size(10.0).color(pal::now().green_dot),
                            );
                            let paired =
                                self.paired_devices.load(std::sync::atomic::Ordering::SeqCst);
                            let status = if paired > 0 {
                                format!("运行中 · 端口 {} · {paired} 台设备", self.port)
                            } else {
                                format!("运行中 · 端口 {}", self.port)
                            };
                            ui.label(RichText::new(status).size(11.5).color(pal::now().green));
                        },
                    );
                });

                // 窄窗口放不下三栏：右两栏收起，这里给一个抽屉入口
                let narrow = bar.width() < WIDE_MIN_W;
                let drawer_w = 78.0_f32;
                if narrow {
                    let devs = self.cfg.device_list().len();
                    let r = eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(moon_x - drawer_w - 6.0, btn_y),
                        eframe::egui::vec2(drawer_w, btn_h),
                    );
                    let resp = ui.interact(
                        r,
                        ui.id().with("device_drawer"),
                        eframe::egui::Sense::click(),
                    );
                    let hovered = resp.hovered();
                    if hovered {
                        ui.painter().rect_filled(r, 6.0, pal::now().btn_hover);
                    }
                    ui.painter().text(
                        r.center(),
                        eframe::egui::Align2::CENTER_CENTER,
                        format!("设备 ({devs})"),
                        eframe::egui::FontId::proportional(11.5),
                        if hovered {
                            pal::now().fg
                        } else {
                            pal::now().muted
                        },
                    );
                    if resp.clicked() {
                        self.drawer_open = !self.drawer_open;
                        ctx.request_repaint();
                    }
                }

                // 主题切换（现在深色，点一下变浅色）
                if theme_button(ui, rect_at(moon_x)) {
                    self.toggle_theme(ctx);
                }
                // 设置
                if gear_button(ui, rect_at(gear_x)) {
                    self.toggle_settings(ctx);
                }
                // 窗口按钮：关闭 / 最大化 / 最小化
                if win_button(
                    ui,
                    rect_at(win_base_x + btn_w * 2.0),
                    "close",
                    WinGlyph::Close,
                    true,
                ) {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                }
                let glyph = if self.maximized {
                    WinGlyph::Restore
                } else {
                    WinGlyph::Maximize
                };
                if win_button(ui, rect_at(win_base_x + btn_w), "maximize", glyph, false) {
                    self.toggle_maximize(ctx);
                }
                if win_button(ui, rect_at(win_base_x), "minimize", WinGlyph::Minimize, false) {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(true));
                }

                // 除按钮外整条都是拖动区（按住拖窗口，双击 = 最大化/还原）。
                // 注意：拖动区不能盖住「设备 (n)」抽屉按钮 —— egui 里后建的
                // 交互区在上面，会把按钮的点击吃掉。
                let drag_right = if narrow {
                    moon_x - drawer_w - 6.0
                } else {
                    moon_x
                };
                let drag_rect = eframe::egui::Rect::from_min_max(
                    bar.left_top(),
                    eframe::egui::pos2(drag_right, bar.bottom()),
                );
                let drag = ui.interact(
                    drag_rect,
                    ui.id().with("titlebar_drag"),
                    eframe::egui::Sense::click_and_drag(),
                );
                if drag.drag_started() {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    self.toggle_maximize(ctx);
                }
            });
    }

    fn toggle_maximize(&mut self, ctx: &eframe::egui::Context) {
        self.maximized = !self.maximized;
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Maximized(self.maximized));
        ctx.request_repaint();
    }

    /// 浅色 / 深色来回切
    fn toggle_theme(&mut self, ctx: &eframe::egui::Context) {
        let next = match crate::settings::theme() {
            crate::settings::Theme::Light => crate::settings::Theme::Dark,
            crate::settings::Theme::Dark => crate::settings::Theme::Light,
        };
        crate::settings::set_theme(next);
        self.settings.theme = next;
        self.settings.save();
        ctx.set_visuals(match next {
            crate::settings::Theme::Light => eframe::egui::Visuals::light(),
            crate::settings::Theme::Dark => eframe::egui::Visuals::dark(),
        });
        ctx.request_repaint();
    }

    /// 打开 / 关闭设置面板
    fn toggle_settings(&mut self, ctx: &eframe::egui::Context) {
        // 打开面板**不**联网：想看有没有新版本自己点「检查更新」
        self.settings_open = !self.settings_open;
        ctx.request_repaint();
    }

    /// 去更新服务器问一次版本号
    fn start_update_check(&mut self, ctx: &eframe::egui::Context) {
        let url = self.settings.update_url.clone();
        let slot = self.update.clone();
        let ctx = ctx.clone();
        {
            let mut g = slot.lock().unwrap();
            if matches!(*g, crate::update::UpdateStatus::Checking) {
                return;
            }
            *g = crate::update::UpdateStatus::Checking;
        }
        std::thread::spawn(move || {
            let result = match crate::update::fetch_release(&url) {
                Ok(rel) => {
                    if crate::update::is_newer(&rel.version, crate::update::APP_VERSION) {
                        crate::update::UpdateStatus::Available(rel)
                    } else {
                        crate::update::UpdateStatus::UpToDate(rel.version)
                    }
                }
                Err(e) if e == crate::update::NO_RELEASE => {
                    crate::update::UpdateStatus::UpToDate(
                        crate::update::APP_VERSION.to_string(),
                    )
                }
                Err(e) => crate::update::UpdateStatus::Failed(e),
            };
            *slot.lock().unwrap() = result;
            ctx.request_repaint();
        });
    }

    /// 下载新版本并替换掉正在运行的 exe
    fn start_update_download(&mut self, ctx: &eframe::egui::Context) {
        let rel = match &*self.update.lock().unwrap() {
            crate::update::UpdateStatus::Available(r) => r.clone(),
            _ => return,
        };
        let slot = self.update.clone();
        let ctx = ctx.clone();
        *slot.lock().unwrap() = crate::update::UpdateStatus::Downloading {
            received: 0,
            total: 0,
        };
        std::thread::spawn(move || {
            let tmp = crate::update::temp_download_path();
            let result = (|| -> Result<(), String> {
                crate::update::download(&rel.url, &tmp, |received, total| {
                    *slot.lock().unwrap() = crate::update::UpdateStatus::Downloading {
                        received,
                        total,
                    };
                    ctx.request_repaint();
                })?;
                crate::update::verify(&tmp, &rel.sha256)?;
                // sha256 是可选的（清单没写就跳过），所以再确认一次「这真的
                // 是个 exe」——否则一个哈希对得上的 zip 也会被装上去。
                crate::update::verify_pe(&tmp)?;
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                crate::update::replace_exe(&tmp, &exe)
            })();
            *slot.lock().unwrap() = match result {
                Ok(()) => crate::update::UpdateStatus::Ready,
                Err(e) => crate::update::UpdateStatus::Failed(e),
            };
            ctx.request_repaint();
        });
    }

    /// 设置面板：目前只有「关于」一项，顺带放上在线更新
    fn draw_settings(&mut self, ctx: &eframe::egui::Context) {
        let p = pal::now();

        let status = self.update.lock().unwrap().clone();
        let (latest, hint, tone) = match &status {
            crate::update::UpdateStatus::Idle => (
                "未检查".to_string(),
                "点「检查更新」看看有没有新版本".to_string(),
                p.muted,
            ),
            crate::update::UpdateStatus::Checking => (
                "检查中…".to_string(),
                "正在连接更新服务器".to_string(),
                p.muted,
            ),
            crate::update::UpdateStatus::UpToDate(v) => {
                (v.clone(), "已经是最新版本".to_string(), p.green)
            }
            crate::update::UpdateStatus::Available(r) => (
                r.version.clone(),
                if r.sha256.is_some() {
                    "发现新版本，可以直接更新".to_string()
                } else {
                    "发现新版本，但清单没给 sha256 —— 无法校验完整性".to_string()
                },
                p.accent,
            ),
            crate::update::UpdateStatus::Downloading { received, total } => {
                let pct = if *total > 0 {
                    (received * 100 / total).min(100)
                } else {
                    0
                };
                (
                    "下载中".to_string(),
                    format!("已下载 {pct}%"),
                    p.accent,
                )
            }
            crate::update::UpdateStatus::Ready => (
                crate::update::APP_VERSION.to_string(),
                "更新完成，重启后生效".to_string(),
                p.green,
            ),
            crate::update::UpdateStatus::Failed(e) => {
                ("—".to_string(), e.clone(), p.btn_hover_close)
            }
        };
        let notes = match &status {
            crate::update::UpdateStatus::Available(r) => r.notes.clone(),
            _ => None,
        };

        let mut open = self.settings_open;
        eframe::egui::Window::new("设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(
                eframe::egui::Align2::CENTER_CENTER,
                eframe::egui::vec2(0.0, 0.0),
            )
            .frame(
                eframe::egui::Frame::none()
                    .fill(p.surface)
                    .rounding(14.0)
                    .stroke(eframe::egui::Stroke::new(1.0_f32, p.card_border))
                    .inner_margin(eframe::egui::Margin::same(18.0)),
            )
            .show(ctx, |ui| {
                ui.set_width(340.0);

                // ---- 关于
                section_label(ui, "关于");
                ui.add_space(6.0);
                for (k, v) in about_rows(crate::update::APP_VERSION, &latest) {
                    kv_row(ui, &k, &v);
                }
                ui.add_space(4.0);
                ui.add(
                    eframe::egui::Label::new(
                        RichText::new(&hint).size(11.0).color(tone),
                    )
                    .wrap(),
                );
                if let Some(n) = &notes {
                    ui.add_space(6.0);
                    ui.add(
                        eframe::egui::Label::new(
                            RichText::new(n).size(11.0).color(p.muted),
                        )
                        .wrap(),
                    );
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if btn_soft(ui, "检查更新", 12.0, 30.0, 84.0) {
                        self.start_update_check(ctx);
                    }
                    match &status {
                        crate::update::UpdateStatus::Available(_) => {
                            if btn_primary(ui, "立即更新", 12.0, 30.0, 84.0) {
                                self.start_update_download(ctx);
                            }
                        }
                        crate::update::UpdateStatus::Ready => {
                            if btn_primary(ui, "立即重启", 12.0, 30.0, 84.0) {
                                crate::update::restart_self();
                            }
                        }
                        _ => {}
                    }
                });

                // ---- 配对：锁定状态 + 一键换码
                //
                // 输错太多次会进冷却，以前只能重启程序；现在这里能看到还剩
                // 多少秒，并且随时可以换一张新码立刻恢复。
                ui.add_space(16.0);
                section_label(ui, "配对");
                ui.add_space(6.0);
                kv_row(ui, "配对码", &self.pair_code);
                let (lock_text, lock_color) = match self.cfg.pair_lock_left() {
                    Some(secs) => (
                        format!("已锁定：{secs} 秒后可重试"),
                        p.btn_hover_close,
                    ),
                    None => (
                        "正常：在手机上扫码或输入上面的配对码".to_string(),
                        p.muted,
                    ),
                };
                ui.add(
                    eframe::egui::Label::new(
                        RichText::new(lock_text).size(11.0).color(lock_color),
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
                if btn_soft(ui, "重新生成配对码", 12.0, 30.0, 132.0) {
                    let (code, secret) = self.cfg.reset_pairing();
                    self.pair_code = code;
                    self.pair_secret = secret;
                    self.pair_hidden = false;
                    self.pair_msg = "配对码已更新".to_string();
                    self.refresh_qr(ctx);
                }
                ui.add(
                    eframe::egui::Label::new(
                        RichText::new("换码后已连接的设备不受影响，新设备要用新码")
                            .size(10.5)
                            .color(p.muted),
                    )
                    .wrap(),
                );
            });
        self.settings_open = open;
    }

    /// 拖窗口边缘改大小。
    ///
    /// 系统标题栏关掉了，没有系统边框可以拖，所以自己实现：光标靠近边缘时
    /// 换成缩放光标，按下鼠标后抓住鼠标，按位移算出新的窗口矩形。
    #[cfg(windows)]
    fn handle_resize(&mut self, ctx: &eframe::egui::Context) {
        if self.hwnd == 0 || self.maximized || win_shape::is_zoomed(self.hwnd) {
            return;
        }
        let Some((cx, cy)) = win_shape::cursor_pos() else {
            return;
        };
        let Some((l, t, r, b)) = win_shape::window_rect(self.hwnd) else {
            return;
        };
        let scale = win_shape::dpi(self.hwnd) as f32 / 96.0;
        let zone = 6.0 * scale;

        // 正在缩放：按光标位移算新矩形
        if let Some(st) = self.resize {
            let dx = cx - st.start_cursor.0;
            let dy = cy - st.start_cursor.1;
            let min_w = (WIN_MIN_W * scale) as i32;
            let min_h = (WIN_MIN_H * scale) as i32;
            let (left, top, right, bottom) =
                resize_rect(st.start_rect, st.edge, dx, dy, min_w, min_h);
            win_shape::set_window_rect(self.hwnd, left, top, right - left, bottom - top);

            // 松手就结束
            if ctx.input(|i| !i.pointer.any_down()) {
                win_shape::release_capture();
                self.resize = None;
            }
            return;
        }

        // 没在缩放：看光标是不是在边缘上
        if cx < l || cx >= r || cy < t || cy >= b {
            return;
        }
        let Some(edge) = detect_edge(cx - l, cy - t, r - l, b - t, zone) else {
            return;
        };
        ctx.set_cursor_icon(edge.cursor());
        let pressed = ctx.input(|i| {
            i.pointer.primary_pressed()
        });
        if pressed {
            win_shape::set_capture(self.hwnd);
            self.resize = Some(ResizeState {
                edge,
                start_cursor: (cx, cy),
                start_rect: (l, t, r, b),
            });
        }
    }
}

impl ResizeEdge {
    /// 拖这条边时左边会不会动
    fn moves_left(self) -> bool {
        matches!(
            self,
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
        )
    }

    fn moves_top(self) -> bool {
        matches!(self, ResizeEdge::TopLeft | ResizeEdge::TopRight)
    }

    fn cursor(self) -> eframe::egui::CursorIcon {
        use eframe::egui::CursorIcon;
        match self {
            ResizeEdge::Left | ResizeEdge::Right => CursorIcon::ResizeHorizontal,
            ResizeEdge::Bottom => CursorIcon::ResizeVertical,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorIcon::ResizeNwSe,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorIcon::ResizeNeSw,
        }
    }
}

/// 光标在窗口的哪条边上（坐标是相对窗口左上角，物理像素）
fn detect_edge(x: i32, y: i32, w: i32, h: i32, zone: f32) -> Option<ResizeEdge> {
    let left = x as f32 <= zone;
    let right = (w - x) as f32 <= zone;
    let top = y as f32 <= zone;
    let bottom = (h - y) as f32 <= zone;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (_, true, true, _) => Some(ResizeEdge::TopRight),
        (true, _, _, true) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Left),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, _, true) => Some(ResizeEdge::Bottom),
        _ => None,
    }
}

/// 拖边缩放：按光标位移算出新的窗口矩形
///
/// `start` 是起始矩形（左上右下），`dx/dy` 是光标位移。最小尺寸必须守住 ——
/// 不然窗口能被拖成一个点。
fn resize_rect(
    start: (i32, i32, i32, i32),
    edge: ResizeEdge,
    dx: i32,
    dy: i32,
    min_w: i32,
    min_h: i32,
) -> (i32, i32, i32, i32) {
    let (mut left, mut top, mut right, mut bottom) = start;
    match edge {
        ResizeEdge::Left => left += dx,
        ResizeEdge::Right => right += dx,
        ResizeEdge::Bottom => bottom += dy,
        ResizeEdge::TopLeft => {
            left += dx;
            top += dy;
        }
        ResizeEdge::TopRight => {
            right += dx;
            top += dy;
        }
        ResizeEdge::BottomLeft => {
            left += dx;
            bottom += dy;
        }
        ResizeEdge::BottomRight => {
            right += dx;
            bottom += dy;
        }
    }
    if right - left < min_w {
        if edge.moves_left() {
            left = right - min_w;
        } else {
            right = left + min_w;
        }
    }
    if bottom - top < min_h {
        if edge.moves_top() {
            top = bottom - min_h;
        } else {
            bottom = top + min_h;
        }
    }
    (left, top, right, bottom)
}

/// Split a SHA-256 fingerprint ("AA:BB:CC:…") so it fits on one line:
/// only the head is shown, the rest is copied to the clipboard on demand.
fn fingerprint_short(fp: &str) -> String {
    let pairs: Vec<&str> = fp.split(':').collect();
    if pairs.len() <= 8 {
        return fp.to_string();
    }
    format!("{}:…:{}", pairs[..4].join(":"), pairs[pairs.len() - 2..].join(":"))
}

// ---------------------------------------------------------------------------
// palette (浅色 / 深色两套，标题栏的月亮按钮随时切换)

mod pal {
    use eframe::egui::Color32;

    #[derive(Clone, Copy)]
    pub struct Palette {
        /// 窗口底色
        pub bg: Color32,
        /// 卡片 / 标题栏底色
        pub surface: Color32,
        pub card_border: Color32,
        pub fg: Color32,
        pub muted: Color32,
        pub accent: Color32,
        pub green: Color32,
        pub green_dot: Color32,
        pub soft_bg: Color32,
        pub soft_fg: Color32,
        /// 输入框未聚焦时的边框
        pub input_border: Color32,
        /// 窗口按钮悬停底色（关闭按钮是红的）
        pub btn_hover: Color32,
        pub btn_hover_close: Color32,
    }

    pub fn light() -> Palette {
        Palette {
            bg: Color32::from_rgb(0xF4, 0xF5, 0xF9),
            surface: Color32::WHITE,
            card_border: Color32::from_rgb(0xE3, 0xE5, 0xEE),
            fg: Color32::from_rgb(0x2A, 0x2E, 0x3F),
            muted: Color32::from_rgb(0x8A, 0x90, 0xA2),
            accent: Color32::from_rgb(0x5A, 0x6E, 0xE8),
            green: Color32::from_rgb(0x1E, 0x7A, 0x46),
            green_dot: Color32::from_rgb(0x34, 0xC7, 0x7B),
            soft_bg: Color32::from_rgb(0xEE, 0xF0, 0xF6),
            soft_fg: Color32::from_rgb(0x4A, 0x4F, 0x66),
            input_border: Color32::from_rgb(0xD8, 0xDC, 0xE6),
            btn_hover: Color32::from_rgb(0xE9, 0xEB, 0xF2),
            btn_hover_close: Color32::from_rgb(0xE5, 0x33, 0x2E),
        }
    }

    pub fn dark() -> Palette {
        Palette {
            bg: Color32::from_rgb(0x1B, 0x1E, 0x27),
            surface: Color32::from_rgb(0x25, 0x29, 0x34),
            card_border: Color32::from_rgb(0x35, 0x3A, 0x48),
            fg: Color32::from_rgb(0xE4, 0xE7, 0xEF),
            muted: Color32::from_rgb(0x96, 0x9C, 0xB0),
            accent: Color32::from_rgb(0x7C, 0x8D, 0xFF),
            green: Color32::from_rgb(0x55, 0xD3, 0x8B),
            green_dot: Color32::from_rgb(0x3A, 0xD1, 0x86),
            soft_bg: Color32::from_rgb(0x2E, 0x33, 0x40),
            soft_fg: Color32::from_rgb(0xC2, 0xC7, 0xD8),
            input_border: Color32::from_rgb(0x3C, 0x42, 0x52),
            btn_hover: Color32::from_rgb(0x33, 0x38, 0x47),
            btn_hover_close: Color32::from_rgb(0xE5, 0x33, 0x2E),
        }
    }

    /// 当前主题的配色（每帧取若干次，开销可以忽略）
    pub fn now() -> Palette {
        match crate::settings::theme() {
            crate::settings::Theme::Light => light(),
            crate::settings::Theme::Dark => dark(),
        }
    }
}

impl App for GuiApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut Frame) {
        self.draw(ctx);
    }
}

impl GuiApp {
    /// 记一条便签提示（10 秒后自动消失；warn 用警示色）。
    fn set_note_status(&mut self, msg: &str, warn: bool) {
        self.note_status = msg.to_string();
        self.note_status_at = Some(std::time::Instant::now());
        self.note_status_warn = warn;
    }

    /// 还应该显示的便签提示（过期就返回 None）。
    fn note_status_live(&self) -> Option<&str> {
        let at = self.note_status_at?;
        if at.elapsed() < NOTE_STATUS_TTL {
            Some(self.note_status.as_str())
        } else {
            None
        }
    }

    /// 展开/收起便签。展开等于离开列表，顺手把没提交的改名取消掉。
    fn set_note_expanded(&mut self, on: bool) {
        self.note_expanded = on;
        if on {
            self.renaming = None;
            self.rename_fresh = false;
        }
    }

    /// Esc 只处理一次：行内改名 > 删除确认 > 设置，处理完就把键消费掉。
    /// 改名排最前是因为 egui 自己会在 Esc 时丢掉焦点，若先关别的层，
    /// 改名框就会按「失焦」被提交——Esc 的语义应该是取消。
    /// 便签展开态故意不接 Esc（退出靠右上角按钮）。
    fn handle_escape(&mut self, ctx: &eframe::egui::Context) {
        if !ctx.input(|i| i.key_pressed(eframe::egui::Key::Escape)) {
            return;
        }
        ctx.input_mut(|i| {
            i.consume_key(eframe::egui::Modifiers::NONE, eframe::egui::Key::Escape);
        });
        if self.renaming.take().is_some() {
            self.rename_fresh = false;
        } else if self.confirm_delete.is_some() {
            self.confirm_delete = None;
        } else if self.settings_open {
            self.settings_open = false;
        }
    }

    /// Build one frame of the window. Kept separate from `App::update` so
    /// the layout can be rendered headlessly in a test.
    fn draw(&mut self, ctx: &eframe::egui::Context) {
        // 服务器没起来就别装正常：端口被占用时窗口会显示二维码和配对码，
        // 但手机根本连不上 —— 这种情况必须直接说清楚。
        // （只看服务器明确报出来的错，不能用"还没 running"去猜：GUI 的
        //   第一帧通常跑在服务器 bind 之前，那不是故障。）
        if let Some(err) = self.cfg.server_error() {
            if !self.server_dead_reported {
                self.server_dead_reported = true;
                show_error(&format!(
                    "服务器没能启动。\n\n{err}\n\n\
                     端口可能已经被另一个 TransData 占用了（重复启动？）。\n\
                     请关掉已经在运行的 TransData 再打开本程序。"
                ));
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            }
            return;
        }

        let (shared_text, shared_v) = self.cfg.read_note();

        // 拖边缘改大小（要在画界面之前处理，鼠标指针才换得及时）
        #[cfg(windows)]
        self.handle_resize(ctx);

        self.handle_escape(ctx);

        let refresh_now = self.qr_dirty;
        if self.qr_dirty {
            self.refresh_qr(ctx);
        }
        if self.icon_handle.is_none() {
            // 标题栏用的应用图标（复用窗口图标那张 PNG）
            self.icon_handle = load_icon_texture(ctx);
        }

        // Windows：把窗口裁成圆角（无边框窗口默认是直角）
        #[cfg(windows)]
        {
            if self.hwnd == 0 {
                self.hwnd = win_shape::find_main_window();
            }
            update_window_shape(self.hwnd, &mut self.shape_size);
        }

        // A newer version on the server wins: remote content overwrites the
        // local edit (两端同步规则，用户确认保持这个行为)。
        //
        // 但覆盖必须说出来：以前正在打字时手机发来一条，框里的内容被直接
        // 换掉，用户完全不知道自己的字去哪了。
        if shared_v != self.note_last_v {
            let had_unsent = self
                .note_local
                .as_deref()
                .map(|t| t != shared_text.as_str())
                .unwrap_or(false);
            self.note_last_v = shared_v;
            self.note_local = None;
            self.text_buf = shared_text.as_ref().clone();
            if had_unsent {
                self.set_note_status("已收到对方内容，本地未发送的改动被覆盖", true);
            } else {
                self.set_note_status("已收到对方内容", false);
            }
        } else if self.text_buf.is_empty() && self.note_local.is_none() {
            // First frame: show whatever the server already has.
            self.text_buf = shared_text.as_ref().clone();
        }

        // ---- 自绘标题栏（代替系统标题栏，深色系统下那条黑边就没了）
        self.draw_titlebar(ctx);

        self.draw_workspace(ctx);

        // 便签提示 10 秒后自动消失：窗口闲着也要能到点重绘，否则它会一直挂着
        if let Some(at) = self.note_status_at {
            let left = NOTE_STATUS_TTL.saturating_sub(at.elapsed());
            if !left.is_zero() {
                ctx.request_repaint_after(left);
            }
        }

        if refresh_now {
            // Only schedule a follow-up repaint when the display actually
            // changed (e.g. after switching the transfer dir). In steady state
            // the window stays idle — no constant full redraw, so no flicker.
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        // 设置面板浮在最上面
        if self.settings_open {
            self.draw_settings(ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// layout helpers


fn section_label(ui: &mut eframe::egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(pal::now().muted));
}

/// 设置面板里「关于」这几行的内容
///
/// 单独拆出来：版本号是这面板上最重要的信息，得能单独测。
fn about_rows(current: &str, latest: &str) -> Vec<(String, String)> {
    vec![
        ("应用".to_string(), "TransData".to_string()),
        ("当前版本".to_string(), current.to_string()),
        ("最新版本".to_string(), latest.to_string()),
    ]
}

/// 设置面板里的一行「名字 — 值」
fn kv_row(ui: &mut eframe::egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).size(12.0).color(pal::now().muted));
        ui.label(RichText::new(value).size(12.0).color(pal::now().fg));
    });
}

/// Accent-filled primary button.
fn btn_primary(ui: &mut eframe::egui::Ui, label: &str, size: f32, h: f32, w: f32) -> bool {
    ui.add(
        eframe::egui::Button::new(
            RichText::new(label)
                .size(size)
                .strong()
                .color(eframe::egui::Color32::WHITE),
        )
        .fill(pal::now().accent)
        .rounding(8.0)
        .min_size(eframe::egui::vec2(w, h)),
    )
    .clicked()
}

/// Soft gray secondary button.
fn btn_soft(ui: &mut eframe::egui::Ui, label: &str, size: f32, h: f32, w: f32) -> bool {
    ui.add(
        eframe::egui::Button::new(RichText::new(label).size(size).color(pal::now().soft_fg))
            .fill(pal::now().soft_bg)
            .rounding(8.0)
            .min_size(eframe::egui::vec2(w, h)),
    )
    .clicked()
}

/// Small text-only button (no background) — used next to labels.
fn btn_link(ui: &mut eframe::egui::Ui, label: &str) -> bool {
    ui.add(
        eframe::egui::Button::new(RichText::new(label).size(11.0).color(pal::now().accent))
            .fill(eframe::egui::Color32::TRANSPARENT)
            .stroke(eframe::egui::Stroke::NONE)
            .rounding(6.0)
            .min_size(eframe::egui::vec2(34.0, 18.0)),
    )
    .clicked()
}

/// 眼睛按钮（自己画，不依赖 emoji 字体）：显示 / 隐藏二维码与配对码。
/// 隐藏状态画一道斜杠，颜色也用主色提示「现在是不显示」。
fn eye_button(ui: &mut eframe::egui::Ui, hidden: bool) -> eframe::egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        eframe::egui::vec2(26.0, 22.0),
        eframe::egui::Sense::click(),
    );
    if resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, pal::now().btn_hover);
    }
    let c = rect.center();
    let col = if hidden {
        pal::now().accent
    } else {
        pal::now().muted
    };
    let (w, h) = (7.5_f32, 4.4_f32);
    let stroke = eframe::egui::Stroke::new(1.3_f32, col);
    // 上下两条弧线拼成杏仁形的眼睛轮廓
    let mut top = Vec::with_capacity(13);
    let mut bot = Vec::with_capacity(13);
    for i in 0..=12 {
        let x = -1.0 + 2.0 * (i as f32 / 12.0);
        let y = (1.0 - x * x).max(0.0).powf(0.62) * h;
        top.push(eframe::egui::pos2(c.x + x * w, c.y - y));
        bot.push(eframe::egui::pos2(c.x + x * w, c.y + y));
    }
    let mut pts = top;
    pts.extend(bot.into_iter().rev());
    let p = ui.painter();
    p.add(eframe::egui::Shape::closed_line(pts, stroke));
    p.circle_filled(c, 1.8, col);
    if hidden {
        let a = eframe::egui::pos2(c.x - w * 0.95, c.y + h * 1.7);
        let b = eframe::egui::pos2(c.x + w * 0.95, c.y - h * 1.7);
        // 先用卡片底色把眼睛「切开」，斜杠才看得清
        p.line_segment([a, b], eframe::egui::Stroke::new(3.2_f32, pal::now().surface));
        p.line_segment([a, b], eframe::egui::Stroke::new(1.4_f32, col));
    }
    resp
}

/// Load the app icon as a texture for the self-drawn title bar.
fn load_icon_texture(ctx: &eframe::egui::Context) -> Option<eframe::egui::TextureHandle> {
    let bytes = include_bytes!("../icon_32.png");
    let buf = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let img = eframe::egui::ColorImage::from_rgba_unmultiplied([w, h], &buf);
    Some(ctx.load_texture("app_icon", img, eframe::egui::TextureOptions::LINEAR))
}

/// The three window buttons of the self-drawn title bar.
enum WinGlyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// 标题栏的主题按钮：现在是浅色就画月亮（点了变深色），深色就画太阳。
fn theme_button(ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect) -> bool {
    let resp = ui.interact(
        rect,
        ui.id().with("theme_button"),
        eframe::egui::Sense::click(),
    );
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, pal::now().btn_hover);
    }
    let c = rect.center();
    let color = if resp.hovered() {
        pal::now().fg
    } else {
        pal::now().soft_fg
    };
    match crate::settings::theme() {
        crate::settings::Theme::Light => {
            // 月牙：外圆减去一个偏心的内圆
            let pts = crescent_points(c, 6.2);
            ui.painter().add(eframe::egui::Shape::convex_polygon(
                pts,
                color,
                eframe::egui::Stroke::NONE,
            ));
        }
        crate::settings::Theme::Dark => {
            // 太阳：一个圆加八根光线
            ui.painter().circle_filled(c, 3.2, color);
            let stroke = eframe::egui::Stroke::new(1.1_f32, color);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::PI / 4.0;
                let d = eframe::egui::vec2(a.cos(), a.sin());
                ui.painter()
                    .line_segment([c + d * 5.2, c + d * 7.6], stroke);
            }
        }
    }
    resp.clicked()
}

/// 月牙的轮廓点（外圆减掉一个偏心的内圆）
fn crescent_points(center: eframe::egui::Pos2, r: f32) -> Vec<eframe::egui::Pos2> {
    let r1 = r;
    // 内圆跟外圆差不多大、偏心一些，挖出来的月牙才细
    let r2 = r * 0.98;
    let d = r * 0.55;
    let a = (d * d - r2 * r2 + r1 * r1) / (2.0 * d);
    let h2 = r1 * r1 - a * a;
    if h2 <= 0.0 {
        return vec![center];
    }
    let h = h2.sqrt();
    let th = h.atan2(a);
    let phi = h.atan2(a - d);
    let n = 24;
    let mut pts = Vec::with_capacity((n + 1) * 2);
    // 外圈：从右上绕左边到右下
    for i in 0..=n {
        let t = th + (2.0 * std::f32::consts::PI - 2.0 * th) * (i as f32 / n as f32);
        pts.push(center + eframe::egui::vec2(r1 * t.cos(), -r1 * t.sin()));
    }
    // 内圈：从右下绕回来（反向），把月牙的肚子挖出来
    let t0 = 2.0 * std::f32::consts::PI - phi;
    for i in 0..=n {
        let t = t0 + (phi - t0) * (i as f32 / n as f32);
        pts.push(center + eframe::egui::vec2(d + r2 * t.cos(), -r2 * t.sin()));
    }
    pts
}

/// 标题栏的设置按钮（齿轮）
fn gear_button(ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect) -> bool {
    let resp = ui.interact(
        rect,
        ui.id().with("gear_button"),
        eframe::egui::Sense::click(),
    );
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, pal::now().btn_hover);
    }
    let c = rect.center();
    let color = if resp.hovered() {
        pal::now().fg
    } else {
        pal::now().soft_fg
    };
    // 齿轮：中间一个圆盘 + 八个方齿 + 中间的孔
    ui.painter().circle_filled(c, 4.4, color);
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::PI / 4.0;
        let (s, co) = (a.sin(), a.cos());
        let mid = c + eframe::egui::vec2(co, s) * 5.1;
        let half = 1.25;
        let corners = [
            eframe::egui::vec2(-half, -half),
            eframe::egui::vec2(half, -half),
            eframe::egui::vec2(half, half),
            eframe::egui::vec2(-half, half),
        ];
        let pts: Vec<eframe::egui::Pos2> = corners
            .iter()
            .map(|v| {
                mid + eframe::egui::vec2(
                    v.x * co - v.y * s,
                    v.x * s + v.y * co,
                )
            })
            .collect();
        ui.painter().add(eframe::egui::Shape::convex_polygon(
            pts,
            color,
            eframe::egui::Stroke::NONE,
        ));
    }
    // 中间的孔
    ui.painter().circle_filled(c, 1.7, pal::now().surface);
    resp.clicked()
}

/// Draw one window button (Windows-11-ish: no border, hover highlight,
/// close turns red). Returns true when clicked.
fn win_button(
    ui: &mut eframe::egui::Ui,
    rect: eframe::egui::Rect,
    id: &'static str,
    glyph: WinGlyph,
    is_close: bool,
) -> bool {
    let resp = ui.interact(rect, ui.id().with(id), eframe::egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            0.0,
            if is_close {
                pal::now().btn_hover_close
            } else {
                pal::now().btn_hover
            },
        );
    }
    let fg = if resp.hovered() && is_close {
        eframe::egui::Color32::WHITE
    } else {
        pal::now().soft_fg
    };
    let stroke = eframe::egui::Stroke::new(1.1_f32, fg);
    let c = rect.center();
    match glyph {
        WinGlyph::Minimize => {
            ui.painter().line_segment(
                [
                    eframe::egui::pos2(c.x - 5.0, c.y + 3.5),
                    eframe::egui::pos2(c.x + 5.0, c.y + 3.5),
                ],
                stroke,
            );
        }
        WinGlyph::Maximize => {
            let r = eframe::egui::Rect::from_center_size(
                c,
                eframe::egui::vec2(11.0, 11.0),
            );
            ui.painter().rect_stroke(r, 0.0, stroke);
        }
        WinGlyph::Restore => {
            let back = eframe::egui::Rect::from_center_size(
                c + eframe::egui::vec2(1.5, -1.5),
                eframe::egui::vec2(9.0, 9.0),
            );
            let front = eframe::egui::Rect::from_center_size(
                c + eframe::egui::vec2(-1.5, 1.5),
                eframe::egui::vec2(9.0, 9.0),
            );
            ui.painter().rect_stroke(back, 0.0, stroke);
            ui.painter().rect_stroke(front, 0.0, stroke);
        }
        WinGlyph::Close => {
            ui.painter().line_segment(
                [
                    c + eframe::egui::vec2(-4.5, -4.5),
                    c + eframe::egui::vec2(4.5, 4.5),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    c + eframe::egui::vec2(4.5, -4.5),
                    c + eframe::egui::vec2(-4.5, 4.5),
                ],
                stroke,
            );
        }
    }
    resp.clicked()
}

/// 「12 秒前 / 3 分钟前 / 2 小时前」
fn fmt_ago(secs: u64) -> String {
    if secs < 10 {
        "刚刚".to_string()
    } else if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        format!("{} 小时前", secs / 3600)
    }
}

/// 1024 进制的大小，跟网页上显示的一致
fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{:.0} {}", v, UNITS[i])
    }
}

fn elide(
    ctx: &eframe::egui::Context,
    text: &str,
    font: eframe::egui::FontId,
    max: f32,
) -> String {    let width_of = |s: &str| {
        ctx.fonts(|f| f.layout_delayed_color(s.to_string(), font.clone(), f32::INFINITY).rect.width())
    };
    if width_of(text) <= max {
        return text.to_string();
    }
    let mut out = text.to_string();
    while out.len() > 2 {
        out.pop();
        let cand = format!("{out}…");
        if width_of(&cand) <= max {
            return cand;
        }
    }
    "…".to_string()
}

fn open_in_explorer(dir: PathBuf) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe")
            .arg(dir.to_string_lossy().to_string())
            .spawn();
    }
}

/// Write `text` to the Windows system clipboard (CF_UNICODETEXT).
/// Retries briefly while another app holds the clipboard open.
#[cfg(windows)]
/// 复制到 Windows 剪贴板。返回是否成功（失败时调用方要如实提示，
/// 不能一律显示"已复制"）。
fn win32_set_clipboard(text: &str) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(hwnd: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(uFormat: u32, hMem: *const std::ffi::c_void) -> usize;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> *mut std::ffi::c_void;
        fn GlobalLock(hMem: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GlobalUnlock(hMem: *const std::ffi::c_void) -> i32;
        fn GlobalFree(hMem: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    // CF_UNICODETEXT 要求 UTF-16（每个码元 2 字节）+ 单个 NUL 结尾。
    // 注意：必须用 u16，不能用 u32 —— 用 u32 时每个码元占 4 字节槽位，
    // 后两位恒为 0，Windows 按 2 字节宽字符读到此处即视为字符串终止，
    // 结果只保留第一个字符。
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0u16); // NUL 结尾
    let byte_len = utf16.len() * std::mem::size_of::<u16>();

    // 最多重试 20 次、每次 10 ms：剪贴板被别的程序占住时不要把 UI 线程
    // 卡住一秒（原来是 50 ms × 20）。
    for _ in 0..20 {
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
        if hmem.is_null() {
            return false;
        }
        let ptr = unsafe { GlobalLock(hmem) };
        if ptr.is_null() {
            // 锁不住就别往下写：以前会拿着一个没锁住的内存块去
            // SetClipboardData，剪贴板里出现的是一段未初始化数据。
            unsafe {
                GlobalFree(hmem);
            }
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
            GlobalUnlock(hmem);
        }
        if unsafe { OpenClipboard(0) } == 0 {
            // Clipboard locked by another app: release this handle, wait, retry.
            unsafe {
                GlobalFree(hmem);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        unsafe {
            EmptyClipboard();
            let ok = SetClipboardData(CF_UNICODETEXT, hmem);
            CloseClipboard();
            if ok != 0 {
                return true; // success: the OS now owns hmem
            }
            GlobalFree(hmem);
        }
    }
    false
}

#[cfg(not(windows))]
fn win32_set_clipboard(_text: &str) -> bool {
    false
}

fn choose_dir() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("选择传输目录")
        .pick_folder()
}

// ---------------------------------------------------------------------------
// 快捷文本输入区
//
// egui 的多行 TextEdit 默认有三个毛病，这里逐个绕开：
//
// 1. 直角 + 聚焦时一圈深灰描边（就是你看到的那圈"黑线"）。
//    → 外面套一层自绘的圆角 Frame，TextEdit 本身 .frame(false)。
//      聚焦时把边框换成主色，而不是 egui 的深灰色。
//
// 2. 高度没有上限：TextEdit 的高度 = 文本排版后的高度。
//    一次粘贴长文本 → 输入框长到上千像素 → 按钮被顶出窗口（看不见了）。
//    → 外面套一层「最大高度固定」的 ScrollArea，超出部分在框内滚动，
//      输入框高度恒定，按钮位置永远不变。
//
// 3. 滚动条会占掉一点宽度，输入框右边和卡片边缘差那么一点。
//    → 圆角外框画在滚动区之外，宽度和卡片内容区完全一致。
//
// 另外自定义 layouter，允许在任意字符间断行（否则一整段没空格的长文本
// 会把左右两侧挤爆）。
/// 便签编辑器的固定 id：收起/展开切换后光标、选区还能续上。
fn note_editor_id() -> eframe::egui::Id {
    eframe::egui::Id::new("td_note_editor")
}

/// `max_height`：展开态的最大高度（收起态按两行字高算）；
/// `fill`：展开态用，输入框铺满可用高度；`right_inset`：右上角图标预留的宽度。
fn note_text_edit(
    ui: &mut eframe::egui::Ui,
    text: &mut String,
    max_height: f32,
    focused: &mut bool,
    fill: bool,
    right_inset: f32,
) -> eframe::egui::Response {
    let font_id = eframe::egui::TextStyle::Body.resolve(ui.style());
    let mut layouter = |ui: &eframe::egui::Ui, t: &str, wrap_width: f32| {
        let color = ui.visuals().widgets.inactive.text_color();
        let mut job = eframe::egui::text::LayoutJob::simple(
            t.to_string(),
            font_id.clone(),
            color,
            wrap_width,
        );
        job.wrap.break_anywhere = true; // 无空格长文本也能正常换行
        ui.fonts(|f| f.layout_job(job))
    };
    // 聚焦时用主色描边，替代 egui 默认的深灰（看着像黑框）
    let border = if *focused {
        pal::now().accent
    } else {
        pal::now().input_border
    };
    let mut resp = None;
    eframe::egui::Frame::none()
        .fill(pal::now().soft_bg)
        .rounding(10.0)
        .stroke(eframe::egui::Stroke::new(1.0_f32, border))
        .inner_margin(eframe::egui::Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| {
            // 收起态视口取整两行：固定 36px 不是整行数，第三行会露出半截，
            // 露出来的部分还会越过圆角边框，看着像文字溢出输入框。
            let max_height = if fill {
                max_height
            } else {
                let row_h = ui.fonts(|f| {
                    let job = eframe::egui::text::LayoutJob::simple(
                        " ".to_owned(),
                        font_id.clone(),
                        eframe::egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    f.layout_job(job)
                        .rows
                        .first()
                        .map(|r| r.height())
                        .unwrap_or(16.0)
                });
                2.0 * row_h
            };
            if !fill {
                // 默认内容裁剪会往外多放几个点，放出去的那截正好画到边框外
                ui.style_mut().visuals.clip_rect_margin = 0.0;
            }
            let mut area = eframe::egui::ScrollArea::vertical()
                // 两种形态分开记滚动位置：展开时不会继承便签条的小滚动偏移
                .id_source(if fill {
                    "note_scroll_full"
                } else {
                    "note_scroll_small"
                })
                .max_height(max_height)
                // ScrollArea 默认要求内容区至少 64pt 高（min_scrolled_size），
                // 不压下去的话整个输入框会比便签条还高、穿出卡片底部
                .min_scrolled_height(24.0)
                // 收起：按内容收缩；展开：铺满给定高度
                .auto_shrink([false, !fill]);
            if !fill {
                // 长文本时滚动条会画在展开按钮边上，看着像多出来的那根竖线。
                // 收起态没有看滚动位置的需求，直接不画（滚轮照常能滚）。
                area = area.scroll_bar_visibility(
                    eframe::egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                );
            }
            area.show(ui, |ui| {
                if fill {
                    // 先铺一层背景交互，后面的 TextEdit 会盖在它上面：
                    // 点正文空白也能直接聚焦，不用非得点中文字
                    let bg = ui.available_rect_before_wrap();
                    let bg_resp = ui.interact(
                        bg,
                        eframe::egui::Id::new("td_note_body"),
                        eframe::egui::Sense::click(),
                    );
                    if bg_resp.clicked() {
                        ui.ctx()
                            .memory_mut(|m| m.request_focus(note_editor_id()));
                    }
                }
                let r = ui.add(
                    eframe::egui::TextEdit::multiline(text)
                        .id(note_editor_id())
                        .frame(false)
                        // 收起态外框已经留了内边距，TextEdit 自带的 4/2 会再多一圈，
                        // 两行的视口就被顶掉一点，第二行又被切
                        .margin(if fill {
                            eframe::egui::Margin::symmetric(4.0, 2.0)
                        } else {
                            eframe::egui::Margin::ZERO
                        })
                        .desired_width((ui.available_width() - right_inset).max(80.0))
                        .desired_rows(if fill { 8 } else { 2 })
                        .layouter(&mut layouter),
                );
                *focused = r.has_focus();
                resp = Some(r);
            });
        });
    resp.expect("note text edit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    /// 眼睛按钮的两种状态各渲染一张，肉眼确认画得对不对：
    ///
    /// ```text
    /// cargo test render_pair_hidden -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn render_pair_hidden() {
        let cfg = std::sync::Arc::new(Config::new());
        let ctx = egui::Context::default();
        let mut style = (*ctx.style()).clone();
        style.visuals = eframe::egui::Visuals::light();
        ctx.set_style(style);
        if let Some((bytes, index)) = cjk_font() {
            let font_data = eframe::egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: eframe::egui::FontTweak::default(),
            };
            let mut definitions = eframe::egui::FontDefinitions::default();
            definitions.font_data.insert("cjk".to_owned(), font_data);
            definitions
                .families
                .insert(eframe::egui::FontFamily::Proportional, vec!["cjk".to_owned()]);
            ctx.set_fonts(definitions);
        }
        let mut app = GuiApp::new(
            cfg.clone(),
            9999,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            crate::settings::Settings::load(),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::update::UpdateStatus::Idle,
            )),
            test_files_rx(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            #[cfg(windows)]
            0,
        );
        app.pair_hidden = false;
        // 塞一台设备 + 两条操作记录，顺便确认这两行不会把卡片撑爆
        cfg.new_session("192.168.1.7:5000");
        cfg.log_activity(
            "192.168.1.7:5000",
            crate::config::ActKind::Upload,
            "会议纪要-2024-06-18-产品评审.docx",
            384211,
        );
        cfg.log_activity(
            "192.168.1.7:5000",
            crate::config::ActKind::Download,
            "a.txt",
            233,
        );
        render_png(&ctx, &mut app, "target/gui_pair_shown.png");
        app.pair_hidden = true;
        render_png(&ctx, &mut app, "target/gui_pair_hidden.png");
    }

    /// Render the window headlessly to `target/gui_render.png` so the layout
    /// can be inspected without a desktop session:
    ///
    /// ```text
    /// cargo test render_window_to_png -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn render_window_to_png() {
        let cfg = std::sync::Arc::new(Config::new());
        let ctx = egui::Context::default();
        let mut style = (*ctx.style()).clone();
        style.visuals = eframe::egui::Visuals::light();
        ctx.set_style(style);
        if let Some((bytes, index)) = cjk_font() {
            let font_data = eframe::egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: eframe::egui::FontTweak::default(),
            };
            let mut definitions = eframe::egui::FontDefinitions::default();
            definitions.font_data.insert("cjk".to_owned(), font_data);
            definitions
                .families
                .insert(eframe::egui::FontFamily::Proportional, vec!["cjk".to_owned()]);
            ctx.set_fonts(definitions);
        }
        let mut app = GuiApp::new(
            cfg.clone(),
            9999,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            crate::settings::Settings::load(),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::update::UpdateStatus::Idle,
            )),
            test_files_rx(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            #[cfg(windows)]
            0,
        );
        // Show the window with HTTPS switched on (the busiest layout).
        if let Ok(m) = crate::tls::load_or_create(&["127.0.0.1".to_string()]) {
            cfg.set_tls_material(m);
            cfg.set_tls(true);
            app.qr_dirty = true;
        }
        render_png(&ctx, &mut app, "target/gui_render.png");
    }

    /// 深色模式 + 设置面板长什么样
    #[test]
    #[ignore]
    fn render_dark_and_settings() {
        let ctx = egui::Context::default();
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        ctx.set_style(style);
        if let Some((bytes, index)) = cjk_font() {
            let font_data = eframe::egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: eframe::egui::FontTweak::default(),
            };
            let mut definitions = eframe::egui::FontDefinitions::default();
            definitions.font_data.insert("cjk".to_owned(), font_data);
            definitions
                .families
                .insert(egui::FontFamily::Proportional, vec!["cjk".to_owned()]);
            ctx.set_fonts(definitions);
        }
        crate::settings::set_theme(crate::settings::Theme::Dark);
        let mut app = GuiApp::new(
            std::sync::Arc::new(Config::new()),
            9999,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            crate::settings::Settings {
                theme: crate::settings::Theme::Dark,
                update_url: String::new(),
            },
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::update::UpdateStatus::Idle,
            )),
            test_files_rx(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            #[cfg(windows)]
            0,
        );
        render_png(&ctx, &mut app, "target/gui_dark.png");
        // 设置面板：浅色 + 有新版本的状态
        crate::settings::set_theme(crate::settings::Theme::Light);
        app.settings.theme = crate::settings::Theme::Light;
        *app.update.lock().unwrap() = crate::update::UpdateStatus::Available(
            crate::update::Release {
                version: "9.9.9".to_string(),
                url: "http://127.0.0.1:1/app.exe".to_string(),
                sha256: None,
                notes: Some("这是测试用的更新说明".to_string()),
            },
        );
        app.settings_open = true;
        render_png(&ctx, &mut app, "target/gui_settings.png");
        crate::settings::set_theme(crate::settings::Theme::Light);
    }

    /// 画一帧并渲染成 PNG
    ///
    /// 连画好几帧：设置面板是带淡入动画的，只画一帧的话它还没出现。
    fn render_png(ctx: &egui::Context, app: &mut GuiApp, path: &str) {
        render_png_size(ctx, app, path, WIN_W, WIN_H);
    }

    /// 指定窗口大小渲染一张 PNG
    fn render_png_size(
        ctx: &egui::Context,
        app: &mut GuiApp,
        path: &str,
        w: f32,
        h: f32,
    ) {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(w, h),
        ));
        let mut out = egui::FullOutput::default();
        for i in 0..20 {
            input.time = Some(i as f64 * 0.05);
            out = ctx.run(input.clone(), |ctx| {
                app.draw(ctx);
            });
        }
        render_shapes_to_png(ctx, out, path, w, h);
    }

    /// 把 egui 画出来的三角形光栅化成 PNG
    fn render_shapes_to_png(
        ctx: &egui::Context,
        out: egui::FullOutput,
        path: &str,
        w: f32,
        h: f32,
    ) {
        let shapes = ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut img = image::RgbaImage::from_pixel(
            w as u32,
            h as u32,
            image::Rgba([244, 245, 249, 255]),
        );
        for cp in shapes {
            let clip = cp.clip_rect;
            let (vertices, indices) = match cp.primitive {
                eframe::egui::epaint::Primitive::Mesh(m) => (m.vertices, m.indices),
                _ => continue,
            };
            for tri in indices.chunks(3) {
                if tri.len() < 3 {
                    continue;
                }
                let pts: Vec<eframe::egui::Pos2> = tri
                    .iter()
                    .map(|i| vertices[*i as usize].pos)
                    .collect();
                let col = vertices[tri[0] as usize].color;
                let min_x = pts.iter().map(|p| p.x).fold(f32::MAX, f32::min).floor() as i32;
                let max_x = pts.iter().map(|p| p.x).fold(f32::MIN, f32::max).ceil() as i32;
                let min_y = pts.iter().map(|p| p.y).fold(f32::MAX, f32::min).floor() as i32;
                let max_y = pts.iter().map(|p| p.y).fold(f32::MIN, f32::max).ceil() as i32;
                for y in min_y..=max_y {
                    for x in min_x..=max_x {
                        if x < 0 || y < 0 || x as u32 >= img.width() || y as u32 >= img.height() {
                            continue;
                        }
                        let p = eframe::egui::pos2(x as f32 + 0.5, y as f32 + 0.5);
                        if !clip.contains(p) {
                            continue;
                        }
                        // barycentric test
                        let (a, b, c) = (pts[0], pts[1], pts[2]);
                        let v0 = b - a;
                        let v1 = c - a;
                        let v2 = p - a;
                        let den = v0.x * v1.y - v1.x * v0.y;
                        if den.abs() < 1e-6 {
                            continue;
                        }
                        let u = (v2.x * v1.y - v1.x * v2.y) / den;
                        let v = (v0.x * v2.y - v2.x * v0.y) / den;
                        if u < 0.0 || v < 0.0 || u + v > 1.0 {
                            continue;
                        }
                        img.put_pixel(x as u32, y as u32, image::Rgba(col.to_array()));
                    }
                }
            }
        }
        img.save(path).unwrap();
        println!("saved {path}");
    }

    /// 真实窗口尺寸见模块顶部的 WIN_W / WIN_H

    /// 拖边缩放：边缘要认得对，最小尺寸也要守得住
    #[test]
    fn resize_edges_and_min_size() {
        // 800x600 的窗口，边缘感应宽度 6 像素
        let w = 800;
        let h = 600;
        let zone = 6.0;
        assert_eq!(detect_edge(2, 300, w, h, zone), Some(ResizeEdge::Left));
        assert_eq!(detect_edge(797, 300, w, h, zone), Some(ResizeEdge::Right));
        assert_eq!(detect_edge(400, 597, w, h, zone), Some(ResizeEdge::Bottom));
        assert_eq!(detect_edge(2, 2, w, h, zone), Some(ResizeEdge::TopLeft));
        assert_eq!(
            detect_edge(797, 2, w, h, zone),
            Some(ResizeEdge::TopRight)
        );
        assert_eq!(
            detect_edge(2, 597, w, h, zone),
            Some(ResizeEdge::BottomLeft)
        );
        // 中间不是边
        assert_eq!(detect_edge(400, 300, w, h, zone), None);

        // 拖右边 100 像素 → 宽 +100
        let start = (100, 100, 900, 700);
        assert_eq!(
            resize_rect(start, ResizeEdge::Right, 100, 0, 640, 470),
            (100, 100, 1000, 700)
        );

        // 拖左边 -50 → 左边往右挪，宽 +50
        assert_eq!(
            resize_rect(start, ResizeEdge::Left, -50, 0, 640, 470),
            (50, 100, 900, 700)
        );

        // 拖到比最小尺寸还小：守住下限
        let (l, _t, r, _b) = resize_rect(start, ResizeEdge::Right, -1000, 0, 640, 470);
        assert_eq!(r - l, 640, "宽度不能小于最小值");
        let (l, _t, r, _b) = resize_rect(start, ResizeEdge::Left, 1000, 0, 640, 470);
        assert_eq!(r - l, 640, "宽度不能小于最小值");
        let (_l, t, _r, b) = resize_rect(start, ResizeEdge::Bottom, 0, -1000, 640, 470);
        assert_eq!(b - t, 470, "高度不能小于最小值");
    }

    /// 窗口越变越小，底下的「复制 / 发送」按钮必须还在（不能被裁掉）
    #[test]
    #[ignore]
    fn render_small_sizes() {
        for (w, h) in [
            (WIN_W, WIN_H), // 打开时的默认尺寸（= 最小尺寸）
            (880.0_f32, 570.0),
            (880.0, 500.0),
            (880.0, 470.0), // 允许的最小高度
            (880.0, 440.0),
            (700.0, 470.0),
        ] {
            let ctx = egui::Context::default();
            let mut style = (*ctx.style()).clone();
            style.visuals = egui::Visuals::light();
            ctx.set_style(style);
            crate::settings::set_theme(crate::settings::Theme::Light);
            let mut app = GuiApp::new(
                std::sync::Arc::new(Config::new()),
                9999,
                std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
                crate::settings::Settings {
                    theme: crate::settings::Theme::Light,
                    update_url: String::new(),
                },
                std::sync::Arc::new(std::sync::Mutex::new(
                    crate::update::UpdateStatus::Idle,
                )),
                test_files_rx(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                #[cfg(windows)]
                0,
            );
            let path = format!("target/gui_{w}x{h}.png");
            render_png_size(&ctx, &mut app, &path, w, h);
            // 「发送」按钮是主色填充的：找主色像素，看它有没有被裁到窗外
            let img = image::open(&path).unwrap().to_rgba8();
            let accent = image::Rgb([0x5A, 0x6E, 0xE8]);
            let mut found = 0;
            let mut max_y = 0i32;
            for (x, y, px) in img.enumerate_pixels() {
                let [r, g, b, _] = px.0;
                if (r as i32 - accent[0] as i32).abs() < 12
                    && (g as i32 - accent[1] as i32).abs() < 12
                    && (b as i32 - accent[2] as i32).abs() < 12
                    && y > 60
                {
                    found += 1;
                    max_y = max_y.max(y as i32);
                }
            }
            println!(
                "{w}x{h}: 主色按钮像素 {found} 个, 最下到 y={max_y} (窗口高 {h}) {}",
                if max_y > 0 && (max_y as f32) < h - 4.0 {
                    "OK"
                } else {
                    "按钮被裁掉了!"
                }
            );
        }
    }

    /// 标题栏里月亮 / 齿轮按钮的位置（逻辑坐标，窗口左上角为原点）
    ///
    /// 和 draw_titlebar 里算的一样：窗口右边的三个是 最小化/最大化/关闭，
    /// 往左是 设置(齿轮) 和 主题(月亮)。
    fn titlebar_button_pos(which: &str) -> (f32, f32) {
        let btn_w = 36.0_f32;
        let win_base_x = WIN_W - btn_w * 3.0;
        let gear_x = win_base_x - 10.0 - btn_w;
        let moon_x = gear_x - btn_w;
        let y = 1.0 + (TITLEBAR_H - 2.0) / 2.0;
        match which {
            "moon" => (moon_x + btn_w / 2.0, y),
            "gear" => (gear_x + btn_w / 2.0, y),
            _ => (0.0, 0.0),
        }
    }

    /// 造一帧「鼠标移到 (x,y) 然后点一下」的输入
    fn click_input(x: f32, y: f32) -> egui::RawInput {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIN_W, WIN_H),
        ));
        input.events.push(egui::Event::PointerMoved(egui::pos2(x, y)));
        input.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        input.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        input
    }

    /// 一个不联网、不写文件的测试用窗口
    /// 测试用的目录通道：没有后台线程往里送，界面就显示「正在读取…」。
    fn test_files_rx() -> std::sync::mpsc::Receiver<crate::ops::Snapshot> {
        let (_tx, rx) = std::sync::mpsc::channel();
        std::mem::forget(_tx);
        rx
    }

    fn test_app(ctx: &egui::Context) -> GuiApp {
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::light();
        ctx.set_style(style);
        if let Some((bytes, index)) = cjk_font() {
            let font_data = eframe::egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: eframe::egui::FontTweak::default(),
            };
            let mut definitions = eframe::egui::FontDefinitions::default();
            definitions.font_data.insert("cjk".to_owned(), font_data);
            definitions
                .families
                .insert(egui::FontFamily::Proportional, vec!["cjk".to_owned()]);
            ctx.set_fonts(definitions);
        }
        GuiApp::new(
            std::sync::Arc::new(Config::new()),
            9999,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            crate::settings::Settings {
                theme: crate::settings::Theme::Light,
                update_url: String::new(),
            },
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::update::UpdateStatus::Idle,
            )),
            test_files_rx(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            #[cfg(windows)]
            0,
        )
    }

    /// 标题栏上的月亮（切深浅色）和齿轮（打开设置）必须真的能点
    #[test]
    fn titlebar_buttons_work() {
        let ctx = egui::Context::default();
        crate::settings::set_theme(crate::settings::Theme::Light);
        let mut app = test_app(&ctx);

        // 先画一帧，让布局稳定
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(WIN_W, WIN_H),
                )),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );

        // 点月亮 → 变深色
        let (mx, my) = titlebar_button_pos("moon");
        let _ = ctx.run(click_input(mx, my), |ctx| app.draw(ctx));
        assert_eq!(
            crate::settings::theme(),
            crate::settings::Theme::Dark,
            "点月亮应该切到深色"
        );

        // 点齿轮 → 打开设置面板
        let (gx, gy) = titlebar_button_pos("gear");
        let _ = ctx.run(click_input(gx, gy), |ctx| app.draw(ctx));
        assert!(app.settings_open, "点齿轮应该打开设置面板");

        // 复原（别把用户的设置改掉）
        crate::settings::set_theme(crate::settings::Theme::Light);
        app.settings.theme = crate::settings::Theme::Light;
        app.settings.save();
    }

    /// 设置面板里能看到版本号（关于）
    #[test]
    fn settings_panel_shows_version() {
        let rows = about_rows(crate::update::APP_VERSION, "9.9.9");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].0, "当前版本");
        assert_eq!(rows[1].1, crate::update::APP_VERSION);
        assert_eq!(rows[2].1, "9.9.9");

        // 面板能打开、也不崩
        let ctx = egui::Context::default();
        crate::settings::set_theme(crate::settings::Theme::Light);
        let mut app = test_app(&ctx);
        app.settings_open = true;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(WIN_W, WIN_H),
                )),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        assert!(app.settings_open);
    }

    /// 模拟一帧布局，返回 (输入框宽, 内容占位高, 按钮 x, 按钮 y)。
    /// `fixed` = 走修复后的 note_text_edit（限高 + 圆角外框 + 任意断行）。
    fn layout_probe(fixed: bool) -> (f32, f32, f32, f32) {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIN_W, WIN_H),
        ));
        // 一整段没有空格的长文本 —— 正是"一次粘贴长文本"的典型内容
        let mut text = "这是一段很长的中文文本用来测试粘贴长文本的效果。".repeat(200);
        let mut out = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                // 固定宽度的一块区域，模拟卡片内部：内容多高就占多高
                ui.allocate_ui_with_layout(
                    egui::vec2(600.0, 300.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let mut focused = false;
                        let resp = if fixed {
                            note_text_edit(ui, &mut text, 120.0, &mut focused, false, 0.0)
                        } else {
                            // 旧写法：TextEdit 直接放在布局流里，高度随文本无限增长
                            ui.add(
                                egui::TextEdit::multiline(&mut text)
                                    .desired_width(ui.available_width().max(80.0))
                                    .desired_rows(4),
                            )
                        };
                        // 输入框在父布局里实际占掉的高度（按钮还没占位时量）
                        let used_h = ui.min_rect().height();
                        let btn = ui.allocate_space(egui::vec2(88.0, 32.0)).1;
                        out = (resp.rect.width(), used_h, btn.min.x, btn.max.y);
                    },
                );
            });
        });
        out
    }

    /// 回归测试：一次粘贴长文本后，输入框不能暴涨，按钮必须留在窗口内。
    #[test]
    fn long_text_must_not_push_buttons_out_of_window() {
        let (w_old, h_old, bx_old, by_old) = layout_probe(false);
        let (w_new, h_new, bx_new, by_new) = layout_probe(true);
        println!(
            "旧写法: 宽 {w_old:.0} 占位高 {h_old:.0} 按钮 ({bx_old:.0},{by_old:.0}) / 新写法: 宽 {w_new:.0} 占位高 {h_new:.0} 按钮 ({bx_new:.0},{by_new:.0})"
        );

        // 旧写法：输入框高度 = 文本排版高度（上万像素），按钮被顶出窗口
        assert!(h_old > 500.0, "旧写法高度本应暴涨, 高 = {h_old}");
        assert!(by_old > WIN_H, "旧写法本应把按钮顶出窗口, y = {by_old}");

        // 修复后：输入框占位高度被限高卡住，按钮留在窗口内
        assert!(h_new <= 150.0, "输入框占位高度仍失控, 高 = {h_new}");
        assert!(w_new <= WIN_W, "输入框宽度仍失控, 宽 = {w_new}");
        assert!(by_new < WIN_H, "按钮仍被挤出窗口, y = {by_new}");
        assert!(bx_new < WIN_W, "按钮仍被挤出屏幕, x = {bx_new}");
    }

    // ---- v1.3 管理端：无头测试 --------------------------------------------

    /// 造一份目录快照（GUI 不碰文件系统，测试直接喂数据）
    fn fake_snapshot() -> crate::ops::Snapshot {
        use crate::ops::Entry;
        let mk = |rel: &str, is_dir: bool, size: u64| Entry {
            rel: rel.to_string(),
            is_dir,
            size,
            mtime: 1_790_000_000,
        };
        crate::ops::Snapshot {
            entries: vec![
                mk("归档", true, 0),
                mk("归档/2026-10 报告.pdf", false, 2_400_000),
                mk("备份", true, 0),
                mk("备份/旧照片.zip", false, 88_000_000),
                mk("会议记录.txt", false, 4096),
            ],
            truncated: false,
        }
    }

    /// 树：文件夹在前、按名排；折叠后子项消失；深度对得上。
    #[test]
    fn tree_rows_follow_collapse_state() {
        let snap = fake_snapshot();
        let mut collapsed = std::collections::HashSet::new();
        let rows = super::workspace::build_rows(&snap, &collapsed);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // 文件夹（归档、备份）在前，文件（会议记录.txt）在后
        assert_eq!(names, vec!["备份", "旧照片.zip", "归档", "2026-10 报告.pdf", "会议记录.txt"]);
        assert_eq!(rows[1].depth, 1, "子项要缩进一级");

        collapsed.insert("归档".to_string());
        let rows = super::workspace::build_rows(&snap, &collapsed);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(!names.contains(&"2026-10 报告.pdf"), "折叠后子项不该出现: {names:?}");
        assert!(names.contains(&"归档"));
    }

    /// 新目录默认折叠；用户展开过的不再被折回去，之后新增的目录仍然默认折叠。
    #[test]
    fn new_dirs_start_collapsed() {
        let snap = fake_snapshot();
        let mut known = std::collections::HashSet::new();
        let mut collapsed = std::collections::HashSet::new();
        super::workspace::fold_new_dirs(&snap, &mut known, &mut collapsed);
        assert!(collapsed.contains("归档") && collapsed.contains("备份"));
        assert!(!collapsed.contains("会议记录.txt"), "文件不参与折叠");

        // 用户展开「归档」
        collapsed.remove("归档");
        super::workspace::fold_new_dirs(&snap, &mut known, &mut collapsed);
        assert!(!collapsed.contains("归档"), "展开过的目录不能被折回去");

        // 之后新增的目录仍然默认折叠
        let mut snap2 = fake_snapshot();
        snap2.entries.push(crate::ops::Entry {
            rel: "新目录".to_string(),
            is_dir: true,
            size: 0,
            mtime: 0,
        });
        super::workspace::fold_new_dirs(&snap2, &mut known, &mut collapsed);
        assert!(collapsed.contains("新目录"));
    }

    /// 便签提示 10 秒后自动消失。
    #[test]
    fn note_status_expires() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.set_note_status("已复制到剪贴板", false);
        assert_eq!(app.note_status_live(), Some("已复制到剪贴板"));
        app.note_status_at = Some(
            std::time::Instant::now() - NOTE_STATUS_TTL - std::time::Duration::from_secs(1),
        );
        assert!(app.note_status_live().is_none());
    }

    /// Esc 一次只关一层：删除确认 > 设置 > 行内改名。
    #[test]
    fn escape_closes_only_one_layer() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.settings_open = true;
        app.confirm_delete = Some(ConfirmDelete {
            rel: "归档/2026-10 报告.pdf".to_string(),
            name: "2026-10 报告.pdf".to_string(),
            is_dir: false,
            items: 0,
        });
        app.renaming = Some(("归档/2026-10 报告.pdf".to_string(), "新名字".to_string()));
        app.rename_fresh = true;
        app.files = fake_snapshot();
        app.files_loading = false;

        let esc = || {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(WIN_W, WIN_H),
            ));
            input.events.push(egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            input
        };

        let _ = ctx.run(esc(), |ctx| app.draw(ctx));
        assert!(app.renaming.is_none(), "第一下取消改名");
        assert!(!app.rename_fresh);
        assert!(!app.status_msg.starts_with("已改名"), "Esc 不能把改名提交掉");
        assert!(app.confirm_delete.is_some(), "同一帧不该顺手关确认窗");
        assert!(app.settings_open, "同一帧不该顺手关设置");

        let _ = ctx.run(esc(), |ctx| app.draw(ctx));
        assert!(app.confirm_delete.is_none(), "第二下关确认窗");
        assert!(app.settings_open, "同一帧不该顺手关设置");

        let _ = ctx.run(esc(), |ctx| app.draw(ctx));
        assert!(!app.settings_open, "第三下关设置");
    }

    /// 展开便签时把没提交的行内改名取消掉（保留原名）。
    #[test]
    fn expanding_note_cancels_pending_rename() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.renaming = Some(("归档/2026-10 报告.pdf".to_string(), "新名字".to_string()));
        app.rename_fresh = true;
        app.set_note_expanded(true);
        assert!(app.note_expanded);
        assert!(app.renaming.is_none(), "展开后不该留着改名框");
        assert!(!app.rename_fresh);
        app.set_note_expanded(false);
        assert!(!app.note_expanded);
    }

    /// 1200×760 三栏能画出来（出图人工核对）；700×470 走抽屉降级。
    #[test]
    fn management_view_renders_and_narrow_uses_drawer() {
        let ctx = egui::Context::default();
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::light();
        ctx.set_style(style);
        crate::settings::set_theme(crate::settings::Theme::Light);

        let mut app = test_app(&ctx);
        app.files = fake_snapshot();
        app.files_loading = false;
        app.selected = Some("归档/2026-10 报告.pdf".to_string());
        app.rename_fresh = true;
        // 两台设备：一台允许、一台禁止（看开关的两种状态）
        app.cfg.new_session("192.168.1.23:5000");
        app.cfg.new_session("192.168.1.57:6000");
        app.cfg.set_device_write("192.168.1.57", false);
        app.cfg.log_activity(
            "192.168.1.23:5000",
            crate::config::ActKind::Upload,
            "归档/2026-10 报告.pdf",
            2_400_000,
        );
        app.cfg.log_activity(
            "192.168.1.23:5000",
            crate::config::ActKind::Download,
            "备份/旧照片.zip",
            88_000_000,
        );
        app.detail_ip = Some("192.168.1.23".to_string());
        render_png_size(&ctx, &mut app, "target/gui_manage_1200x760.png", 1200.0, 760.0);
        assert!(std::path::Path::new("target/gui_manage_1200x760.png").exists());

        // 便签铺开：三栏让位，编辑器占满内容区（出图人工核对）
        app.set_note_expanded(true);
        app.text_buf = "这是一段用来检查展开态排版的长文本。".repeat(12);
        app.set_note_status("已发送，另一端将自动填入", false);
        render_png_size(&ctx, &mut app, "target/gui_note_expanded.png", 1200.0, 760.0);
        assert!(std::path::Path::new("target/gui_note_expanded.png").exists());
        // 收起态 + 提示：提示挤在同一行里，便签条高度不变（出图人工核对）
        app.set_note_expanded(false);
        app.text_buf.clear();
        render_png_size(&ctx, &mut app, "target/gui_note_status.png", 1200.0, 760.0);
        assert!(std::path::Path::new("target/gui_note_status.png").exists());
        app.note_status_at = None;
        app.note_status.clear();

        // 窄窗口：右两栏收起，标题栏出现「设备 (n)」，点一下开抽屉
        let (w, h) = (700.0_f32, 470.0_f32);
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h)));
        for i in 0..20 {
            input.time = Some(i as f64 * 0.05);
            let _ = ctx.run(input.clone(), |ctx| app.draw(ctx));
        }
        render_png_size(&ctx, &mut app, "target/gui_manage_700x470.png", w, h);
        assert!(!app.drawer_open, "默认不展开抽屉");

        // 抽屉按钮在主题按钮左边：bar.right() = w - 10
        let btn_w = 36.0_f32;
        let win_base_x = (w - 10.0) - btn_w * 3.0;
        let gear_x = win_base_x - 10.0 - btn_w;
        let moon_x = gear_x - btn_w;
        let cx = moon_x - 78.0 - 6.0 + 39.0;
        let cy = 1.0 + (TITLEBAR_H - 2.0) / 2.0;
        let mut click = egui::RawInput::default();
        click.screen_rect = Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h)));
        click.events.push(egui::Event::PointerMoved(egui::pos2(cx, cy)));
        for pressed in [true, false] {
            click.events.push(egui::Event::PointerButton {
                pos: egui::pos2(cx, cy),
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            });
        }
        click.time = Some(2.0);
        let _ = ctx.run(click, |ctx| app.draw(ctx));
        assert!(app.drawer_open, "窄窗口点「设备 (n)」应该打开详情抽屉");
    }

    /// 删除确认窗：目录要说清楚「里面的 N 个项目会一起删除」。
    #[test]
    fn delete_confirm_counts_children() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx);
        app.files = fake_snapshot();
        app.files_loading = false;
        let row = super::workspace::build_rows(&app.files, &app.collapsed)
            .into_iter()
            .find(|r| r.rel == "备份")
            .expect("备份 目录要在列表里");
        app.ask_delete(&row);
        let c = app.confirm_delete.as_ref().expect("确认窗要打开");
        assert!(c.is_dir);
        assert_eq!(c.items, 1, "备份 里只有一个文件");
        app.confirm_delete = None;
    }

}
