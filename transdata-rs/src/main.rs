#![windows_subsystem = "windows"]
mod config;
mod gui;
mod http;
mod instance;
mod net;
mod settings;
mod tls;
mod update;
mod web;

use config::Config;

fn main() {
    // 单实例：第二个实例启动时会清理传输目录里的 `.td-tmp-*.part`，而那可能
    // 正是第一个实例正在写的上传中间文件 —— 所以先拿锁，拿不到就别启动。
    let _instance = match instance::acquire() {
        Some(lock) => lock,
        None => {
            gui::show_error(
                "TransData 已经在运行了。\n\n请切换回已有的窗口，或先把它关掉再启动。",
            );
            return;
        }
    };

    let cfg = std::sync::Arc::new(Config::new());
    // 拿到锁之后才清理残留（见 Config::cleanup_stale_files 的说明）。
    cfg.cleanup_stale_files();
    gui::run_gui(cfg.clone(), cfg.port);
}
