//! 内置天气插件：JSONL 入口，HTTP 由宿主代理。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod cache;
mod conditions;
mod location;
mod model;
mod presentation;
mod protocol;
mod providers;
mod service;
mod settings;

fn main() {
    service::run();
}
