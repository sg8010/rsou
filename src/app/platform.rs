//! 平台交互:用系统默认程序打开文件、在文件管理器中定位文件。
//!
//! 与 filebrowser 里的平台分支同一套约定:spawn 不等待,失败只返回错误文案
//! (由调用方写进界面提示),不 panic。

use std::path::Path;
use std::process::Command;

/// 用系统默认程序打开文件(或目录)。
pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;

        // 「双击打开」对应的系统 API 是 ShellExecuteW;cmd /c start 只是
        // 绕到它的跳板,还会为控制台子进程新开一个一闪而过的窗口。
        // 直接调它:不引 windows-sys 依赖,unsafe 收在这一处、可审计;
        // shell32.dll 自 Win95 起提供该函数,Win7 导入表检查无碍。
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: *mut core::ffi::c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show_cmd: i32,
            ) -> isize;
        }
        const SW_SHOWNORMAL: i32 = 1;

        let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // verb 传 null = 注册表默认动作(与双击/start 一致);hwnd 不需要。
        // 返回值 >32 为成功,≤32 是错误码(历史原因用 HINSTANCE 承载)。
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                std::ptr::null(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        return if result > 32 {
            Ok(())
        } else {
            Err(format!(
                "无法打开 {}: ShellExecute 错误码 {result}",
                path.display()
            ))
        };
    }
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
    // Windows 分支已在上面提前返回,这里只覆盖 macOS/Linux 两条进程路径。
    #[cfg(not(target_os = "windows"))]
    {
        command
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法打开 {}: {e}", path.display()))
    }
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
