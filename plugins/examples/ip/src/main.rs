//! IP 查询：网卡快照、国内/国际出口独立主备，联网复用宿主 HTTP。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod model;
mod network;
mod presentation;
mod protocol;
mod providers;
mod service;
mod settings;

fn main() {
    service::run();
}
