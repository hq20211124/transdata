#![windows_subsystem = "windows"]
mod config;

mod gui;
mod http;
mod net;
mod tls;
mod web;

use config::Config;

fn main() {
    let cfg = std::sync::Arc::new(Config::new());
    gui::run_gui(cfg.clone(), cfg.port);
}
