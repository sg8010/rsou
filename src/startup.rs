//! 启动阶段诊断。
//!
//! eframe 在创建窗口和 OpenGL 上下文前不会显示应用自己的界面。桌面菜单启动时，
//! `Terminal=false` 又会把标准错误隐藏起来，因此这里把启动日志写入用户目录，
//! 并在 eframe 返回错误或启动超时时尝试弹出系统错误对话框。
//!
//! 覆盖不到的是 `main()` 之前的失败：动态链接器报错（缺少硬链接库、glibc 版本
//! 不够）和 exec 失败发生在进程进入 Rust 之前，本模块没有机会运行。Debian 包
//! 因此把 `usr/bin/rsou` 装成启动器脚本，由脚本把这些报错重定向进同一个
//! 日志文件（见 `assets/rsou-launcher.sh`）。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

/// 由启动器脚本设置的环境变量。
///
/// 脚本已经做过日志轮转，并把 exec / 动态链接阶段的 stderr 写进了新日志，所以
/// 程序自身改为追加写入，避免把发生在 `main()` 之前的证据截断掉。
const LAUNCHER_ENV: &str = "RSOU_LAUNCHER";

/// 图形环境初始化的最长等待时间。
///
/// 正常情况下 eframe 从启动到第一帧远小于这个时间。超时主要用于捕获
/// OpenGL/EGL 驱动调用卡死这一类不会返回 `Result` 的故障。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// 启动诊断状态。
pub struct StartupDiagnostics {
    log_path: Option<PathBuf>,
    file: Mutex<Option<File>>,
    failure_reported: AtomicBool,
    started_at: Instant,
}

impl StartupDiagnostics {
    /// 创建本次启动的诊断日志并记录运行环境。
    pub fn begin() -> Arc<Self> {
        let (log_path, file) = open_log_file();
        let diagnostics = Arc::new(Self {
            log_path,
            file: Mutex::new(file),
            failure_reported: AtomicBool::new(false),
            started_at: Instant::now(),
        });

        // eframe/glutin 使用 log crate 输出的 OpenGL 初始化细节对定位问题很有用。
        // 若其他组件已经注册 logger,不影响程序启动,仍会保留本模块的直接日志。
        if log::set_boxed_logger(Box::new(FileLogger(Arc::clone(&diagnostics)))).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }

        crash_handler::install(&diagnostics);
        diagnostics.write_line("启动诊断开始");
        diagnostics.write_environment();
        diagnostics
    }

    /// 启动日志路径,用于错误提示和用户反馈。
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// 写入一条不依赖 log logger 的诊断信息。
    pub fn write_line(&self, message: &str) {
        // 时间戳兼顾两件事:换算成可读的 UTC 时间便于和系统日志对齐,附带的
        // 启动耗时能看出某一步卡了多久(卡死类故障只有秒级时间戳是看不出来的)。
        let line = format!(
            "[{} +{:.3}s] {}\n",
            format_timestamp(SystemTime::now()),
            self.started_at.elapsed().as_secs_f64(),
            truncate_line(message)
        );

        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        // 诊断日志的首要目标是记录卡死前最后一步,每行立即 flush。
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }

    /// 启动一个轻量 watchdog。第一帧 UI 到达后会自动结束等待。
    pub fn spawn_watchdog(self: &Arc<Self>, ready: Arc<AtomicBool>) {
        let diagnostics = Arc::clone(self);
        std::thread::spawn(move || {
            // 分段等待,这样第一帧到达后无需等满 30 秒才结束线程。
            let slices = STARTUP_TIMEOUT.as_millis() / 250;
            for _ in 0..slices {
                if ready.load(Ordering::Acquire)
                    || diagnostics.failure_reported.load(Ordering::Acquire)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }

            if ready.load(Ordering::Acquire) {
                return;
            }

            let first_report = diagnostics.report_failure(
                "启动超过 30 秒仍未显示首个界面。程序可能卡在 X11 或 OpenGL/EGL 图形环境初始化阶段。",
            );
            if first_report {
                // 已经弹出错误提示后结束卡住的进程,避免桌面启动器一直显示忙碌状态。
                std::process::exit(1);
            }
        });
    }

    /// 安装 panic hook,确保 app creator 或首帧前的 panic 也能留下原因。
    pub fn install_panic_hook(self: &Arc<Self>, ready: &Arc<AtomicBool>) {
        let previous = std::panic::take_hook();
        let diagnostics = Arc::clone(self);
        let ready = Arc::clone(ready);
        std::panic::set_hook(Box::new(move |info| {
            let detail = panic_detail(info);
            if ready.load(Ordering::Acquire) {
                diagnostics.write_line(&format!("程序运行期间发生异常: {detail}"));
                if !stderr_goes_to_log() {
                    eprintln!("程序运行期间发生异常: {detail}");
                }
            } else {
                diagnostics.report_failure(&format!("启动期间发生异常: {detail}"));
            }
            previous(info);
        }));
    }

    /// 记录失败并尽量直接显示错误。返回 `true` 表示本次调用首次报告。
    pub fn report_failure(&self, error: &str) -> bool {
        if self.failure_reported.swap(true, Ordering::AcqRel) {
            // watchdog 先报超时、eframe 随后返回具体错误时,具体错误仍需进入日志。
            self.write_line(&format!("后续启动错误: {error}"));
            return false;
        }

        let message = failure_message(error, self.log_path());
        self.write_line(&format!("启动失败: {message}"));
        if !stderr_goes_to_log() {
            eprintln!("{message}");
        }
        show_failure_dialog(self, &message);
        true
    }

    fn write_environment(&self) {
        let version = option_env!("RSOU_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        self.write_line(&format!("版本: {version}"));
        self.write_line(&format!(
            "目标: {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        self.write_line(&format!(
            "可执行文件: {}",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "<无法获取>".to_owned())
        ));
        self.write_line(&format!(
            "工作目录: {}",
            std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "<无法获取>".to_owned())
        ));

        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_SESSION_TYPE",
            "XDG_CURRENT_DESKTOP",
            "XDG_RUNTIME_DIR",
            "LD_LIBRARY_PATH",
        ] {
            let value = std::env::var_os(key)
                .map(|value| value.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "<未设置>".to_owned());
            self.write_line(&format!("环境变量 {key}: {value}"));
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(kernel) = fs::read_to_string("/proc/sys/kernel/osrelease") {
                self.write_line(&format!("Linux 内核: {}", kernel.trim()));
            }
            if let Ok(os_release) = fs::read_to_string("/etc/os-release") {
                let summary = os_release
                    .lines()
                    .filter(|line| line.starts_with("PRETTY_NAME=") || line.starts_with("NAME="))
                    .take(2)
                    .collect::<Vec<_>>()
                    .join("; ");
                if !summary.is_empty() {
                    self.write_line(&format!("发行版: {summary}"));
                }
            }
            self.write_linux_library_status();
        }
    }

    #[cfg(target_os = "linux")]
    fn write_linux_library_status(&self) {
        // glutin、winit 为了兼容不同发行版通过 dlopen 加载这些库,所以 ldd
        // 看不到它们;这里仅作诊断记录,不把探测失败当作程序启动硬错误。
        const LIBRARIES: &[&str] = &[
            "libGL.so.1",
            "libEGL.so.1",
            "libX11.so.6",
            "libX11-xcb.so.1",
            "libxcb.so.1",
            "libxkbcommon.so.0",
            "libxkbcommon-x11.so.0",
            "libXcursor.so.1",
            "libXi.so.6",
            "libXrandr.so.2",
            "libXfixes.so.3",
            "libXrender.so.1",
        ];

        let output = ["/sbin/ldconfig", "ldconfig"].iter().find_map(|program| {
            Command::new(program)
                .arg("-p")
                .output()
                .ok()
                .filter(|output| output.status.success())
        });
        let Some(output) = output else {
            self.write_line("动态库检查: 无法执行 ldconfig -p");
            return;
        };
        let listing = String::from_utf8_lossy(&output.stdout);
        self.write_line("动态库检查(来自 ldconfig -p):");
        for library in LIBRARIES {
            let found = listing.lines().any(|line| line.contains(library));
            self.write_line(&format!(
                "  {library}: {}",
                if found { "存在" } else { "未找到" }
            ));
        }
    }
}

struct FileLogger(Arc<StartupDiagnostics>);

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // 跟随全局级别:启动阶段是 Debug(glutin/winit 的图形初始化细节),首帧
        // 呈现后收回到 Info,避免空闲时每帧的 winit debug 把日志写成一个巨大的文件。
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0
                .write_line(&format!("[{}] {}", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

/// 让不走 panic 的崩溃也能在日志里留下一条记录。
///
/// 显卡驱动在初始化或首帧呈现时崩溃(段错误)在目标设备上并不罕见,这类崩溃不经过
/// panic hook,日志会停在上一条里程碑上。信号处理器只能调用异步信号安全的函数,
/// 所以这里不做加锁、不做分配,直接把固定文案 `write(2)` 到日志文件的 fd。
#[cfg(target_os = "linux")]
mod crash_handler {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::StartupDiagnostics;

    /// 诊断日志的原始文件描述符,进程生命周期内一直有效。
    static LOG_FD: AtomicI32 = AtomicI32::new(-1);

    /// 需要留痕的致命信号。`SIGKILL` 无法捕获,被 OOM killer 杀掉时日志仍然只会
    /// 中断在最后一条记录上。
    const CRASH_SIGNALS: &[i32] = &[
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        libc::SIGABRT,
    ];

    pub fn install(diagnostics: &StartupDiagnostics) {
        let fd = match diagnostics.file.lock() {
            Ok(guard) => guard.as_ref().map_or(-1, |file| file.as_raw_fd()),
            Err(_) => -1,
        };
        LOG_FD.store(fd, Ordering::Release);

        for signal in CRASH_SIGNALS {
            // SAFETY: 处理器只做 write(2) 和重新抛出信号,不触碰锁与分配器。
            unsafe {
                libc::signal(*signal, handle as *const () as libc::sighandler_t);
            }
        }
    }

    /// 处理器地址,供单测核对内核里登记的确实是本模块的处理器。
    #[cfg(test)]
    pub fn handler_address() -> usize {
        handle as *const () as usize
    }

    extern "C" fn handle(signal: i32) {
        let mut buffer = [0u8; 192];
        let length = format_message(signal, &mut buffer);
        let fd = LOG_FD.load(Ordering::Acquire);
        if fd >= 0 {
            // SAFETY: buffer 是本地栈内存,fd 在进程生命周期内有效。此处刻意不加锁:
            // 处理器里不能加锁,可能与另一线程的 write_line 交错,但不会丢内容。
            unsafe {
                libc::write(fd, buffer.as_ptr().cast(), length);
            }
        }
        // 恢复默认处理并重新触发,保留 core dump 与"被信号杀死"的退出语义。
        // SAFETY: 只是把处理器换回默认值再抛出同一个信号。
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// 把崩溃提示格式化到调用方提供的缓冲区,返回写入长度。
    ///
    /// 处理器不能分配内存,所以按"写进栈缓冲区"实现;单测直接复用这个函数。
    pub fn format_message(signal: i32, buffer: &mut [u8]) -> usize {
        let mut cursor = 0;
        push(buffer, &mut cursor, "异常终止: 收到信号 ");
        push_decimal(buffer, &mut cursor, signal);
        push(buffer, &mut cursor, " (");
        push(buffer, &mut cursor, signal_name(signal));
        push(buffer, &mut cursor, "),上一行是崩溃前的最后一条记录\n");
        cursor
    }

    fn signal_name(signal: i32) -> &'static str {
        match signal {
            libc::SIGSEGV => "段错误",
            libc::SIGBUS => "总线错误",
            libc::SIGILL => "非法指令",
            libc::SIGFPE => "算术异常",
            libc::SIGABRT => "中止",
            _ => "未知信号",
        }
    }

    fn push(buffer: &mut [u8], cursor: &mut usize, text: &str) {
        for byte in text.as_bytes() {
            push_byte(buffer, cursor, *byte);
        }
    }

    fn push_decimal(buffer: &mut [u8], cursor: &mut usize, value: i32) {
        let mut digits = [0u8; 11];
        let mut length = 0;
        let mut remaining = value.abs();
        loop {
            digits[length] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            length += 1;
            if remaining == 0 {
                break;
            }
        }
        while length > 0 {
            length -= 1;
            push_byte(buffer, cursor, digits[length]);
        }
    }

    fn push_byte(buffer: &mut [u8], cursor: &mut usize, byte: u8) {
        if *cursor < buffer.len() {
            buffer[*cursor] = byte;
            *cursor += 1;
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod crash_handler {
    use super::StartupDiagnostics;

    pub fn install(_diagnostics: &StartupDiagnostics) {}
}

fn open_log_file() -> (Option<PathBuf>, Option<File>) {
    // 启动器脚本已经轮转过日志并写入了链接器/exec 阶段的报错,此时只能追加。
    let launched_by_script = std::env::var_os(LAUNCHER_ENV).is_some_and(|value| !value.is_empty());

    for path in log_candidates() {
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        let file = if launched_by_script {
            OpenOptions::new().create(true).append(true).open(&path)
        } else {
            // 保留上一次启动的日志作为 `startup.log.1`。偶发故障往往伴随一次
            // 成功启动,不轮转的话失败证据会被下一次启动直接覆盖掉。
            rotate_previous_log(&path);
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
        };
        if let Ok(file) = file {
            return (Some(path), Some(file));
        }
    }
    (None, None)
}

/// 日志候选路径。启动器脚本 `assets/rsou-launcher.sh` 使用同样的顺序,
/// 两边必须保持一致,否则脚本写进去的链接器报错会落在另一个文件里。
fn log_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(state_home) = non_empty_env_path("XDG_STATE_HOME") {
        candidates.push(state_home.join("rsou/startup.log"));
    }
    if let Some(home) = non_empty_env_path("HOME") {
        candidates.push(home.join(".cache/rsou/startup.log"));
    }
    candidates.push(PathBuf::from("/tmp/rsou-startup.log"));
    candidates
}

fn rotate_previous_log(path: &Path) {
    if path.is_file() {
        let _ = fs::rename(path, previous_log_path(path));
    }
}

fn previous_log_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// 由启动器设置:stderr 已经被重定向进日志文件,屏幕上不会再显示。
///
/// 此时程序自己再 `eprintln!` 同一段失败原因,只是把同样的内容在日志里写第二遍;
/// 但经脚本的终端分支(tee)或直接运行二进制时,stderr 仍连着终端,必须照常输出。
const STDERR_IN_LOG_ENV: &str = "RSOU_STDERR_IN_LOG";

fn stderr_goes_to_log() -> bool {
    std::env::var_os(STDERR_IN_LOG_ENV).is_some_and(|value| !value.is_empty())
}

/// 单条日志的长度上限。
///
/// winit 创建窗口时会把自己收到的整个 `WindowAttributes` 打进 debug 日志,其中的
/// 程序图标是一百多万字节的数组;原样写入会让日志变成几兆的数字墙,既没人看得懂,
/// 也让"保留上一次日志"变成磁盘负担。
const MAX_LOG_LINE: usize = 8192;

fn truncate_line(text: &str) -> String {
    if text.len() <= MAX_LOG_LINE {
        return text.to_owned();
    }
    // 截断点要落在字符边界上,否则会把多字节字符切成半个。
    let mut end = MAX_LOG_LINE;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…(本行过长,已省略 {} 字节)",
        &text[..end],
        text.len() - end
    )
}

/// 把时间格式化成 `2026-09-15T00:20:23Z`。
///
/// 只做 UTC,不查时区库:日志的目的和系统日志对齐和比较两次启动的先后,标注
/// 清楚时区即可,不必为此引入依赖。
fn format_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}

/// 天数(自 1970-01-01)→ 年月日。用 Howard Hinnant 的 `civil_from_days` 算法,
/// 只需整数运算,避免为了格式化日期引依赖。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn non_empty_env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn panic_detail(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("未知 panic 信息");
    match info.location() {
        Some(location) => format!("{payload} ({location})"),
        None => payload.to_owned(),
    }
}

fn failure_message(error: &str, log_path: Option<&Path>) -> String {
    let hint = explain_error(error);
    let log = log_path
        .map(|path| format!("诊断日志: {}", path.display()))
        .unwrap_or_else(|| "诊断日志: 无法创建日志文件,请从终端启动并查看错误输出".to_owned());
    format!("rsou 无法启动。\n\n可能原因:\n{hint}\n\n底层错误:\n{error}\n\n{log}")
}

fn explain_error(error: &str) -> &'static str {
    let error = error.to_ascii_lowercase();
    if error.contains("neither wayland")
        || error.contains("display is not set")
        || error.contains("wayland_display")
    {
        "未检测到可用的桌面显示会话。请从 UOS 桌面启动,并确认 DISPLAY 已设置;当前版本使用 X11,不支持仅有 Wayland 会话。"
    } else if error.contains("xkbcommon-x11") {
        "无法加载 libxkbcommon-x11.so,这是 UOS X11 输入支持的运行库。请安装发行版对应的 libxkbcommon-x11-0 运行包后重试。"
    } else if error.contains("libgl.so") || error.contains("libegl.so") {
        "无法加载 OpenGL/EGL 运行库。请检查 UOS 的 libgl1、libegl1 及显卡/Mesa 驱动是否完整安装。"
    } else if error.contains("xcursor")
        || error.contains("xkbcommon")
        || error.contains("x11")
        || error.contains("xcb")
    {
        "X11 运行库或 X11 扩展可能缺失/不可用,请检查 libX11、libxcb、libxkbcommon、libXcursor 等运行库。"
    } else if error.contains("glutin")
        || error.contains("opengl")
        || error.contains("egl")
        || error.contains("glx")
        || error.contains("shader")
        || error.contains("gl context")
    {
        "OpenGL/EGL/GLX 图形环境初始化失败,常见原因是显卡驱动、Mesa/图形运行库缺失或当前硬件不支持所需 OpenGL。"
    } else {
        "窗口或图形渲染环境初始化失败。请检查 UOS 桌面会话、X11/图形运行库及显卡驱动;诊断日志包含更具体的底层信息。"
    }
}

#[cfg(target_os = "linux")]
fn show_failure_dialog(diagnostics: &StartupDiagnostics, message: &str) {
    // 没有图形会话时,zenity 等工具可能等待很久才退出;此时直接保留 stderr
    // 和日志即可,不要让错误处理本身看起来像又卡住了。
    let has_display = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|key| {
        std::env::var_os(key)
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    });
    if !has_display {
        diagnostics.write_line("未检测到 DISPLAY/WAYLAND_DISPLAY,跳过系统错误提示");
        return;
    }

    let title = "rsou 启动失败";
    // 不把 zenity 等工具作为正常运行依赖;只在图形初始化失败、程序无法创建自己
    // 的窗口时尝试使用系统已有的错误提示工具。
    let dialogs: &[(&str, &[&str])] = &[
        (
            "zenity",
            &[
                "--error",
                "--no-markup",
                "--title",
                title,
                "--width",
                "760",
                "--text",
                message,
            ],
        ),
        ("kdialog", &["--title", title, "--error", message]),
        ("xmessage", &["-center", "-title", title, message]),
    ];
    // 每次尝试的结果都写进日志:目标机上可能一个提示工具都没有,用户其实什么都
    // 没看到,只有日志能说明这一点。
    for (program, args) in dialogs {
        if run_dialog(program, args) {
            diagnostics.write_line(&format!("已用 {program} 显示错误提示"));
            return;
        }
        diagnostics.write_line(&format!("{program} 不可用,换下一种提示方式"));
    }

    // 没有模态对话框工具时,通知和打开日志仍比静默退出更容易让用户发现原因。
    if run_dialog("notify-send", &["--urgency=critical", title, message]) {
        diagnostics.write_line("已用 notify-send 发送失败通知");
        return;
    }
    diagnostics.write_line("notify-send 不可用,换下一种提示方式");

    // 桌面环境基本都带终端模拟器,在终端里直接显示完整日志,比丢给用户一个
    // 不知道在哪的文件更容易让人看懂发生了什么。
    if let Some(log_path) = diagnostics.log_path() {
        let log_arg = log_path.display().to_string();
        let script =
            "cat -- \"$0\"; printf '\\n以上是本次启动的完整日志,按回车键关闭。\\n'; read _";
        let args = ["-e", "sh", "-c", script, log_arg.as_str()];
        if run_dialog("x-terminal-emulator", &args) {
            diagnostics.write_line("已用 x-terminal-emulator 显示日志");
            return;
        }
        diagnostics.write_line("x-terminal-emulator 不可用,改为直接打开日志文件");
    }

    // 模态对话框都不可用时,直接打开本次启动实际使用的日志文件。
    let mut log_paths = Vec::new();
    if let Some(path) = diagnostics.log_path() {
        log_paths.push(path.to_path_buf());
    }
    log_paths.extend(
        [
            Some(PathBuf::from("/tmp/rsou-startup.log")),
            non_empty_env_path("XDG_STATE_HOME").map(|p| p.join("rsou/startup.log")),
            non_empty_env_path("HOME").map(|p| p.join(".cache/rsou/startup.log")),
        ]
        .into_iter()
        .flatten(),
    );
    for path in log_paths {
        if path.is_file() && spawn_detached("xdg-open", &[path.as_os_str()]) {
            diagnostics.write_line(&format!("已用 xdg-open 打开日志 {}", path.display()));
            return;
        }
    }

    diagnostics.write_line("系统错误提示方式均不可用:用户看不到任何提示,信息只在本日志里");
}

#[cfg(not(target_os = "linux"))]
fn show_failure_dialog(_diagnostics: &StartupDiagnostics, _message: &str) {}

#[cfg(target_os = "linux")]
fn run_dialog(program: &str, args: &[&str]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    // 图形对话框打开后不能等待用户关闭,否则主进程不会及时结束;只等待很短时间
    // 判断工具是否因缺少库/无效显示变量立即失败。仍在运行则视为已成功交给桌面。
    for _ in 0..6 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn spawn_detached(program: &str, args: &[&std::ffi::OsStr]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_missing_display() {
        assert!(explain_error("neither WAYLAND_DISPLAY nor DISPLAY is set").contains("显示会话"));
    }

    #[test]
    fn explains_graphics_failure() {
        assert!(explain_error("glutin error: failed to create EGL context").contains("OpenGL"));
    }

    #[test]
    fn explains_missing_xkbcommon_x11() {
        assert!(
            explain_error("Library libxkbcommon-x11.so could not be loaded")
                .contains("libxkbcommon-x11-0")
        );
    }

    #[test]
    fn failure_message_contains_raw_error_and_log() {
        let message = failure_message("测试底层错误", Some(Path::new("/tmp/test.log")));
        assert!(message.contains("测试底层错误"));
        assert!(message.contains("/tmp/test.log"));
    }

    #[test]
    fn formats_timestamp_as_readable_utc() {
        let time = UNIX_EPOCH + Duration::from_secs(1_789_431_623);
        assert_eq!(format_timestamp(time), "2026-09-15T00:20:23Z");
    }

    #[test]
    fn formats_epoch_and_leap_day() {
        assert_eq!(format_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2000-02-29 是闰日,用它验证月份回绕(1、2 月要算作上一年)。
        let leap_day = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(format_timestamp(leap_day), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn rotates_previous_log_instead_of_losing_it() {
        let dir = temp_dir("rotate");
        let path = dir.join("startup.log");
        fs::write(&path, "上一轮启动的失败证据").unwrap();

        rotate_previous_log(&path);

        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(previous_log_path(&path)).unwrap(),
            "上一轮启动的失败证据"
        );
        // 没有日志文件时轮转不应报错。
        rotate_previous_log(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn previous_log_path_keeps_directory() {
        assert_eq!(
            previous_log_path(Path::new("/home/u/.cache/rsou/startup.log")),
            PathBuf::from("/home/u/.cache/rsou/startup.log.1")
        );
    }

    /// 崩溃文案是信号处理器里直接写 fd 的内容,这里验证信号名和编号都在里面。
    #[cfg(target_os = "linux")]
    #[test]
    fn crash_message_names_the_signal() {
        let mut buffer = [0u8; 192];
        let length = crash_handler::format_message(libc::SIGSEGV, &mut buffer);
        let text = std::str::from_utf8(&buffer[..length]).unwrap();
        assert!(text.contains("信号 11"), "实际内容: {text}");
        assert!(text.contains("段错误"), "实际内容: {text}");
        assert!(text.ends_with('\n'));
    }

    /// 缓冲区不足时只截断,不能越界。
    #[cfg(target_os = "linux")]
    #[test]
    fn crash_message_truncates_safely() {
        let mut small = [0u8; 8];
        let length = crash_handler::format_message(libc::SIGABRT, &mut small);
        assert_eq!(length, small.len());
    }

    /// 只核对内核里登记的处理器是哪一个,不真的抛信号:按设计,抛了会终止测试进程。
    #[cfg(target_os = "linux")]
    #[test]
    fn crash_handler_replaces_default_disposition() {
        let dir = temp_dir("crash-handler");
        let path = dir.join("startup.log");
        let diagnostics = StartupDiagnostics {
            log_path: Some(path.clone()),
            file: Mutex::new(Some(File::create(&path).unwrap())),
            failure_reported: AtomicBool::new(false),
            started_at: Instant::now(),
        };

        crash_handler::install(&diagnostics);

        for signal in [libc::SIGSEGV, libc::SIGABRT, libc::SIGBUS] {
            // SAFETY: query 时传 null 表示只读取当前设置,不修改。
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigaction(signal, std::ptr::null(), &mut current);
            }
            assert_eq!(
                current.sa_sigaction,
                crash_handler::handler_address(),
                "信号 {signal} 没有登记崩溃处理器"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncates_overlong_log_lines() {
        assert_eq!(truncate_line("短行"), "短行");

        let long = "a".repeat(MAX_LOG_LINE + 100);
        let truncated = truncate_line(&long);
        assert!(truncated.starts_with(&"a".repeat(MAX_LOG_LINE)));
        assert!(truncated.contains("已省略 100 字节"));

        // 截断点必须落在字符边界上,不能把多字节字符切成半个。
        let cjk = "中".repeat(MAX_LOG_LINE);
        let truncated = truncate_line(&cjk);
        assert!(truncated.starts_with('中'));
        assert!(truncated.contains("已省略"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsou-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
