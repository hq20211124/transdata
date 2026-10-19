use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::RichText;
use eframe::{App, CreationContext, Frame};

use crate::config::Config;

/// 主窗口尺寸（固定不可缩放，测试也按这个尺寸模拟布局）
const WIN_W: f32 = 860.0;
const WIN_H: f32 = 560.0;

/// Load Microsoft YaHei (微软雅黑) so Chinese text renders instead of tofu
/// boxes. Falls back to SimSun, then nothing (app still runs).
fn cjk_font() -> Option<(Vec<u8>, u32)> {
    for (path, index) in [
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
fn primary_monitor_size() -> (f32, f32) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn GetSystemMetrics(index: i32) -> i32;
        }
        unsafe {
            let w = GetSystemMetrics(0) as f32;
            let h = GetSystemMetrics(1) as f32;
            if w > 0.0 && h > 0.0 {
                return (w, h);
            }
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
    let ctx_holder_thread = ctx_holder.clone();
    let ctx_holder_gui = ctx_holder.clone();
    let port_thread = port;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        // Wait until the GUI context exists, then start the live note client.
        let gui_ctx = loop {
            if let Some(ctx) = ctx_holder_thread.lock().unwrap().clone() {
                break ctx;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        rt.spawn(ws_note_client(gui_ctx, port_thread));
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
            .with_resizable(false)
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
            Ok(Box::new(GuiApp::new(cc, cfg_gui, port)))
        }),
    )
    .expect("eframe run failed");
}

struct GuiApp {
    cfg: Arc<Config>,
    port: u16,
    qr_img: Option<eframe::egui::ColorImage>,
    qr_handle: Option<eframe::egui::TextureHandle>,
    qr_dirty: bool,
    /// Cached `ipconfig` result — fetched once at startup and only when the
    /// transfer dir changes. Never re-run per frame (that spawned a visible
    /// cmd console window every second on some machines).
    ips: Vec<String>,
    /// 快捷文本：Some = 本地有未发送的编辑；None = 跟随共享文本。
    /// 远端内容到达时直接覆盖（note_local 被丢弃）。
    note_local: Option<String>,
    note_last_v: u64,
    note_status: String,
}

impl GuiApp {
    fn new(_cc: &CreationContext, cfg: Arc<Config>, port: u16) -> Self {
        let (_, last_v) = cfg.read_note();
        Self {
            cfg,
            port,
            qr_img: None,
            qr_handle: None,
            qr_dirty: true,
            ips: crate::net::all_ipv4s(),
            note_local: None,
            note_last_v: last_v,
            note_status: String::new(),
        }
    }

    fn refresh_qr(&mut self, ctx: &eframe::egui::Context) {
        let url = format!("http://{}:{}", crate::net::display_ip(), self.port);
        if let Ok(qr) = qrcode::QrCode::new(&url) {
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
            if self.qr_handle.is_none() {
                self.qr_img = Some(img.clone());
                let handle = ctx.load_texture("qr", img, eframe::egui::TextureOptions::default());
                self.qr_handle = Some(handle);
            }
        }
        self.qr_dirty = false;
    }

    fn set_dir(&mut self, d: PathBuf) {
        self.cfg.set_upload_dir(d);
        self.qr_dirty = true;
        self.ips = crate::net::all_ipv4s();
    }
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
}

impl App for GuiApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut Frame) {
        let url = format!("http://{}:{}", crate::net::display_ip(), self.port);
        let (shared_text, shared_v) = self.cfg.read_note();

        let refresh_now = self.qr_dirty;
        if self.qr_dirty {
            self.refresh_qr(ctx);
        }

        let mut text = self.note_local.clone().unwrap_or(shared_text);

        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::none()
                    .fill(pal::BG)
                    .inner_margin(eframe::egui::Margin::symmetric(18.0, 14.0)),
            )
            .show(ctx, |ui| {
                // ---- Header: title left, status right
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("文件传输服务器")
                            .size(18.0)
                            .strong()
                            .color(pal::FG),
                    );
                    ui.with_layout(
                        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new("●").size(10.0).color(pal::GREEN_DOT),
                                );
                                ui.label(
                                    RichText::new(format!("运行中 · 端口 {}", self.port))
                                        .size(12.0)
                                        .color(pal::GREEN),
                                );
                            });
                        },
                    );
                });
                ui.add_space(12.0);

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
                            eframe::egui::Layout::top_down(eframe::egui::Align::Center),
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

                        // Right column
                        ui.allocate_ui_with_layout(
                            eframe::egui::vec2(right_w, max_h),
                            eframe::egui::Layout::top_down(eframe::egui::Align::Center),
                            |ui| {
                                card(ui, |ui| {
                                ui.horizontal(|ui| {
                                    section_label(ui, "手机扫码");
                                    ui.with_layout(
                                        eframe::egui::Layout::right_to_left(
                                            eframe::egui::Align::Center,
                                        ),
                                        |ui| {
                                            ui.label(
                                                RichText::new("或输入IP地址")
                                                    .size(11.0)
                                                    .color(pal::MUTED),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(10.0);
                                ui.vertical_centered(|ui| {
                                    if let Some(handle) = &self.qr_handle {
                                        eframe::egui::Frame::none()
                                            .fill(eframe::egui::Color32::WHITE)
                                            .rounding(10.0)
                                            .stroke(eframe::egui::Stroke::new(
                                                1.0_f32,
                                                pal::CARD_BORDER,
                                            ))
                                            .inner_margin(eframe::egui::Margin::same(10.0))
                                            .show(ui, |ui| {
                                                ui.add(
                                                    eframe::egui::widgets::Image::from_texture(
                                                        handle,
                                                    )
                                                    .max_size(eframe::egui::vec2(
                                                        132.0_f32, 132.0_f32,
                                                    )),
                                                );
                                            });
                                    }
                                });
                                ui.add_space(6.0);
                                ui.add(
                                    eframe::egui::Label::new(
                                        RichText::new(
                                            "手机连电脑热点（或同一局域网）后扫码访问",
                                        )
                                        .size(11.0)
                                        .color(pal::MUTED),
                                    )
                                    .wrap(),
                                );
                            });
                        },
                    );
                    },
                );
                ui.add_space(12.0);

                // ---- Quick-note card (full width): shared text box, live-synced
                // with the phone page. 复制 = copy box content to this machine's
                // clipboard; 发送 = push to the other side (auto-fills it).
                // Remote content always overwrites local edits.
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
                    let before = text.clone();
                    // 固定最大高度 + 框内滚动：粘贴长文本也不会把下面的按钮顶出窗口
                    note_text_edit(ui, &mut text, 120.0);
                    let changed = text != before;
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
                            win32_set_clipboard(&text);
                            self.note_local = None;
                            self.note_last_v = shared_v;
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
                            let t = text.clone();
                            let v = self.cfg.store_note(&t);
                            self.note_local = None;
                            self.note_last_v = v;
                            self.note_status = "已发送，另一端将自动填入".to_string();
                        }
                    });
                    if changed {
                        self.note_local = Some(text.clone());
                    }
                    // Remote content always overwrites local edits.
                    if shared_v != self.note_last_v {
                        self.note_last_v = shared_v;
                        self.note_local = None;
                        self.note_status = "已收到对方内容".to_string();
                    }
                    if !self.note_status.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(&self.note_status).size(11.0).color(pal::MUTED),
                        );
                    }
                });

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

/// Live note client: stays connected to our own /ws endpoint and repaints the
/// GUI whenever remote note content arrives. Reconnects automatically.
async fn ws_note_client(ctx: eframe::egui::Context, port: u16) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    loop {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws")).await {
            Ok((mut ws, _)) => {
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(WsMsg::Text(_) | WsMsg::Binary(_)) => {
                            let _ = ctx.request_repaint();
                        }
                        Ok(WsMsg::Ping(p)) => {
                            let _ = ws.send(WsMsg::Pong(p)).await;
                        }
                        Ok(WsMsg::Close(_)) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(e) => {
                eprintln!("note ws client: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn choose_dir() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("选择传输目录")
        .pick_folder()
}

// ---------------------------------------------------------------------------
// 快捷文本输入区
//
// egui 的多行 TextEdit 有两个坑，都会把下方的「复制 / 发送」按钮挤掉：
//
// 1. 高度没有上限：TextEdit 的高度 = 文本排版后的高度。
//    一次粘贴长文本 → 输入框长到上千像素 → 按钮被顶出窗口（看不见了）。
//    → 外面套一层「最大高度固定」的 ScrollArea，超出部分在框内滚动，
//      输入框高度恒定，按钮位置永远不变。
//
// 2. 默认只在空格处断行：粘贴一整段没有空格的长文本（URL / base64 / 中文）
//    → 排版宽度暴涨 → 输入框把左右两侧的按钮顶出屏幕。
//    → 自定义 layouter，允许在任意字符间断行。
fn note_text_edit(
    ui: &mut eframe::egui::Ui,
    text: &mut String,
    max_height: f32,
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
    eframe::egui::ScrollArea::vertical()
        .id_source("note_text_edit_scroll")
        .max_height(max_height)
        .show(ui, |ui| {
            ui.add(
                eframe::egui::TextEdit::multiline(text)
                    .desired_width(ui.available_width().max(80.0))
                    .desired_rows(6)
                    .layouter(&mut layouter),
            )
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    /// 真实窗口尺寸见模块顶部的 WIN_W / WIN_H

    /// 模拟一帧布局，返回 (输入框宽, 输入框占位高, 按钮 x, 按钮 y)。
    /// `fixed` = 走修复后的 note_text_edit（限高 + 任意断行）。
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
                let resp = if fixed {
                    note_text_edit(ui, &mut text, 120.0)
                } else {
                    // 旧写法：TextEdit 直接放在布局流里，高度随文本无限增长
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .desired_width(ui.available_width().max(80.0))
                            .desired_rows(4),
                    )
                };
                let btn = ui.allocate_space(egui::vec2(88.0, 32.0)).1;
                // 输入框在父布局里实际占掉的高度（修复后应被限高卡住）
                let used_h = btn.min.y - resp.rect.min.y;
                out = (resp.rect.width(), used_h, btn.min.x, btn.max.y);
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

        // 旧写法：输入框高度 = 文本排版高度（上万像素），按钮被顶到窗口外
        assert!(h_old > 500.0, "旧写法高度本应暴涨, 高 = {h_old}");
        assert!(by_old > WIN_H, "旧写法本应把按钮顶出窗口, y = {by_old}");

        // 修复后：输入框占位高度被限高卡住，按钮留在窗口内
        assert!(h_new <= 130.0, "输入框占位高度仍失控, 高 = {h_new}");
        assert!(w_new <= WIN_W, "输入框宽度仍失控, 宽 = {w_new}");
        assert!(by_new < WIN_H, "按钮仍被挤出窗口, y = {by_new}");
        assert!(bx_new < WIN_W, "按钮仍被挤出屏幕, x = {bx_new}");
    }

}
