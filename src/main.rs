#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
#[cfg(target_os = "linux")]
mod file_dialog;
mod startup;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use app::RsouApp;
use eframe::egui;
use startup::StartupDiagnostics;

/// 程序图标(窗口 / 任务栏)。读取 assets/icon.png,失败时退化为默认图标。
fn app_icon() -> Option<Arc<egui::IconData>> {
    let bytes = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));
    match eframe::icon_data::from_png_bytes(bytes) {
        Ok(icon) => Some(Arc::new(icon)),
        Err(e) => {
            eprintln!("警告: 加载程序图标失败: {e}");
            None
        }
    }
}

fn main() {
    let diagnostics = StartupDiagnostics::begin();
    let startup_ready = Arc::new(AtomicBool::new(false));
    diagnostics.install_panic_hook(&startup_ready);
    diagnostics.spawn_watchdog(Arc::clone(&startup_ready));
    let startup_log_path = diagnostics.log_path().map(|p| p.to_path_buf());

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([1024.0, 680.0])
        .with_clamp_size_to_monitor_size(true);
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    diagnostics.write_line("开始创建 eframe 窗口和 OpenGL 上下文");
    let result = eframe::run_native(
        "rsou",
        options,
        Box::new({
            let startup_ready = Arc::clone(&startup_ready);
            move |cc| {
                Ok(Box::new(RsouApp::new_with_startup_marker(
                    cc,
                    startup_ready,
                    startup_log_path,
                )))
            }
        }),
    );
    diagnostics.write_line("eframe 窗口事件循环已返回");
    if let Err(error) = result {
        diagnostics.report_failure(&error.to_string());
        std::process::exit(1);
    }
}
