// ---------------------------------------------------------------------------
// v1.3 管理端：一屏三栏
//
//   左｜目录（文件树，行高 30 起，长名字换行不截断）  中｜连接 + 设备（296）  右｜设备操作详情（336）
//   底｜便签条（76）
//
// 这里只是「怎么画」。所有动文件的事都走 `crate::ops`，和网页端排同一把锁。
// ---------------------------------------------------------------------------

use super::*;

/// 目录树里的一行（扁平化之后）。
pub(super) struct Row {
    pub(super) rel: String,
    pub(super) name: String,
    pub(super) depth: usize,
    pub(super) is_dir: bool,
    pub(super) size: u64,
}

/// 把 `ops::list` 的扁平条目按父子关系拼成树，再按折叠状态摊平。
///
/// 父路径缺失就补节点：`ops::list` 会返回所有目录，所以正常情况下不会缺；
/// 但快照是 3 秒一次，中途新建的深层目录可能还没被列进来，这里不 panic 就行。
pub(super) fn build_rows(
    snap: &crate::ops::Snapshot,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<Row> {
    use std::collections::BTreeMap;
    let mut kids: BTreeMap<String, Vec<&crate::ops::Entry>> = BTreeMap::new();
    for e in &snap.entries {
        let parent = match e.rel.rsplit_once('/') {
            Some((p, _)) => p.to_string(),
            None => String::new(),
        };
        kids.entry(parent).or_default().push(e);
    }
    // 文件夹排在文件前，同类型按名字（忽略大小写）排
    for v in kids.values_mut() {
        v.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()))
        });
    }

    fn walk(
        prefix: &str,
        depth: usize,
        kids: &std::collections::BTreeMap<String, Vec<&crate::ops::Entry>>,
        collapsed: &std::collections::HashSet<String>,
        out: &mut Vec<Row>,
    ) {
        let Some(list) = kids.get(prefix) else { return };
        for e in list {
            let name = e.rel.rsplit('/').next().unwrap_or(&e.rel).to_string();
            out.push(Row {
                rel: e.rel.clone(),
                name,
                depth,
                is_dir: e.is_dir,
                size: e.size,
            });
            if e.is_dir && !collapsed.contains(&e.rel) {
                walk(&e.rel, depth + 1, kids, collapsed, out);
            }
        }
    }

    let mut out = Vec::new();
    walk("", 0, &kids, collapsed, &mut out);
    out
}

/// 第一次见到的文件夹默认折叠。
///
/// `known` 记住出现过的目录，`collapsed` 只在新目录上补一笔：用户手动展开过
/// 的目录不会因为下一次快照又被折回去。
pub(super) fn fold_new_dirs(
    snap: &crate::ops::Snapshot,
    known: &mut std::collections::HashSet<String>,
    collapsed: &mut std::collections::HashSet<String>,
) {
    for e in &snap.entries {
        if e.is_dir && known.insert(e.rel.clone()) {
            collapsed.insert(e.rel.clone());
        }
    }
}

/// 文件夹图标（矢量画，不依赖 emoji 字体）
fn paint_folder(p: &eframe::egui::Painter, c: eframe::egui::Pos2, color: eframe::egui::Color32) {
    use eframe::egui::{pos2, Rect, Stroke};
    let body = Rect::from_min_size(pos2(c.x - 5.0, c.y - 3.5), eframe::egui::vec2(10.0, 7.5));
    p.rect_filled(body, 1.5, color);
    let tab = Rect::from_min_size(pos2(c.x - 5.0, c.y - 5.0), eframe::egui::vec2(4.5, 2.5));
    p.rect_filled(tab, 1.0, color);
    let _ = Stroke::NONE;
}

/// 文件图标
fn paint_file(p: &eframe::egui::Painter, c: eframe::egui::Pos2, color: eframe::egui::Color32) {
    use eframe::egui::{pos2, Rect, Stroke};
    let body = Rect::from_min_size(pos2(c.x - 4.0, c.y - 5.0), eframe::egui::vec2(8.0, 10.0));
    p.rect(body, 1.5, eframe::egui::Color32::TRANSPARENT, Stroke::new(1.0_f32, color));
    p.line_segment(
        [
            pos2(c.x - 2.0, c.y - 1.5),
            pos2(c.x + 2.0, c.y - 1.5),
        ],
        Stroke::new(1.0_f32, color),
    );
    p.line_segment(
        [pos2(c.x - 2.0, c.y + 1.0), pos2(c.x + 2.0, c.y + 1.0)],
        Stroke::new(1.0_f32, color),
    );
}

/// 铅笔（改名）
fn paint_pencil(p: &eframe::egui::Painter, c: eframe::egui::Pos2, color: eframe::egui::Color32, k: f32) {
    use eframe::egui::{pos2, Stroke};
    p.line_segment(
        [pos2(c.x - 3.5 * k, c.y + 3.5 * k), pos2(c.x + 3.5 * k, c.y - 3.5 * k)],
        Stroke::new(1.6_f32 * k, color),
    );
    p.line_segment(
        [pos2(c.x - 3.5 * k, c.y + 3.5 * k), pos2(c.x - 1.0 * k, c.y + 2.6 * k)],
        Stroke::new(1.2_f32 * k, color),
    );
}

/// 垃圾桶（删除）
fn paint_trash(p: &eframe::egui::Painter, c: eframe::egui::Pos2, color: eframe::egui::Color32, k: f32) {
    use eframe::egui::{pos2, Rect, Stroke};
    let s = Stroke::new(1.2_f32 * k, color);
    p.line_segment([pos2(c.x - 4.0 * k, c.y - 3.0 * k), pos2(c.x + 4.0 * k, c.y - 3.0 * k)], s);
    p.line_segment([pos2(c.x - 1.5 * k, c.y - 4.5 * k), pos2(c.x + 1.5 * k, c.y - 4.5 * k)], s);
    let body = Rect::from_min_size(
        pos2(c.x - 3.0 * k, c.y - 1.5 * k),
        eframe::egui::vec2(6.0 * k, 6.5 * k),
    );
    p.rect(body, 1.0, eframe::egui::Color32::TRANSPARENT, s);
}

impl GuiApp {
    /// 一帧的工作区：三栏 + 便签条 + 确认窗。
    pub(super) fn draw_workspace(&mut self, ctx: &eframe::egui::Context) {
        // 后台线程送来的目录快照（可能一次收到好几份，只留最新的）
        while let Ok(snap) = self.files_rx.try_recv() {
            // 新出现的文件夹默认折叠（用户展开过的不会被重新折回去）
            fold_new_dirs(&snap, &mut self.known_dirs, &mut self.collapsed);
            self.files = snap;
            self.files_loading = false;
        }
        // 最小化时后台不刷目录
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        self.window_visible
            .store(!minimized, std::sync::atomic::Ordering::SeqCst);

        // 状态栏的统计（每次快照算一遍，1 万条也就几毫秒）
        let mut dirs = 0usize;
        let mut files = 0usize;
        let mut bytes = 0u64;
        for e in &self.files.entries {
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
                bytes += e.size;
            }
        }
        self.counts = (dirs, files, bytes);

        let pal_bg = pal::now().bg;
        eframe::egui::CentralPanel::default()
            .frame(
                eframe::egui::Frame::none()
                    .fill(pal_bg)
                    .inner_margin(eframe::egui::Margin::symmetric(12.0, 10.0)),
            )
            .show(ctx, |ui| {
                let panel = ui.available_rect_before_wrap();

                // 展开态：三栏让位，便签铺满整个内容区
                if self.note_expanded {
                    self.note_panel(ui, panel, ctx, true);
                    return;
                }

                let note_rect = eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(panel.left(), panel.bottom() - NOTE_STRIP_H),
                    panel.max,
                );
                let main_rect = eframe::egui::Rect::from_min_max(
                    panel.min,
                    eframe::egui::pos2(panel.right(), panel.bottom() - NOTE_STRIP_H - COL_GAP),
                );
                let wide = panel.width() >= WIDE_MIN_W;

                if wide {
                    let left_w = (main_rect.width() - COL_DEVICE_W - COL_DETAIL_W - COL_GAP * 2.0)
                        .max(240.0);
                    let left = eframe::egui::Rect::from_min_size(
                        main_rect.min,
                        eframe::egui::vec2(left_w, main_rect.height()),
                    );
                    let mid = eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(left.right() + COL_GAP, main_rect.top()),
                        eframe::egui::vec2(COL_DEVICE_W, main_rect.height()),
                    );
                    let right = eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(mid.right() + COL_GAP, main_rect.top()),
                        eframe::egui::vec2(COL_DETAIL_W, main_rect.height()),
                    );
                    self.dir_panel(ui, left, ctx);
                    self.connect_panel(ui, mid, ctx);
                    self.detail_panel(ui, right, ctx);
                } else {
                    // 窄窗：目录 + 设备表并排，详情放进抽屉（标题栏的「设备 (n)」）
                    let left_w = (main_rect.width() - COL_DEVICE_W - COL_GAP).max(220.0);
                    let left = eframe::egui::Rect::from_min_size(
                        main_rect.min,
                        eframe::egui::vec2(left_w, main_rect.height()),
                    );
                    let mid = eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(left.right() + COL_GAP, main_rect.top()),
                        eframe::egui::vec2(COL_DEVICE_W, main_rect.height()),
                    );
                    self.dir_panel(ui, left, ctx);
                    self.connect_panel(ui, mid, ctx);
                }

                self.note_panel(ui, note_rect, ctx, false);
            });

        // 窄窗的详情抽屉（便签铺开时不画，免得盖在编辑区上）
        if !self.note_expanded && self.drawer_open && ctx.screen_rect().width() < WIDE_MIN_W {
            let mut open = true;
            eframe::egui::Window::new("设备操作详情")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_size([COL_DETAIL_W, 420.0])
                .anchor(eframe::egui::Align2::RIGHT_TOP, [-16.0, 56.0])
                .show(ctx, |ui| {
                    self.detail_body(ui, ctx);
                });
            if !open {
                self.drawer_open = false;
            }
        }

        self.draw_confirm(ctx);
    }

    /// 画一张卡片底（圆角 + 边框），返回卡片内可用的矩形。
    fn card_bg(&self, ui: &eframe::egui::Ui, rect: eframe::egui::Rect) -> eframe::egui::Rect {
        ui.painter().rect(
            rect,
            12.0,
            pal::now().surface,
            eframe::egui::Stroke::new(1.0_f32, pal::now().card_border),
        );
        rect.shrink(10.0)
    }

    // ---- 左栏：目录 ------------------------------------------------------

    fn dir_panel(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) {
        ui.allocate_ui_at_rect(rect, |ui| {
            let inner = self.card_bg(ui, ui.max_rect());
            let mut rect = inner;

            // 工具栏 38：路径 + 刷新 + 打开目录
            let tool = eframe::egui::Rect::from_min_size(
                rect.min,
                eframe::egui::vec2(rect.width(), 38.0),
            );
            ui.allocate_ui_at_rect(tool, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // 路径框（等宽字体，超长省略）：吃掉按钮之外的全部宽度，
                    // 窗口变宽就给目录名更多空间，三个按钮始终贴在右边
                    let dir = self.cfg.upload_dir().to_string_lossy().to_string();
                    let font = eframe::egui::FontId::monospace(12.0);
                    let btn_w = 50.0 + 60.0 + 50.0;
                    let gaps = 6.0 * 3.0;
                    let w = (ui.available_width() - btn_w - gaps).max(60.0);
                    let shown = elide(ctx, &dir, font.clone(), w - 16.0);
                    eframe::egui::Frame::none()
                        .fill(pal::now().soft_bg)
                        .rounding(7.0)
                        .inner_margin(eframe::egui::Margin::symmetric(8.0, 5.0))
                        .show(ui, |ui| {
                            ui.set_width((w - 16.0).max(20.0));
                            ui.label(RichText::new(shown).font(font).color(pal::now().soft_fg));
                        });
                    if btn_soft(ui, "刷新", 11.0, 24.0, 50.0) {
                        self.refresh_now
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        ctx.request_repaint();
                    }
                    if btn_soft(ui, "打开目录", 11.0, 24.0, 60.0) {
                        open_in_explorer(self.cfg.upload_dir());
                    }
                    if btn_soft(ui, "更改", 11.0, 24.0, 50.0) {
                        if let Some(d) = choose_dir() {
                            self.set_dir(d);
                            self.refresh_now
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            ctx.request_repaint();
                        }
                    }
                });
            });
            rect.min.y = tool.bottom() + 2.0;

            // 列头 22
            let head = eframe::egui::Rect::from_min_size(
                rect.min,
                eframe::egui::vec2(rect.width(), 22.0),
            );
            let cols = dir_columns(head);
            ui.allocate_ui_at_rect(head, |ui| {
                let p = ui.painter();
                let f = eframe::egui::FontId::proportional(11.0);
                let c = pal::now().muted;
                p.text(cols.0.left_center(), eframe::egui::Align2::LEFT_CENTER, "名称", f.clone(), c);
                p.text(cols.1.right_center(), eframe::egui::Align2::RIGHT_CENTER, "大小", f, c);
            });
            rect.min.y = head.bottom();

            // 状态栏 24（贴着卡片底）
            let status = eframe::egui::Rect::from_min_max(
                eframe::egui::pos2(rect.left(), rect.bottom() - 24.0),
                rect.max,
            );
            let list_rect = eframe::egui::Rect::from_min_max(
                rect.min,
                eframe::egui::pos2(rect.right(), status.top() - 2.0),
            );

            self.file_list(ui, list_rect, ctx);
            self.dir_status(ui, status, ctx);
        });
    }

    fn file_list(
        &mut self,
        ui: &mut eframe::egui::Ui,
        rect: eframe::egui::Rect,
        ctx: &eframe::egui::Context,
    ) {
        let rows = build_rows(&self.files, &self.collapsed);
        ui.allocate_ui_at_rect(rect, |ui| {
            eframe::egui::ScrollArea::vertical()
                .id_source("dir_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.files_loading {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("正在读取…")
                                .size(11.5)
                                .color(pal::now().muted),
                        );
                        return;
                    }
                    if rows.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("这个目录还是空的")
                                .size(11.5)
                                .color(pal::now().muted),
                        );
                        return;
                    }
                    let cap = rows.len().min(RENDER_CAP);
                    for i in 0..cap {
                        self.dir_row(ui, &rows[i], ctx);
                    }
                    if rows.len() > cap {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "仅渲染前 {cap} 行（共 {} 行），其余请在资源管理器里查看",
                                rows.len()
                            ))
                            .size(10.5)
                            .color(pal::now().btn_hover_close),
                        );
                    }
                });
        });
    }

    fn dir_row(&mut self, ui: &mut eframe::egui::Ui, row: &Row, ctx: &eframe::egui::Context) {
        let w = ui.available_width().max(120.0);
        let indent = row.depth as f32 * 14.0;
        let font = eframe::egui::FontId::proportional(12.5);
        let pal = pal::now();
        let name_color = if row.is_dir { pal.fg } else { pal.soft_fg };
        let renaming_this = self
            .renaming
            .as_ref()
            .map(|(rel, _)| rel == &row.rel)
            .unwrap_or(false);

        // 名字按列宽排版：宁可换行也不截断（完整标题必须看得见），行高跟着变
        let name_w = {
            let probe = eframe::egui::Rect::from_min_size(
                eframe::egui::pos2(0.0, 0.0),
                eframe::egui::vec2(w, 1.0),
            );
            let c = dir_columns(probe);
            (c.1.left() - (c.0.left() + indent + 27.0) - 8.0).max(40.0)
        };
        let galley = if renaming_this {
            None
        } else {
            let mut job = eframe::egui::text::LayoutJob::simple(
                row.name.clone(),
                font.clone(),
                name_color,
                name_w,
            );
            job.wrap.break_anywhere = true; // 中文/无空格的长名字也能断行
            Some(ui.fonts(|f| f.layout_job(job)))
        };
        let row_h = galley
            .as_ref()
            .map(|g| (g.rect.height() + 10.0).max(ROW_H))
            .unwrap_or(ROW_H);

        let (rect, resp) =
            ui.allocate_exact_size(eframe::egui::vec2(w, row_h), eframe::egui::Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let selected = self.selected.as_deref() == Some(row.rel.as_str());
        if selected {
            ui.painter()
                .rect(rect, 6.0, pal.soft_bg, eframe::egui::Stroke::NONE);
        } else if resp.hovered() {
            ui.painter().rect(
                rect,
                6.0,
                pal.soft_bg.gamma_multiply(0.6),
                eframe::egui::Stroke::NONE,
            );
        }

        let cols = dir_columns(rect);
        let icon_cx = cols.0.left() + indent + 7.0;

        // 展开箭头（只有目录有）
        if row.is_dir {
            let arrow = if self.collapsed.contains(&row.rel) {
                "▸"
            } else {
                "▾"
            };
            ui.painter().text(
                eframe::egui::pos2(icon_cx, rect.center().y),
                eframe::egui::Align2::CENTER_CENTER,
                arrow,
                eframe::egui::FontId::proportional(9.0),
                pal.muted,
            );
        }
        let icon_x = icon_cx + 11.0;
        if row.is_dir {
            paint_folder(ui.painter(), eframe::egui::pos2(icon_x, rect.center().y), pal.accent);
        } else {
            paint_file(ui.painter(), eframe::egui::pos2(icon_x, rect.center().y), pal.muted);
        }

        // 名字（改名时就地变成输入框）
        let name_left = icon_x + 9.0;

        if renaming_this {
            let mut text = self.renaming.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
            let edit_rect = eframe::egui::Rect::from_min_size(
                eframe::egui::pos2(name_left, rect.center().y - 11.0),
                eframe::egui::vec2(name_w, 22.0),
            );
            let r = ui.put(
                edit_rect,
                eframe::egui::TextEdit::singleline(&mut text)
                    .font(eframe::egui::FontId::proportional(12.5))
                    .desired_width(name_w),
            );
            if self.rename_fresh {
                r.request_focus();
                // 只选中主文件名（不含扩展名）：改名通常只想改前半段
                if let Some(mut state) =
                    eframe::egui::text_edit::TextEditState::load(ctx, r.id)
                {
                    let stem = text
                        .rsplit_once('.')
                        .map(|(s, _)| s.chars().count())
                        .unwrap_or_else(|| text.chars().count());
                    let cc = eframe::egui::text::CCursor::new(stem);
                    state.cursor.set_char_range(Some(
                        eframe::egui::text::CCursorRange::one(cc),
                    ));
                    state.store(ctx, r.id);
                }
                self.rename_fresh = false;
            }
            if let Some((_, t)) = self.renaming.as_mut() {
                *t = text.clone();
            }
            let enter = ui.input(|i| i.key_pressed(eframe::egui::Key::Enter));
            // Esc 已在 GuiApp::handle_escape 里统一处理（会先把 renaming 清掉）
            if enter || r.lost_focus() {
                let from = row.rel.clone();
                let to = text.trim().replace('\\', "/");
                self.renaming = None;
                if !to.is_empty() && to != from {
                    self.commit_rename(&from, &to, ctx);
                }
            }
            return;
        }

        if let Some(g) = galley {
            let galley_h = g.rect.height();
            let y = rect.top() + (row_h - galley_h) * 0.5;
            ui.painter()
                .galley(eframe::egui::pos2(name_left, y), g, name_color);
        }

        // 大小
        let f_small = eframe::egui::FontId::proportional(11.0);
        let size_txt = if row.is_dir {
            String::new()
        } else {
            fmt_size(row.size)
        };
        ui.painter().text(
            cols.1.right_center(),
            eframe::egui::Align2::RIGHT_CENTER,
            size_txt,
            f_small,
            pal.muted,
        );

        // 行内 ✏ 🗑（悬停或选中时才出现，免得整屏都是按钮）
        let actions_rect = cols.2;
        if resp.hovered() || selected {
            let pencil = eframe::egui::Rect::from_center_size(
                eframe::egui::pos2(actions_rect.left() + 23.0, rect.center().y),
                eframe::egui::vec2(32.0, 22.0),
            );
            let trash = eframe::egui::Rect::from_center_size(
                eframe::egui::pos2(actions_rect.left() + 59.0, rect.center().y),
                eframe::egui::vec2(32.0, 22.0),
            );
            let pr = ui.interact(pencil, ui.id().with(("pen", &row.rel)), eframe::egui::Sense::click());
            let tr = ui.interact(trash, ui.id().with(("trash", &row.rel)), eframe::egui::Sense::click());
            if pr.hovered() {
                ui.painter().rect_filled(pencil, 5.0, pal.soft_bg);
            }
            if tr.hovered() {
                ui.painter().rect_filled(trash, 5.0, pal.soft_bg);
            }
            paint_pencil(
                ui.painter(),
                pencil.center(),
                if pr.hovered() { pal.accent } else { pal.muted },
                1.3,
            );
            paint_trash(
                ui.painter(),
                trash.center(),
                if tr.hovered() {
                    pal.btn_hover_close
                } else {
                    pal.muted
                },
                1.3,
            );
            if pr.clicked() {
                self.selected = Some(row.rel.clone());
                self.renaming = Some((row.rel.clone(), row.name.clone()));
                self.rename_fresh = true;
                ctx.request_repaint();
                return;
            }
            if tr.clicked() {
                self.ask_delete(row);
                return;
            }
        }

        if resp.clicked() {
            self.selected = Some(row.rel.clone());
            if row.is_dir {
                // 文件夹单击同时切展开（和资源管理器一致）
                if !self.collapsed.remove(&row.rel) {
                    self.collapsed.insert(row.rel.clone());
                }
            }
        }
        if resp.double_clicked() && row.is_dir {
            // 双击进目录：展开 + 选中（不做路径跳转，避免出现「当前目录」概念）
            self.collapsed.remove(&row.rel);
        }
    }

    fn dir_status(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) {
        ui.allocate_ui_at_rect(rect, |ui| {
            let p = ui.painter();
            let f = eframe::egui::FontId::proportional(10.5);
            let (dirs, files, bytes) = self.counts;
            let fresh_msg = !self.status_msg.is_empty()
                && self
                    .status_at
                    .map(|t| t.elapsed().as_secs() < 6)
                    .unwrap_or(false);
            let left = if fresh_msg {
                self.status_msg.clone()
            } else if let Some(rel) = &self.selected {
                let size = self
                    .files
                    .entries
                    .iter()
                    .find(|e| &e.rel == rel)
                    .map(|e| e.size)
                    .unwrap_or(0);
                let name = rel.rsplit('/').next().unwrap_or(rel);
                if size > 0 {
                    format!("已选中：{name} · {}", fmt_size(size))
                } else {
                    format!("已选中：{name}")
                }
            } else {
                "单击选中，文件夹可展开".to_string()
            };
            let color = if fresh_msg && self.status_is_err {
                pal::now().btn_hover_close
            } else {
                pal::now().muted
            };
            p.text(
                eframe::egui::pos2(rect.left(), rect.center().y),
                eframe::egui::Align2::LEFT_CENTER,
                elide(ctx, &left, f.clone(), rect.width() * 0.55),
                f.clone(),
                color,
            );
            // 窄窗不画统计：两段文字会叠在一起
            if rect.width() > 430.0 {
                p.text(
                    eframe::egui::pos2(rect.right(), rect.center().y),
                    eframe::egui::Align2::RIGHT_CENTER,
                    format!("{dirs} 个文件夹 · {files} 个文件 · {}", fmt_size(bytes)),
                    f,
                    pal::now().muted,
                );
            }
        });
    }

    // ---- 中栏：连接 + 设备 ------------------------------------------------

    fn connect_panel(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) {
        ui.allocate_ui_at_rect(rect, |ui| {
            // 连接卡片按内容自适应高度：HTTPS 下面不留空白，省出来的都给设备表
            let used_bottom = self.connect_card(ui, rect, ctx);
            let top = (used_bottom + COL_GAP)
                .min(rect.bottom() - 120.0)
                .max(rect.top() + 40.0);
            let bottom = eframe::egui::Rect::from_min_max(
                eframe::egui::pos2(rect.left(), top),
                rect.max,
            );
            self.device_card(ui, bottom, ctx);
        });
    }

    fn connect_card(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) -> f32 {
        let url = self.base_url();
        let mut toggle_hidden = false;
        let mut copy_addr = false;
        let mut rotate_code = false;
        let mut tls_changed: Option<bool> = None;
        let mut copy_fp = false;

        let inner_w = (rect.width() - 20.0).max(120.0);
        let card = eframe::egui::Frame::none()
            .fill(pal::now().surface)
            .rounding(12.0)
            .stroke(eframe::egui::Stroke::new(1.0_f32, pal::now().card_border))
            .inner_margin(eframe::egui::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_width(inner_w);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("连接").size(12.5).strong().color(pal::now().fg));
                    ui.with_layout(
                        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                        |ui| {
                            let tip = if self.pair_hidden {
                                "显示二维码和配对码"
                            } else {
                                "隐藏二维码和配对码"
                            };
                            if eye_button(ui, self.pair_hidden).on_hover_text(tip).clicked() {
                                toggle_hidden = true;
                            }
                            ui.label(
                                RichText::new("未配对的设备看不到文件")
                                    .size(10.0)
                                    .color(pal::now().muted),
                            );
                        },
                    );
                });
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    // 二维码：白底方框自己画（居中、四边留白一致），码贴到框里
                    let qr = 112.0_f32;
                    let (qr_rect, qr_resp) = ui.allocate_exact_size(
                        eframe::egui::vec2(qr, qr),
                        eframe::egui::Sense::click(),
                    );
                    ui.painter().rect(
                        qr_rect,
                        8.0,
                        eframe::egui::Color32::WHITE,
                        eframe::egui::Stroke::new(1.0_f32, pal::now().card_border),
                    );
                    if self.pair_hidden {
                        ui.painter().text(
                            qr_rect.center(),
                            eframe::egui::Align2::CENTER_CENTER,
                            "已隐藏\n点眼睛显示",
                            eframe::egui::FontId::proportional(10.0),
                            pal::now().muted,
                        );
                    } else if let Some(h) = &self.qr_handle {
                        // 码本身不带白边，四周 5px 白框就是静区
                        ui.painter().image(
                            h.id(),
                            qr_rect.shrink(5.0),
                            eframe::egui::Rect::from_min_max(
                                eframe::egui::pos2(0.0, 0.0),
                                eframe::egui::pos2(1.0, 1.0),
                            ),
                            eframe::egui::Color32::WHITE,
                        );
                    }
                    if qr_resp.clicked() && self.pair_hidden {
                        toggle_hidden = true;
                    }

                    // 右侧：配对码 + 地址 + 复制
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(
                            RichText::new("配对码（或扫左边的码）")
                                .size(10.0)
                                .color(pal::now().muted),
                        );
                        let code = if self.pair_hidden {
                            "••••••••".to_string()
                        } else {
                            self.pair_code.clone()
                        };
                        let code_resp = ui.add(
                            eframe::egui::Label::new(
                                RichText::new(code)
                                    .size(19.0)
                                    .strong()
                                    .monospace()
                                    .color(pal::now().fg),
                            )
                            .sense(eframe::egui::Sense::click()),
                        );
                        if code_resp.clicked() {
                            if self.pair_hidden {
                                toggle_hidden = true;
                            } else {
                                ctx.copy_text(self.pair_code.clone());
                                self.pair_msg = "配对码已复制".to_string();
                            }
                        }
                        // 地址不省略：先缩字号尽量整行放下，实在放不下再换行
                        let avail = ui.available_width() - 2.0;
                        let mut font = eframe::egui::FontId::monospace(10.5);
                        for cand in [10.5_f32, 10.0, 9.5, 9.0] {
                            font = eframe::egui::FontId::monospace(cand);
                            let fits = ui.fonts(|fonts| {
                                fonts
                                    .layout_no_wrap(
                                        url.clone(),
                                        font.clone(),
                                        eframe::egui::Color32::WHITE,
                                    )
                                    .size()
                                    .x
                            }) <= avail;
                            if fits {
                                break;
                            }
                        }
                        let mut job = eframe::egui::text::LayoutJob::simple(
                            url.clone(),
                            font,
                            pal::now().accent,
                            avail,
                        );
                        job.wrap.break_anywhere = true;
                        let addr = ui.label(job);
                        // 热点 / 双网卡时默认路由的 IP 未必是手机能连上的那个
                        if self.ips.len() > 1 {
                            addr.on_hover_text(format!(
                                "全部 IP：{}\n（热点一般是 192.168.137.1）",
                                self.ips.join("、")
                            ));
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            if btn_link(ui, "复制地址") {
                                copy_addr = true;
                            }
                            if btn_link(ui, "刷新二维码") {
                                rotate_code = true;
                            }
                        });
                    });
                });

                if let Some(secs) = self.cfg.pair_lock_left() {
                    ui.label(
                        RichText::new(format!("已锁定，{secs} 秒后可重试（刷新二维码可立即解除）"))
                            .size(10.0)
                            .color(pal::now().btn_hover_close),
                    );
                }
                if !self.pair_msg.is_empty() {
                    ui.label(
                        RichText::new(&self.pair_msg)
                            .size(10.0)
                            .color(pal::now().muted),
                    );
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                // HTTPS
                let mut tls_on = self.cfg.tls_on();
                let resp = ui.checkbox(
                    &mut tls_on,
                    RichText::new("启用 HTTPS（加密传输）")
                        .size(11.5)
                        .color(pal::now().fg),
                );
                if resp.changed() {
                    tls_changed = Some(tls_on);
                }
                if self.cfg.tls_on() {
                    if let Some(m) = self.cfg.tls_material() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("证书指纹")
                                    .size(10.0)
                                    .color(pal::now().muted),
                            );
                            let fp = fingerprint_short(&m.fingerprint);
                            ui.label(
                                RichText::new(fp)
                                    .font(eframe::egui::FontId::monospace(9.5))
                                    .color(pal::now().soft_fg),
                            );
                            if btn_link(ui, "复制") {
                                copy_fp = true;
                            }
                        });
                    }
                } else {
                    ui.label(
                        RichText::new("未开启时是明文 HTTP，同一网络内可被旁观。")
                            .size(10.0)
                            .color(pal::now().muted),
                    );
                }
            });

        let used_bottom = card.response.rect.bottom();

        if toggle_hidden {
            self.pair_hidden = !self.pair_hidden;
            self.pair_msg.clear();
        }
        if copy_addr {
            ctx.copy_text(url);
            self.pair_msg = "地址已复制".to_string();
        }
        if rotate_code {
            let (code, secret) = self.cfg.reset_pairing();
            self.pair_code = code;
            self.pair_secret = secret;
            self.pair_hidden = false;
            self.pair_msg = "二维码已刷新，旧码立即失效".to_string();
            self.refresh_qr(ctx);
        }
        if copy_fp {
            if let Some(m) = self.cfg.tls_material() {
                ctx.copy_text(m.fingerprint.clone());
                self.pair_msg = "指纹已复制".to_string();
            }
        }
        if let Some(on) = tls_changed {
            self.set_tls(on);
            ctx.request_repaint();
        }
        used_bottom
    }

    fn device_card(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) {
        let devices = self.cfg.device_list();
        // 点设备行 -> 右栏详情；点开关 -> 授权；点断开 -> 踢会话
        let mut pick: Option<String> = None;
        let mut set_write: Option<(String, bool)> = None;
        let mut kick: Option<String> = None;

        ui.allocate_ui_at_rect(rect, |ui| {
            let inner = self.card_bg(ui, ui.max_rect());
            ui.allocate_ui_at_rect(inner, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("设备 {} 台", devices.len()))
                            .size(12.0)
                            .strong()
                            .color(pal::now().fg),
                    );
                    ui.with_layout(
                        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new("关授权不影响传输")
                                    .size(9.5)
                                    .color(pal::now().muted),
                            );
                        },
                    );
                });
                ui.add_space(4.0);

                // 表头
                let w = ui.available_width();
                let (head, _) = ui.allocate_exact_size(
                    eframe::egui::vec2(w, 18.0),
                    eframe::egui::Sense::hover(),
                );
                let c = dev_columns(head);
                let f = eframe::egui::FontId::proportional(9.5);
                let muted = pal::now().muted;
                let p = ui.painter();
                p.text(c.0.left_center(), eframe::egui::Align2::LEFT_CENTER, "设备", f.clone(), muted);
                p.text(c.1.center_top(), eframe::egui::Align2::CENTER_TOP, "空闲", f.clone(), muted);
                p.text(c.2.center_top(), eframe::egui::Align2::CENTER_TOP, "删除/修改", f, muted);

                eframe::egui::ScrollArea::vertical()
                    .id_source("device_list")
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - 26.0).max(40.0))
                    .show(ui, |ui| {
                        for d in &devices {
                            let (row, resp) = ui.allocate_exact_size(
                                eframe::egui::vec2(ui.available_width(), DEV_ROW_H),
                                eframe::egui::Sense::click(),
                            );
                            let cols = dev_columns(row);
                            let pal = pal::now();
                            let sel = self.detail_ip.as_deref() == Some(d.ip.as_str());
                            if sel {
                                ui.painter().rect(
                                    row,
                                    6.0,
                                    pal.soft_bg,
                                    eframe::egui::Stroke::NONE,
                                );
                            } else if resp.hovered() {
                                ui.painter().rect(
                                    row,
                                    6.0,
                                    pal.soft_bg.gamma_multiply(0.5),
                                    eframe::egui::Stroke::NONE,
                                );
                            }
                            ui.painter().circle_filled(
                                eframe::egui::pos2(cols.0.left() + 4.0, row.center().y),
                                3.5,
                                if d.idle_secs < 120 {
                                    pal.green_dot
                                } else {
                                    pal.input_border
                                },
                            );
                            let ip_font = eframe::egui::FontId::proportional(11.5);
                            ui.painter().text(
                                eframe::egui::pos2(cols.0.left() + 12.0, row.center().y),
                                eframe::egui::Align2::LEFT_CENTER,
                                elide(ctx, &d.ip, ip_font.clone(), cols.0.width() - 14.0),
                                ip_font,
                                pal.fg,
                            );
                            let f_small = eframe::egui::FontId::proportional(10.0);
                            ui.painter().text(
                                cols.1.center(),
                                eframe::egui::Align2::CENTER_CENTER,
                                fmt_ago_short(d.idle_secs),
                                f_small,
                                pal.muted,
                            );
                            // 授权开关
                            let sw_rect = eframe::egui::Rect::from_center_size(
                                cols.2.center(),
                                eframe::egui::vec2(30.0, 16.0),
                            );
                            let mut on = d.allow_write;
                            let sw = ui.interact(
                                sw_rect,
                                ui.id().with(("sw", &d.ip)),
                                eframe::egui::Sense::click(),
                            );
                            let p = ui.painter();
                            p.rect(
                                sw_rect,
                                8.0,
                                if on { pal.accent } else { pal.input_border },
                                eframe::egui::Stroke::NONE,
                            );
                            let knob_x = if on {
                                sw_rect.right() - 8.0
                            } else {
                                sw_rect.left() + 8.0
                            };
                            p.circle_filled(
                                eframe::egui::pos2(knob_x, sw_rect.center().y),
                                6.0,
                                eframe::egui::Color32::WHITE,
                            );
                            if sw.clicked() {
                                on = !on;
                                set_write = Some((d.ip.clone(), on));
                            }
                            // 断开
                            let kick_rect = eframe::egui::Rect::from_min_max(
                                eframe::egui::pos2(cols.2.right() + 2.0, row.top()),
                                row.max,
                            );
                            let kr = ui.interact(
                                kick_rect,
                                ui.id().with(("kick", &d.ip)),
                                eframe::egui::Sense::click(),
                            );
                            ui.painter().text(
                                kick_rect.center(),
                                eframe::egui::Align2::CENTER_CENTER,
                                "断开",
                                eframe::egui::FontId::proportional(10.0),
                                if kr.hovered() {
                                    pal.btn_hover_close
                                } else {
                                    pal.muted
                                },
                            );
                            if kr.clicked() {
                                kick = Some(d.ip.clone());
                            } else if resp.clicked() {
                                pick = Some(d.ip.clone());
                            }
                        }
                    });

                // 表尾：等待新设备接入
                let rest = ui.available_rect_before_wrap();
                if rest.height() > 4.0 {
                    let y = rest.top() + 8.0;
                    let p = ui.painter();
                    let color = pal::now().card_border;
                    // 虚线：手动画几段短横线
                    let mut x = rest.left();
                    while x < rest.right() {
                        p.line_segment(
                            [
                                eframe::egui::pos2(x, y),
                                eframe::egui::pos2((x + 4.0).min(rest.right()), y),
                            ],
                            eframe::egui::Stroke::new(1.0_f32, color),
                        );
                        x += 7.0;
                    }
                    p.text(
                        eframe::egui::pos2(rest.center().x, y + 10.0),
                        eframe::egui::Align2::CENTER_CENTER,
                        "＋ 等待新设备接入",
                        eframe::egui::FontId::proportional(10.0),
                        pal::now().muted,
                    );
                }
            });
        });

        if let Some((ip, on)) = set_write {
            self.cfg.set_device_write(&ip, on);
            ctx.request_repaint();
        }
        if let Some(ip) = kick {
            let n = self.cfg.drop_client(&ip);
            self.status_msg = format!("已断开 {ip}（{n} 个会话），对方需要重新配对");
            self.status_at = Some(std::time::Instant::now());
            self.status_is_err = false;
            if self.detail_ip.as_deref() == Some(ip.as_str()) {
                self.detail_ip = None;
            }
            ctx.request_repaint();
        }
        if let Some(ip) = pick {
            self.detail_ip = Some(ip);
        }
        // 默认选第一台
        if self.detail_ip.is_none() {
            self.detail_ip = devices.first().map(|d| d.ip.clone());
        }
    }

    // ---- 右栏：设备操作详情 ------------------------------------------------

    fn detail_panel(&mut self, ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, ctx: &eframe::egui::Context) {
        ui.allocate_ui_at_rect(rect, |ui| {
            let inner = self.card_bg(ui, ui.max_rect());
            ui.allocate_ui_at_rect(inner, |ui| {
                self.detail_body(ui, ctx);
            });
        });
    }

    fn detail_body(&mut self, ui: &mut eframe::egui::Ui, ctx: &eframe::egui::Context) {
        let Some(ip) = self.detail_ip.clone() else {
            ui.add_space(8.0);
            ui.label(
                RichText::new("点左边的设备，看它自己的操作")
                    .size(11.5)
                    .color(pal::now().muted),
            );
            return;
        };
        let dev = self.cfg.device_list().into_iter().find(|d| d.ip == ip);
        let acts = self.cfg.recent_activity_for(&ip, 30);

        ui.horizontal(|ui| {
            ui.label(RichText::new(&ip).size(13.0).strong().color(pal::now().fg));
            ui.with_layout(
                eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                |ui| {
                    let (txt, color) = match &dev {
                        Some(d) if d.allow_write => ("可删改", pal::now().green),
                        Some(_) => ("已禁止删改", pal::now().btn_hover_close),
                        None => ("已断开", pal::now().muted),
                    };
                    eframe::egui::Frame::none()
                        .fill(pal::now().soft_bg)
                        .rounding(6.0)
                        .inner_margin(eframe::egui::Margin::symmetric(6.0, 2.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(txt).size(9.5).color(color));
                        });
                },
            );
        });
        ui.add_space(2.0);
        let sub = match &dev {
            Some(d) if d.sessions > 1 => format!(
                "已配对设备 · {} 个标签页 · 空闲 {}",
                d.sessions,
                fmt_ago(d.idle_secs)
            ),
            Some(d) => format!("已配对设备 · 空闲 {}", fmt_ago(d.idle_secs)),
            None => "会话已失效（对方需要重新配对）".to_string(),
        };
        ui.label(RichText::new(sub).size(10.0).color(pal::now().muted));

        // 本次：这条 IP 在活动日志里最早一条到现在
        let up = acts.iter().filter(|a| a.kind == crate::config::ActKind::Upload).count();
        let down = acts.iter().filter(|a| a.kind == crate::config::ActKind::Download).count();
        let span = acts
            .last()
            .map(|a| a.at.elapsed().as_secs())
            .unwrap_or(0);
        let span_txt = if span < 60 {
            "不到 1 分钟".to_string()
        } else {
            format!("{} 分钟", span / 60)
        };
        ui.label(
            RichText::new(format!("本次 {span_txt} · ↑{up} · ↓{down}"))
                .size(10.0)
                .color(pal::now().muted),
        );

        ui.add_space(6.0);
        // 表头
        let w = ui.available_width();
        let (head, _) =
            ui.allocate_exact_size(eframe::egui::vec2(w, 18.0), eframe::egui::Sense::hover());
        let c = act_columns(head);
        let f = eframe::egui::FontId::proportional(9.5);
        let muted = pal::now().muted;
        let p = ui.painter();
        p.text(c.0.left_center(), eframe::egui::Align2::LEFT_CENTER, "操作 / 文件", f.clone(), muted);
        p.text(c.1.right_center(), eframe::egui::Align2::RIGHT_CENTER, "大小", f.clone(), muted);
        p.text(c.2.right_center(), eframe::egui::Align2::RIGHT_CENTER, "时间", f, muted);

        if acts.is_empty() {
            ui.add_space(6.0);
            ui.label(
                RichText::new("这台设备还没有操作记录")
                    .size(10.5)
                    .color(pal::now().muted),
            );
        }
        eframe::egui::ScrollArea::vertical()
            .id_source("activity_list")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 18.0).max(40.0))
            .show(ui, |ui| {
                for a in &acts {
                    let (row, _) = ui.allocate_exact_size(
                        eframe::egui::vec2(ui.available_width(), ACT_ROW_H),
                        eframe::egui::Sense::hover(),
                    );
                    let cols = act_columns(row);
                    let pal = pal::now();
                    let f = eframe::egui::FontId::proportional(10.5);
                    let color = match a.kind {
                        crate::config::ActKind::Upload => pal.accent,
                        crate::config::ActKind::Download => pal.green,
                        crate::config::ActKind::Delete => pal.btn_hover_close,
                        crate::config::ActKind::Rename => pal.soft_fg,
                    };
                    let icon = a.kind.arrow();
                    ui.painter().text(
                        eframe::egui::pos2(cols.0.left(), row.center().y),
                        eframe::egui::Align2::LEFT_CENTER,
                        icon,
                        f.clone(),
                        color,
                    );
                    let name_w = (cols.0.width() - 16.0).max(30.0);
                    ui.painter().text(
                        eframe::egui::pos2(cols.0.left() + 14.0, row.center().y),
                        eframe::egui::Align2::LEFT_CENTER,
                        elide(ctx, &a.name, f.clone(), name_w),
                        f.clone(),
                        pal.fg,
                    );
                    ui.painter().text(
                        cols.1.right_center(),
                        eframe::egui::Align2::RIGHT_CENTER,
                        if a.size > 0 { fmt_size(a.size) } else { String::new() },
                        eframe::egui::FontId::proportional(9.5),
                        pal.muted,
                    );
                    ui.painter().text(
                        cols.2.right_center(),
                        eframe::egui::Align2::RIGHT_CENTER,
                        fmt_ago(a.at.elapsed().as_secs()),
                        eframe::egui::FontId::proportional(9.5),
                        pal.muted,
                    );
                }
            });
        ui.label(
            RichText::new("只保留最近 30 条 · 重启清空")
                .size(9.0)
                .color(pal::now().muted),
        );
    }

    // ---- 底部便签：收起是一条，展开铺满整个内容区 --------------------------

    fn note_panel(
        &mut self,
        ui: &mut eframe::egui::Ui,
        rect: eframe::egui::Rect,
        ctx: &eframe::egui::Context,
        expanded: bool,
    ) {
        let mut send = false;
        let mut copy = false;
        let mut toggle: Option<bool> = None;

        ui.allocate_ui_at_rect(rect, |ui| {
            let inner = self.card_bg(ui, ui.max_rect());
            if expanded {
                // 头部：标题 + 右上角「收起」
                let head = eframe::egui::Rect::from_min_size(
                    inner.min,
                    eframe::egui::vec2(inner.width(), 24.0),
                );
                ui.allocate_ui_at_rect(head, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("快捷文本")
                                .size(12.0)
                                .strong()
                                .color(pal::now().fg),
                        );
                    });
                });
                if note_expand_icon(
                    ui,
                    eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(inner.right() - 22.0, inner.top()),
                        eframe::egui::vec2(18.0, 18.0),
                    ),
                    true,
                ) {
                    toggle = Some(false);
                }

                // 底部：左边状态、右边按钮；中间编辑器铺满剩余高度
                let footer = eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(inner.left(), inner.bottom() - 38.0),
                    inner.max,
                );
                let editor = eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(inner.left(), head.bottom() + 6.0),
                    eframe::egui::pos2(inner.right(), footer.top() - 6.0),
                );
                ui.allocate_ui_at_rect(editor, |ui| {
                    let r = note_text_edit(
                        ui,
                        &mut self.text_buf,
                        (editor.height() - 8.0).max(24.0),
                        &mut self.note_focused,
                        true,
                        0.0,
                    );
                    if r.changed() {
                        self.note_dirty = true;
                    }
                });

                let btns_w = 136.0;
                if let Some(msg) = self.note_status_live() {
                    let f = eframe::egui::FontId::proportional(10.5);
                    let color = if self.note_status_warn {
                        pal::now().btn_hover_close
                    } else {
                        pal::now().muted
                    };
                    let room = (footer.width() - btns_w - 8.0).max(40.0);
                    ui.painter().text(
                        footer.left_center(),
                        eframe::egui::Align2::LEFT_CENTER,
                        elide(ctx, msg, f.clone(), room),
                        f,
                        color,
                    );
                }
                ui.allocate_ui_at_rect(
                    eframe::egui::Rect::from_min_size(
                        eframe::egui::pos2(footer.right() - btns_w, footer.top() + 3.0),
                        eframe::egui::vec2(btns_w, 32.0),
                    ),
                    |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if btn_soft(ui, "复制", 12.0, 32.0, 64.0) {
                                copy = true;
                            }
                            if btn_primary(ui, "发送", 12.0, 32.0, 64.0) {
                                send = true;
                            }
                        });
                    },
                );
            } else {
                // 收起：一行输入（右上角展开按钮）。
                // 提示也放在这一行里（输入框和按钮之间），不另占纵向空间 ——
                // 否则窗口够高时便签条会多出一截空白，输入框被顶到上面去。
                let row = eframe::egui::Rect::from_center_size(
                    inner.center(),
                    eframe::egui::vec2(inner.width(), 44.0),
                );
                ui.allocate_ui_at_rect(row, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(
                            RichText::new("快捷文本")
                                .size(11.5)
                                .color(pal::now().muted),
                        );
                        let status = self
                            .note_status_live()
                            .map(|m| (m.to_string(), self.note_status_warn));
                        let status_w = if status.is_some() { 160.0 } else { 0.0 };
                        let extra = if status.is_some() { status_w + 8.0 } else { 0.0 };
                        let btn_w = 64.0 * 2.0 + 8.0;
                        let input_w =
                            (ui.available_width() - btn_w - 8.0 - extra).max(120.0);
                        let (input_rect, _) = ui.allocate_exact_size(
                            eframe::egui::vec2(input_w, 44.0),
                            eframe::egui::Sense::hover(),
                        );
                        ui.allocate_ui_at_rect(input_rect, |ui| {
                            // 右边留 22px 给展开按钮，免得压住第一行最后一个字
                            let r = note_text_edit(
                                ui,
                                &mut self.text_buf,
                                36.0,
                                &mut self.note_focused,
                                false,
                                22.0,
                            );
                            if r.changed() {
                                self.note_dirty = true;
                            }
                        });
                        if note_expand_icon(
                            ui,
                            eframe::egui::Rect::from_min_size(
                                eframe::egui::pos2(input_rect.right() - 22.0, input_rect.top() + 3.0),
                                eframe::egui::vec2(18.0, 18.0),
                            ),
                            false,
                        ) {
                            toggle = Some(true);
                        }
                        if let Some((msg, warn)) = status {
                            let (r, _) = ui.allocate_exact_size(
                                eframe::egui::vec2(status_w, 20.0),
                                eframe::egui::Sense::hover(),
                            );
                            let f = eframe::egui::FontId::proportional(10.5);
                            let color = if warn {
                                pal::now().btn_hover_close
                            } else {
                                pal::now().muted
                            };
                            ui.painter().text(
                                r.left_center(),
                                eframe::egui::Align2::LEFT_CENTER,
                                elide(ctx, &msg, f.clone(), status_w - 4.0),
                                f,
                                color,
                            );
                        }
                        if btn_soft(ui, "复制", 12.0, 32.0, 64.0) {
                            copy = true;
                        }
                        if btn_primary(ui, "发送", 12.0, 32.0, 64.0) {
                            send = true;
                        }
                    });
                });
            }
        });

        if let Some(on) = toggle {
            self.set_note_expanded(on);
            // 光标、选区跟着过去；两种形态共用同一个 TextEdit id
            ctx.memory_mut(|m| m.request_focus(note_editor_id()));
            ctx.request_repaint();
        }

        // Ctrl+Enter 也能发
        let ctrl_enter = ctx.input(|i| {
            i.key_pressed(eframe::egui::Key::Enter)
                && i.modifiers.contains(eframe::egui::Modifiers::CTRL)
        });
        if ctrl_enter {
            let _ = ctx.input_mut(|i| {
                i.consume_key(eframe::egui::Modifiers::CTRL, eframe::egui::Key::Enter)
            });
        }
        if copy {
            let ok = win32_set_clipboard(&self.text_buf);
            if ok {
                self.set_note_status("已复制到剪贴板", false);
            } else {
                self.set_note_status("复制失败：剪贴板被其他程序占用", true);
            }
        }
        if send || ctrl_enter {
            let v = self.cfg.store_note(&self.text_buf);
            self.note_dirty = false;
            self.note_last_v = v;
            self.set_note_status("已发送，另一端将自动填入", false);
        }
    }

    // ---- 改名 / 删除 ------------------------------------------------------

    fn commit_rename(&mut self, from: &str, to: &str, ctx: &eframe::egui::Context) {
        match crate::ops::rename(&self.cfg, from, to, "本机") {
            Ok(()) => {
                self.status_msg = format!("已改名：{to}");
                self.status_at = Some(std::time::Instant::now());
                self.status_is_err = false;
                self.selected = Some(to.to_string());
                // 改完要能看见：把目标所在的父目录展开
                if let Some((parent, _)) = to.rsplit_once('/') {
                    self.collapsed.remove(parent);
                }
            }
            Err(e) => {
                self.status_msg = format!("改名失败：{e}");
                self.status_at = Some(std::time::Instant::now());
                self.status_is_err = true;
            }
        }
        self.refresh_now
            .store(true, std::sync::atomic::Ordering::SeqCst);
        ctx.request_repaint();
    }

    pub(super) fn ask_delete(&mut self, row: &Row) {
        let items = if row.is_dir {
            let prefix = format!("{}/", row.rel);
            self.files
                .entries
                .iter()
                .filter(|e| e.rel.starts_with(&prefix))
                .count()
        } else {
            0
        };
        self.confirm_delete = Some(ConfirmDelete {
            rel: row.rel.clone(),
            name: row.name.clone(),
            is_dir: row.is_dir,
            items,
        });
    }

    fn draw_confirm(&mut self, ctx: &eframe::egui::Context) {
        let Some(c) = self.confirm_delete.as_ref() else {
            return;
        };
        let (rel, name, is_dir, items) =
            (c.rel.clone(), c.name.clone(), c.is_dir, c.items);
        let mut close = false;
        let mut go = false;
        eframe::egui::Window::new("确认删除")
            .collapsible(false)
            .resizable(false)
            .anchor(eframe::egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.add_space(4.0);
                if is_dir {
                    ui.label(
                        RichText::new(format!(
                            "文件夹「{name}」和里面的 {items} 个项目会一起删除。"
                        ))
                        .size(12.5)
                        .color(pal::now().fg),
                    );
                } else {
                    ui.label(
                        RichText::new(format!("「{name}」会被删除。"))
                            .size(12.5)
                            .color(pal::now().fg),
                    );
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("不可撤销，配额立即归还。")
                        .size(11.0)
                        .color(pal::now().btn_hover_close),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if btn_soft(ui, "取消", 12.0, 30.0, 88.0) {
                        close = true;
                    }
                    ui.with_layout(
                        eframe::egui::Layout::right_to_left(eframe::egui::Align::Center),
                        |ui| {
                            if btn_primary(ui, "删除", 12.0, 30.0, 88.0) {
                                go = true;
                            }
                        },
                    );
                });
            });
        if close {
            self.confirm_delete = None;
            return;
        }
        if go {
            self.confirm_delete = None;
            match crate::ops::delete(&self.cfg, &rel, "本机") {
                Ok(s) => {
                    self.status_at = Some(std::time::Instant::now());
                    self.status_msg = if s.is_dir {
                        format!("已删除文件夹 {}（{} 个文件）", s.rel, s.files)
                    } else {
                        format!("已删除 {}", s.rel)
                    };
                    self.status_is_err = false;
                    if self.selected.as_deref() == Some(rel.as_str()) {
                        self.selected = None;
                    }
                }
                Err(e) => {
                    self.status_msg = format!("删除失败：{e}");
                    self.status_at = Some(std::time::Instant::now());
                    self.status_is_err = true;
                }
            }
            self.refresh_now
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ctx.request_repaint();
        }
    }
}

/// 便签右上角的展开/收起按钮（自己画四角箭头，不依赖 emoji 字体）。
fn note_expand_icon(ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect, expanded: bool) -> bool {
    let resp = ui.interact(
        rect,
        eframe::egui::Id::new("td_note_toggle"),
        eframe::egui::Sense::click(),
    );
    let pal = pal::now();
    let color = if resp.hovered() {
        pal.accent
    } else {
        pal.muted
    };
    if resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, pal.soft_bg);
    }
    let c = rect.center();
    let stroke = eframe::egui::Stroke::new(1.3_f32, color);
    for (sx, sy) in [
        (-1.0_f32, -1.0_f32),
        (1.0, -1.0),
        (-1.0, 1.0),
        (1.0, 1.0),
    ] {
        let outer = eframe::egui::pos2(c.x + sx * 6.0, c.y + sy * 6.0);
        let inner = eframe::egui::pos2(c.x + sx * 2.0, c.y + sy * 2.0);
        // 展开：箭头朝四角；收起：箭头朝中心
        let (from, to) = if expanded { (inner, outer) } else { (outer, inner) };
        // 箭头两翼要画在“箭头背后”，所以收起态方向取反
        let (ax, ay) = if expanded { (sx, sy) } else { (-sx, -sy) };
        ui.painter().line_segment([from, to], stroke);
        ui.painter()
            .line_segment([eframe::egui::pos2(to.x - ax * 3.0, to.y), to], stroke);
        ui.painter()
            .line_segment([eframe::egui::pos2(to.x, to.y - ay * 3.0), to], stroke);
    }
    resp.on_hover_text(if expanded {
        "收起（回到便签条）"
    } else {
        "展开编辑"
    })
    .clicked()
}

/// 目录行的列边界：名称（弹性）/ 大小 / 行内按钮
fn dir_columns(r: eframe::egui::Rect) -> (
    eframe::egui::Rect,
    eframe::egui::Rect,
    eframe::egui::Rect,
) {
    let actions_w = 78.0;
    let size_w = 66.0;
    let actions = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(r.right() - actions_w, r.top()),
        eframe::egui::pos2(r.right(), r.bottom()),
    );
    let size = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(actions.left() - size_w, r.top()),
        eframe::egui::pos2(actions.left(), r.bottom()),
    );
    let name = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(r.left() + 4.0, r.top()),
        eframe::egui::pos2(size.left() - 6.0, r.bottom()),
    );
    (name, size, actions)
}

/// 设备表的列边界：IP / 空闲 / 开关（+ 断开）
fn dev_columns(r: eframe::egui::Rect) -> (
    eframe::egui::Rect,
    eframe::egui::Rect,
    eframe::egui::Rect,
) {
    let kick_w = 34.0;
    let sw_w = 34.0;
    let idle_w = 52.0;
    let right = r.right() - kick_w;
    let sw = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(right - sw_w, r.top()),
        eframe::egui::pos2(right, r.bottom()),
    );
    let idle = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(sw.left() - idle_w, r.top()),
        eframe::egui::pos2(sw.left(), r.bottom()),
    );
    let ip = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(r.left(), r.top()),
        eframe::egui::pos2(idle.left() - 4.0, r.bottom()),
    );
    (ip, idle, sw)
}

/// 详情表的列边界：操作/文件 / 大小 / 时间
fn act_columns(r: eframe::egui::Rect) -> (
    eframe::egui::Rect,
    eframe::egui::Rect,
    eframe::egui::Rect,
) {
    let size_w = 62.0;
    let time_w = 58.0;
    let time = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(r.right() - time_w, r.top()),
        eframe::egui::pos2(r.right(), r.bottom()),
    );
    let size = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(time.left() - size_w, r.top()),
        eframe::egui::pos2(time.left(), r.bottom()),
    );
    let name = eframe::egui::Rect::from_min_max(
        r.min,
        eframe::egui::pos2(size.left() - 6.0, r.bottom()),
    );
    (name, size, time)
}

/// 设备表里的「空闲」要短：3 分 / 26 分 / 刚刚
fn fmt_ago_short(secs: u64) -> String {
    if secs < 30 {
        "刚刚".to_string()
    } else if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分", secs / 60)
    } else {
        format!("{} 时", secs / 3600)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 图标确实能被点中（它只是叠在输入框上画的，没有占布局）。
    #[test]
    fn note_expand_icon_can_be_clicked() {
        crate::settings::set_theme(crate::settings::Theme::Light);
        let ctx = eframe::egui::Context::default();
        let mut style = (*ctx.style()).clone();
        style.visuals = eframe::egui::Visuals::light();
        ctx.set_style(style);

        let rect = eframe::egui::Rect::from_min_size(
            eframe::egui::pos2(40.0, 20.0),
            eframe::egui::vec2(18.0, 18.0),
        );
        let mut hits = 0;
        for frame in 0..3 {
            let mut input = eframe::egui::RawInput::default();
            input.screen_rect = Some(eframe::egui::Rect::from_min_size(
                eframe::egui::pos2(0.0, 0.0),
                eframe::egui::vec2(200.0, 100.0),
            ));
            input.time = Some(frame as f64 * 0.1);
            if frame >= 1 {
                input.events.push(eframe::egui::Event::PointerMoved(rect.center()));
            }
            if frame == 1 {
                input.events.push(eframe::egui::Event::PointerButton {
                    pos: rect.center(),
                    button: eframe::egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: eframe::egui::Modifiers::default(),
                });
            }
            if frame == 2 {
                input.events.push(eframe::egui::Event::PointerButton {
                    pos: rect.center(),
                    button: eframe::egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: eframe::egui::Modifiers::default(),
                });
            }
            let _ = ctx.run(input, |ctx| {
                eframe::egui::CentralPanel::default().show(ctx, |ui| {
                    if note_expand_icon(ui, rect, false) {
                        hits += 1;
                    }
                });
            });
        }
        assert!(hits > 0, "图标点击没命中");
    }
}
