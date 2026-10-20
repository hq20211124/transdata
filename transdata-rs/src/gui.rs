use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::RichText;
use eframe::{App, Frame};

use crate::config::Config;

/// 主窗口尺寸（固定不可缩放，测试也按这个尺寸模拟布局）
const WIN_W: f32 = 880.0;
const WIN_H: f32 = 570.0;
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

pub fn run_gui(cfg: Arc<Config>, port: u16) {
    let cfg_server = cfg.clone();
    let cfg_gui = cfg.clone();
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
            .with_resizable(false)
            .with_min_inner_size(eframe::egui::vec2(WIN_W, WIN_H))
            .with_icon(window_icon())
            .with_position(eframe::egui::Pos2::new(start_x, start_y)),
        ..Default::default()
    };

    eframe::run_native(
        "TransData",
        options,
        Box::new(move |cc| {
            *ctx_holder_gui.lock().unwrap() = Some(cc.egui_ctx.clone());
            // Force the light theme regardless of the OS dark mode: the design is
            // light (white cards on gray). Without this, dark-mode systems render
            // the TextEdit with a near-black background.
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals = eframe::egui::Visuals::light();
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
}

impl GuiApp {
    fn new(
        cfg: Arc<Config>,
        port: u16,
        active_transfers: Arc<std::sync::atomic::AtomicUsize>,
        paired_devices: Arc<std::sync::atomic::AtomicUsize>,
        #[cfg(windows)] hwnd: isize,
    ) -> Self {
        let (_, last_v) = cfg.read_note();
        let pair_code = cfg.pair_code.clone();
        let pair_secret = cfg.pair_secret.clone();
        Self {
            cfg,
            port,
            pair_code,
            pair_secret,
            qr_handle: None,
            icon_handle: None,
            qr_dirty: true,
            maximized: false,
            pair_msg: String::new(),
            note_focused: false,
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
                    .fill(eframe::egui::Color32::WHITE)
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
                    eframe::egui::Stroke::new(1.0_f32, pal::CARD_BORDER),
                );

                // 窗口按钮固定在右上角（绝对定位，不参与布局）
                let btn_w = 36.0_f32;
                let btn_h = TITLEBAR_H - 2.0;
                let btn_base_x = bar.right() - btn_w * 3.0;
                let btn_y = bar.top() + 1.0;
                let rect_of = |i: usize| {
                    eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(btn_base_x + btn_w * i as f32, btn_y),
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
                                    .color(pal::FG),
                            );
                        });
                    });
                });

                // 运行状态：右对齐到窗口按钮左侧
                let status_rect = eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(bar.left(), bar.top()),
                    eframe::egui::pos2(btn_base_x - 10.0, bar.bottom()),
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
                                        .color(pal::ACCENT),
                                );
                            }
                            ui.label(
                                RichText::new("●").size(10.0).color(pal::GREEN_DOT),
                            );
                            let paired =
                                self.paired_devices.load(std::sync::atomic::Ordering::SeqCst);
                            let status = if paired > 0 {
                                format!("运行中 · 端口 {} · 已配对 {paired} 台", self.port)
                            } else {
                                format!("运行中 · 端口 {}", self.port)
                            };
                            ui.label(RichText::new(status).size(11.5).color(pal::GREEN));
                        },
                    );
                });

                // 窗口按钮：关闭 / 最大化 / 最小化
                if win_button(ui, rect_of(2), "close", WinGlyph::Close, true) {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                }
                let glyph = if self.maximized {
                    WinGlyph::Restore
                } else {
                    WinGlyph::Maximize
                };
                if win_button(ui, rect_of(1), "maximize", glyph, false) {
                    self.toggle_maximize(ctx);
                }
                if win_button(ui, rect_of(0), "minimize", WinGlyph::Minimize, false) {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(true));
                }

                // 除按钮外整条都是拖动区（按住拖窗口，双击 = 最大化/还原）
                let drag_rect = eframe::egui::Rect::from_min_max(
                    bar.left_top(),
                    eframe::egui::pos2(btn_base_x, bar.bottom()),
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
// palette (light theme)

mod pal {
    pub const BG: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xF4, 0xF5, 0xF9);
    pub const CARD_BORDER: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xE3, 0xE5, 0xEE);
    pub const FG: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x2A, 0x2E, 0x3F);
    pub const MUTED: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x8A, 0x90, 0xA2);
    pub const ACCENT: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x5A, 0x6E, 0xE8);
    pub const GREEN: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x1E, 0x7A, 0x46);
    pub const GREEN_DOT: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x34, 0xC7, 0x7B);
    pub const SOFT_BG: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xEE, 0xF0, 0xF6);
    pub const SOFT_FG: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0x4A, 0x4F, 0x66);
    /// 输入框未聚焦时的边框
    pub const INPUT_BORDER: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xD8, 0xDC, 0xE6);
    /// 窗口按钮悬停底色（关闭按钮是红的）
    pub const BTN_HOVER: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xE9, 0xEB, 0xF2);
    pub const BTN_HOVER_CLOSE: eframe::egui::Color32 = eframe::egui::Color32::from_rgb(0xE5, 0x33, 0x2E);
}

impl App for GuiApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut Frame) {
        self.draw(ctx);
    }
}

impl GuiApp {
    /// Build one frame of the window. Kept separate from `App::update` so
    /// the layout can be rendered headlessly in a test.
    fn draw(&mut self, ctx: &eframe::egui::Context) {
        let url = self.base_url();
        let (shared_text, shared_v) = self.cfg.read_note();

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
        // local edit (两端同步规则).
        if shared_v != self.note_last_v {
            self.note_last_v = shared_v;
            self.note_local = None;
            self.text_buf = shared_text.as_ref().clone();
            self.note_status = "已收到对方内容".to_string();
        } else if self.text_buf.is_empty() && self.note_local.is_none() {
            // First frame: show whatever the server already has.
            self.text_buf = shared_text.as_ref().clone();
        }

        // ---- 自绘标题栏（代替系统标题栏，深色系统下那条黑边就没了）
        self.draw_titlebar(ctx);

        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::none()
                    .fill(pal::BG)
                    .inner_margin(eframe::egui::Margin::symmetric(18.0, 12.0)),
            )
            .show(ctx, |ui| {

                // ---- Main: left (地址 + 传输目录) | right (手机扫码), centered
                ui.with_layout(
                    eframe::egui::Layout::left_to_right(eframe::egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;

                        // A nested ui.vertical in a horizontal layout grabs ALL
                        // remaining width, which squeezed the QR column to 0 and
                        // hid it. Constrain each column by allocating an explicit
                        // max-sized ui instead.
                        let right_w = 186.0_f32;
                        let left_w = (ui.available_width() - right_w - 12.0).max(240.0);
                        let max_h = ui.available_height();

                        // Left column
                        ui.allocate_ui_with_layout(
                            eframe::egui::vec2(left_w, max_h),
                            eframe::egui::Layout::top_down(eframe::egui::Align::Min),
                            |ui| {
                                card(ui, |ui| {
                                ui.horizontal(|ui| {
                                    section_label(ui, "访问地址");
                                    ui.with_layout(
                                        eframe::egui::Layout::right_to_left(
                                            eframe::egui::Align::Center,
                                        ),
                                        |ui| {
                                            if btn_soft(ui, "复制", 11.0, 24.0, 52.0) {
                                                ctx.copy_text(url.clone());
                                            }
                                        },
                                    );
                                });
                                ui.add_space(6.0);
                                let url_font = eframe::egui::FontId::monospace(13.0);
                                let shown = elide(
                                    ctx,
                                    &url,
                                    url_font.clone(),
                                    ui.available_width().max(80.0),
                                );
                                ui.label(RichText::new(shown).font(url_font).color(pal::ACCENT));
                                // Hotspot / second NIC: the default-route IP may not be
                                // the one the phone can reach — list all IPs.
                                if self.ips.len() > 1 {
                                    ui.add_space(4.0);
                                    let tip = format!(
                                        "全部IP：{}（热点用 192.168.137.1）",
                                        self.ips.join("、")
                                    );
                                    let tip_font = eframe::egui::FontId::proportional(11.0);
                                    // 关键信息：不截断，超长换行全量显示（窗口已加宽配合）
                                    ui.add(
                                        eframe::egui::Label::new(
                                            RichText::new(tip)
                                                .font(tip_font)
                                                .color(pal::MUTED),
                                        )
                                        .wrap(),
                                    );
                                }

                                // ---- HTTPS 开关
                                ui.add_space(10.0);
                                let mut tls_on = self.cfg.tls_on();
                                let resp = ui.checkbox(
                                    &mut tls_on,
                                    RichText::new("启用 HTTPS（加密传输）")
                                        .size(12.5)
                                        .color(pal::FG),
                                );
                                if resp.changed() {
                                    self.set_tls(tls_on);
                                    ctx.request_repaint();
                                }
                                if self.cfg.tls_on() {
                                    ui.add_space(4.0);
                                    ui.add(
                                        eframe::egui::Label::new(
                                            RichText::new(
                                                "手机首次打开会提示一次「证书不受信任」：Chrome 点「高级 → 继续前往」，Safari 点「显示详细信息 → 访问此网站」。",
                                            )
                                            .size(10.5)
                                            .color(pal::MUTED),
                                        )
                                        .wrap(),
                                    );
                                    if let Some(m) = self.cfg.tls_material() {
                                        ui.add_space(4.0);
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("证书指纹")
                                                    .size(10.5)
                                                    .color(pal::MUTED),
                                            );
                                            let fp_font = eframe::egui::FontId::monospace(10.0);
                                            ui.label(
                                                RichText::new(fingerprint_short(&m.fingerprint))
                                                    .font(fp_font)
                                                    .color(pal::SOFT_FG),
                                            );
                                            if btn_link(ui, "复制") {
                                                ctx.copy_text(m.fingerprint.clone());
                                            }
                                        });
                                    }
                                } else {
                                    ui.add(
                                        eframe::egui::Label::new(
                                            RichText::new("不开启时为明文 HTTP，同一网络内可被旁观。")
                                                .size(10.5)
                                                .color(pal::MUTED),
                                        )
                                        .wrap(),
                                    );
                                }
                            });
                            ui.add_space(12.0);

                            card(ui, |ui| {
                                ui.horizontal(|ui| {
                                    section_label(ui, "传输目录");
                                    ui.with_layout(
                                        eframe::egui::Layout::right_to_left(
                                            eframe::egui::Align::Center,
                                        ),
                                        |ui| {
                                            let open_hit = btn_soft(ui, "打开", 11.0, 24.0, 52.0);
                                            let chg_hit = btn_primary(ui, "更改", 11.0, 24.0, 52.0);
                                            if open_hit {
                                                open_in_explorer(self.cfg.upload_dir());
                                            }
                                            if chg_hit {
                                                if let Some(d) = choose_dir() {
                                                    self.set_dir(d);
                                                }
                                            }
                                        },
                                    );
                                });
                                ui.add_space(6.0);
                                let dir_str = self.cfg.upload_dir().to_string_lossy().to_string();
                                let font = eframe::egui::FontId::proportional(12.0);
                                ui.label(
                                    RichText::new(elide(
                                        ctx,
                                        &dir_str,
                                        font.clone(),
                                        ui.available_width().max(60.0),
                                    ))
                                    .font(font)
                                    .color(pal::FG),
                                );
                                ui.add_space(6.0);
                                ui.add(
                                    eframe::egui::Label::new(
                                        RichText::new(
                                            "关闭窗口即停止服务器；「打开」进入目录，「更改」切换位置",
                                        )
                                        .size(11.0)
                                        .color(pal::MUTED)
                                        .weak(),
                                    )
                                    .wrap(),
                                );
                            });
                            },
                        );
                        // NOTE: no ui.add_space here — the horizontal layout's
                        // item_spacing.x (12) already provides the gap. Adding
                        // another 12px pushed the right column 12px past the
                        // panel edge and clipped the QR card.

                        // Right column：只留「扫码连接 或用配对码」+ 二维码 + 配对码
                        ui.allocate_ui_with_layout(
                            eframe::egui::vec2(right_w, max_h),
                            eframe::egui::Layout::top_down(eframe::egui::Align::Min),
                            |ui| {
                                card(ui, |ui| {
                                ui.label(
                                    RichText::new("扫码连接  或用配对码")
                                        .size(12.0)
                                        .color(pal::MUTED),
                                );
                                ui.add_space(8.0);
                                ui.vertical_centered(|ui| {
                                    if let Some(handle) = &self.qr_handle {
                                        eframe::egui::Frame::none()
                                            .fill(eframe::egui::Color32::WHITE)
                                            .rounding(10.0)
                                            .stroke(eframe::egui::Stroke::new(
                                                1.0_f32,
                                                pal::CARD_BORDER,
                                            ))
                                            .inner_margin(eframe::egui::Margin::same(8.0))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    eframe::egui::widgets::Image::from_texture(
                                                        handle,
                                                    )
                                                    .max_size(eframe::egui::vec2(
                                                        118.0_f32, 118.0_f32,
                                                    )),
                                                );
                                            });
                                    }
                                });
                                ui.add_space(10.0);
                                // 配对码：给不方便扫码的设备（点一下复制）
                                let mut code_rect = eframe::egui::Rect::NOTHING;
                                ui.vertical_centered(|ui| {
                                    let row = ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("配对码")
                                                .size(11.0)
                                                .color(pal::MUTED),
                                        );
                                        ui.label(
                                            RichText::new(&self.pair_code)
                                                .size(20.0)
                                                .strong()
                                                .monospace()
                                                .color(pal::FG),
                                        );
                                    });
                                    code_rect = row.response.rect;
                                });
                                let code_resp = ui.interact(
                                    code_rect,
                                    ui.id().with("pair_code"),
                                    eframe::egui::Sense::click(),
                                );
                                if code_resp.clicked() {
                                    ctx.copy_text(self.pair_code.clone());
                                    self.pair_msg = "配对码已复制".to_string();
                                }
                                if !self.pair_msg.is_empty() {
                                    ui.add_space(2.0);
                                    ui.label(
                                        RichText::new(&self.pair_msg)
                                            .size(10.5)
                                            .color(pal::MUTED),
                                    );
                                }
                            });
                        },
                    );
                    },
                );
                ui.add_space(12.0);

                // ---- Quick-note card: 贴住窗口底部，共享文本框与手机页实时同步
                // 复制 = 复制框内文字到本机剪贴板；发送 = 推给另一端（自动填入）。
                // 远端内容总是覆盖本地编辑。
                // 卡片高度 = 剩余空间，输入框把剩余高度吃掉，窗口底部不留空白。
                let leftover = ui.available_height();
                // 输入框吃掉剩余空间（窗口最大化时输入框变高，中间不留灰洞）
                let input_h = (leftover - 92.0).clamp(88.0, 360.0);
                ui.with_layout(
                    eframe::egui::Layout::bottom_up(eframe::egui::Align::Min),
                    |ui| {
                    // 卡片内部要恢复成从上到下（bottom_up 会把里面的顺序也倒过来）
                    ui.with_layout(
                        eframe::egui::Layout::top_down(eframe::egui::Align::Min),
                        |ui| {
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            section_label(ui, "快捷文本");
                            ui.with_layout(
                                eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new("自动同步 · 不限长度")
                                            .size(11.0)
                                            .color(pal::MUTED),
                                    );
                                },
                            );
                        });
                        ui.add_space(8.0);
                        // 圆角输入框：聚焦时边框变主色，不再是 egui 默认那圈黑线
                        let resp = note_text_edit(
                            ui,
                            &mut self.text_buf,
                            input_h,
                            &mut self.note_focused,
                        );
                        let changed = resp.changed;
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let copy_hit = btn_soft(ui, "复制", 13.0, 32.0, 88.0);
                            // right_to_left：先放的「发送」贴最右，再放小提示 → 提示在按钮左侧
                            let send_hit = ui
                                .with_layout(
                                    eframe::egui::Layout::right_to_left(
                                        eframe::egui::Align::Center,
                                    ),
                                    |ui| {
                                        let hit = btn_primary(ui, "发送", 13.0, 32.0, 88.0);
                                        ui.label(
                                            RichText::new("Ctrl+Enter 可快捷发送")
                                                .size(11.0)
                                                .color(pal::MUTED),
                                        );
                                        hit
                                    },
                                )
                                .inner;
                            if copy_hit {
                                win32_set_clipboard(&self.text_buf);
                                self.note_local = None;
                                self.note_status = "已复制到剪贴板".to_string();
                            }
                            // Ctrl+Enter 快捷键直接发送（顺带吞掉该按键，避免 TextEdit 插入换行）
                            let ctrl_enter = ctx.input(|i| {
                                i.key_pressed(eframe::egui::Key::Enter)
                                    && i.modifiers.contains(eframe::egui::Modifiers::CTRL)
                            });
                            if ctrl_enter {
                                let _ = ctx.input_mut(|i| {
                                    i.consume_key(eframe::egui::Modifiers::CTRL, eframe::egui::Key::Enter)
                                });
                            }
                            if send_hit || ctrl_enter {
                                let v = self.cfg.store_note(&self.text_buf);
                                self.note_local = None;
                                self.note_last_v = v;
                                self.note_status = "已发送，另一端将自动填入".to_string();
                            }
                        });
                        if changed {
                            // 只有真的敲了字才留一份本地副本（每帧拷贝大文本是浪费）
                            self.note_local = Some(self.text_buf.clone());
                        }
                        if !self.note_status.is_empty() {
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(&self.note_status).size(11.0).color(pal::MUTED),
                            );
                        }
                    });
                        },
                    );
                    },
                );

            });

        if refresh_now {
            // Only schedule a follow-up repaint when the display actually
            // changed (e.g. after switching the transfer dir). In steady state
            // the window stays idle — no constant full redraw, so no flicker.
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
}

// ---------------------------------------------------------------------------
// layout helpers

/// White rounded card with a 1px border and bottom spacing.
fn card(ui: &mut eframe::egui::Ui, add: impl FnOnce(&mut eframe::egui::Ui)) {
    eframe::egui::Frame::none()
        .fill(eframe::egui::Color32::WHITE)
        .rounding(12.0)
        .stroke(eframe::egui::Stroke::new(1.0_f32, pal::CARD_BORDER))
        .inner_margin(eframe::egui::Margin::same(14.0))
        .outer_margin(eframe::egui::Margin {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 12.0,
        })
        .show(ui, add);
}

fn section_label(ui: &mut eframe::egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(pal::MUTED));
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
        .fill(pal::ACCENT)
        .rounding(8.0)
        .min_size(eframe::egui::vec2(w, h)),
    )
    .clicked()
}

/// Soft gray secondary button.
fn btn_soft(ui: &mut eframe::egui::Ui, label: &str, size: f32, h: f32, w: f32) -> bool {
    ui.add(
        eframe::egui::Button::new(RichText::new(label).size(size).color(pal::SOFT_FG))
            .fill(pal::SOFT_BG)
            .rounding(8.0)
            .min_size(eframe::egui::vec2(w, h)),
    )
    .clicked()
}

/// Small text-only button (no background) — used next to labels.
fn btn_link(ui: &mut eframe::egui::Ui, label: &str) -> bool {
    ui.add(
        eframe::egui::Button::new(RichText::new(label).size(11.0).color(pal::ACCENT))
            .fill(eframe::egui::Color32::TRANSPARENT)
            .stroke(eframe::egui::Stroke::NONE)
            .rounding(6.0)
            .min_size(eframe::egui::vec2(34.0, 18.0)),
    )
    .clicked()
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
                pal::BTN_HOVER_CLOSE
            } else {
                pal::BTN_HOVER
            },
        );
    }
    let fg = if resp.hovered() && is_close {
        eframe::egui::Color32::WHITE
    } else {
        pal::SOFT_FG
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

/// Truncate `text` with a trailing ellipsis so it fits within `max` pixels.
fn elide(
    ctx: &eframe::egui::Context,
    text: &str,
    font: eframe::egui::FontId,
    max: f32,
) -> String {
    let width_of = |s: &str| {
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
fn win32_set_clipboard(text: &str) {
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

    for _ in 0..20 {
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
        if hmem.is_null() {
            return;
        }
        let ptr = unsafe { GlobalLock(hmem) };
        if !ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
                GlobalUnlock(hmem);
            }
        }
        if unsafe { OpenClipboard(0) } == 0 {
            // Clipboard locked by another app: release this handle, wait, retry.
            unsafe {
                GlobalFree(hmem);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        unsafe {
            EmptyClipboard();
            let ok = SetClipboardData(CF_UNICODETEXT, hmem);
            CloseClipboard();
            if ok != 0 {
                return; // success: the OS now owns hmem
            }
            GlobalFree(hmem);
        }
    }
}

#[cfg(not(windows))]
fn win32_set_clipboard(_text: &str) {}

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
fn note_text_edit(
    ui: &mut eframe::egui::Ui,
    text: &mut String,
    max_height: f32,
    focused: &mut bool,
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
        pal::ACCENT
    } else {
        pal::INPUT_BORDER
    };
    let mut resp = None;
    eframe::egui::Frame::none()
        .fill(pal::SOFT_BG)
        .rounding(10.0)
        .stroke(eframe::egui::Stroke::new(1.0_f32, border))
        .inner_margin(eframe::egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            eframe::egui::ScrollArea::vertical()
                .id_source("note_text_edit_scroll")
                .max_height(max_height)
                // 高度固定：内容再长也不把下面的按钮顶走
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let r = ui.add(
                        eframe::egui::TextEdit::multiline(text)
                            .frame(false)
                            .desired_width(ui.available_width().max(80.0))
                            .desired_rows(4)
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
            #[cfg(windows)]
            0,
        );
        // Show the window with HTTPS switched on (the busiest layout).
        if let Ok(m) = crate::tls::load_or_create(&["127.0.0.1".to_string()]) {
            cfg.set_tls_material(m);
            cfg.set_tls(true);
            app.qr_dirty = true;
        }
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIN_W, WIN_H),
        ));
        let out = ctx.run(input, |ctx| {
            app.draw(ctx);
        });
        let _ = &out;
        let shapes = ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut img = image::RgbaImage::from_pixel(
            WIN_W as u32,
            WIN_H as u32,
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
        img.save("target/gui_render.png").unwrap();
        println!("saved target/gui_render.png");
    }

    /// 真实窗口尺寸见模块顶部的 WIN_W / WIN_H

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
                            note_text_edit(ui, &mut text, 120.0, &mut focused)
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

}
