//! 平台交互:用系统默认程序打开文件、在文件管理器中定位文件。
//!
//! 与 filebrowser 里的平台分支同一套约定:spawn 不等待,失败只返回错误文案
//! (由调用方写进界面提示),不 panic。

use std::path::Path;
use std::process::Command;

/// 用系统默认程序打开文件(或目录)。
pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        // start 需要一个窗口标题占位参数
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", &path.to_string_lossy()]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开 {}: {e}", path.display()))
}

/// 在文件管理器中显示该文件所在位置。
pub fn reveal_in_folder(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // explorer /select, 要接的是一条参数而不是分开的两个
        Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法打开所在目录 {}: {e}", path.display()))
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法打开所在目录 {}: {e}", path.display()))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux 没有可靠的 select-in-folder,退化为打开父目录
        let dir = path.parent().unwrap_or(path);
        Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法打开所在目录 {}: {e}", dir.display()))
    }
}
