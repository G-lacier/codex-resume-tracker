#![forbid(unsafe_code)]

pub mod app_server;
pub mod config;
pub mod db;
pub mod doctor;
pub mod hook;
pub mod paths;
pub mod setup;
pub mod summary;
pub mod sync;
pub mod terminal;
pub mod tui;

pub const APP_NAME: &str = "codex-resume-tracker";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
