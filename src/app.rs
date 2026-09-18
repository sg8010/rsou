//! rsou 主应用界面 (egui)
//!
//! 本文件只放状态类型与 `App::ui` 入口;子模块按职责拆分:
//! - `shell`:侧栏 + 顶栏 + 工作区滚动容器
//! - `theme`:色板、通用控件、全局样式
//! - `workers`:构造、索引库打开、后台任务轮询与对话框编排
//! - `page_library`/`page_search`/`page_settings`:三个页面

mod page_library;
mod page_search;
mod page_settings;
mod shell;
mod theme;
mod workers;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};
use rsou_lib::store::{self, DataDirs, OpenMode};
use rusqlite::Connection;

#[cfg(target_os = "linux")]
use crate::file_dialog::{self, DialogAction, FileDialog};

/// 侧栏条目对应的页面。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    /// 资料库:导入与文档管理
    Library,
    /// 检索:全文搜索与预览
    Search,
    /// 设置:数据目录、索引维护与关于
    Settings,
}

impl Page {
    pub(crate) const ALL: [Page; 3] = [Page::Library, Page::Search, Page::Settings];

    fn title(self) -> &'static str {
        match self {
            Self::Library => "资料库",
            Self::Search => "检索",
            Self::Settings => "设置",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Library => "导入并管理要检索的文档",
            Self::Search => "在已索引文档中全文查找",
            Self::Settings => "数据目录、索引维护与关于",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Library => "把文档放进资料库,之后即可全文检索。",
            Self::Search => "输入关键词,在全部已索引文档中查找。",
            Self::Settings => "查看数据位置,维护索引文件。",
        }
    }
}

/// 待发起的文件对话框请求。
///
/// 不在点击处直接弹窗:系统原生对话框(rfd)是阻塞调用,统一放到帧末处理;
/// Linux 的内置对话框也走同一条路,保证两条实现的行为一致。
#[derive(Clone, Copy)]
pub(crate) enum DialogRequest {
    /// 为资料库选择要导入的文档
    ImportFiles,
}

/// 正在显示的内置对话框(Linux)
#[cfg(target_os = "linux")]
pub(crate) struct ActiveDialog {
    request: DialogRequest,
    dialog: FileDialog,
}

pub struct RsouApp {
    /// 当前页面:主工作区一次只展示一个页面。
    page: Page,
    /// 写连接(导入线程以后独占;GUI 只做建库与统计查询)。
    /// None = 打开失败,原因在 `db_error`。
    db: Option<Connection>,
    /// 索引库打开/建表失败的中文原因(不 panic,直接显示在页面上)
    db_error: Option<String>,
    /// 数据目录布局(data_dir / db_path / tmp_dir)
    dirs: DataDirs,
    /// 启动日志路径(设置页展示用)
    startup_log_path: Option<PathBuf>,
    /// 资料库页的操作提示(文件已选中等)
    library_notice: Option<String>,
    /// 文档总数缓存(顶栏徽标;每帧刷新一次,空库代价可忽略)
    doc_count: Option<i64>,
    /// 是否有导入任务在后台进行(阶段 2 接入;侧栏状态点的语义就是「有在途任务」)
    import_active: bool,
    /// 是否有检索任务在后台进行(阶段 3 接入)
    search_active: bool,
    /// 是否有索引维护任务在后台进行(阶段 4 接入)
    maintenance_active: bool,
    /// 已点击待处理的对话框请求(帧末统一处理)
    pending_dialog: Option<DialogRequest>,
    /// 内置文件对话框(Linux;其他平台用系统原生 rfd 对话框)
    #[cfg(target_os = "linux")]
    dialog: Option<ActiveDialog>,
    /// 内置对话框上次停留的目录(下次从这里打开)
    #[cfg(target_os = "linux")]
    last_dir: Option<PathBuf>,
    /// 启动诊断用:首帧真正呈现到屏幕后才标记启动完成。
    startup_ready: Option<Arc<AtomicBool>>,
    /// 启动诊断用:已进入的帧数。第二帧开始时说明首帧已经完成呈现。
    startup_frames: u32,
}

impl RsouApp {
    fn new_state() -> Self {
        Self {
            page: Page::Library,
            db: None,
            db_error: None,
            dirs: store::data_dirs(),
            startup_log_path: None,
            library_notice: None,
            doc_count: None,
            import_active: false,
            search_active: false,
            maintenance_active: false,
            pending_dialog: None,
            #[cfg(target_os = "linux")]
            dialog: None,
            #[cfg(target_os = "linux")]
            last_dir: None,
            startup_ready: None,
            startup_frames: 0,
        }
    }

    /// 该页是否有在途后台任务(侧栏状态点的语义)。
    pub(crate) fn page_inflight(&self, page: Page) -> bool {
        match page {
            Page::Library => self.import_active,
            Page::Search => self.search_active,
            Page::Settings => self.maintenance_active,
        }
    }

    /// 是否有任何在途后台任务(决定是否主动请求重绘)。
    fn has_inflight(&self) -> bool {
        self.import_active || self.search_active || self.maintenance_active
    }
}

impl eframe::App for RsouApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("rsou_sidebar")
            .exact_size(224.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Self::navy())
                    .inner_margin(egui::Margin::symmetric(16, 25)),
            )
            .show(ui, |ui| self.ui_sidebar(ui));

        egui::Panel::top("topbar")
            .exact_size(66.0)
            .frame(
                egui::Frame::new()
                    .fill(Self::white())
                    .stroke(Stroke::new(1.0, Self::line()))
                    .inner_margin(egui::Margin::symmetric(35, 0)),
            )
            .show(ui, |ui| self.ui_topbar(ui));

        egui::CentralPanel::default()
            // 在导航栏与主工作区之间保留独立的浅色留白，避免内容贴边。
            .frame(
                egui::Frame::new()
                    .fill(Self::canvas())
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("workspace_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        self.ui_workspace(ui);
                    });
            });

        // 帧末:统一 poll 后台任务结果(非阻塞;阶段 2/3 的导入与检索 worker 都挂在这里)
        self.poll_workers();
        // 帧末:处理对话框(rfd 是阻塞调用,必须放在帧末;内置对话框也统一在这里画)
        self.drive_dialog(ui.ctx());
        // 只有在途任务需要主动重绘;空闲时不要求帧,避免无谓占 CPU。
        if self.has_inflight() {
            ui.ctx().request_repaint();
        }

        // 进入第二帧才宣告启动完成:第一帧的绘制和呈现发生在 ui() 返回之后,若图形
        // 驱动卡在首帧的缓冲交换里,ui() 不会再被调用,watchdog 仍能按启动超时报告,
        // 而不会因为标记过早置位、日志里反而写着"已经启动完成"。
        if self.startup_ready.is_some() {
            self.startup_frames = self.startup_frames.saturating_add(1);
            if self.startup_frames >= 2 {
                if let Some(startup_ready) = self.startup_ready.take() {
                    startup_ready.store(true, Ordering::Release);
                }
                // 图形初始化已经成功,后面每帧的 winit/glutin debug 只会让日志迅速膨胀。
                log::set_max_level(log::LevelFilter::Info);
                log::info!("界面已显示,启动完成");
            } else {
                // 界面静止时 egui 不会自己重绘,必须主动要一帧,否则健康运行的窗口
                // 可能一直不进入第二帧,反倒被看门狗当成启动超时杀掉。
                ui.ctx().request_repaint();
            }
        }
    }
}

/// 把「可能是最后一份引用」的对象整体移交后台线程析构。
///
/// 大型结果(整篇文档文本、分块列表)的析构要遍历大量堆块;若发生在 UI 线程上,
/// 替换/清空时会带来可感知的帧停顿。注意必须 move 整个持有者——只有最后一份
/// 引用进了线程,析构才真的发生在后台。线程创建失败时闭包在调用线程上析构,
/// 等价于原地 drop。
///
/// 阶段 2/3 的后台任务消息与大型结果清理都经它释放;本阶段暂无调用点。
#[allow(dead_code)]
fn drop_in_background<T: Send + 'static>(value: T) {
    if let Err(error) = std::thread::Builder::new()
        .name("rsou-drop".to_owned())
        .spawn(move || drop(value))
    {
        log::warn!("后台释放线程创建失败,改为当前线程释放: {error}");
    }
}

/// 安装 CJK 字体(运行时从系统加载,避免二进制膨胀)
fn install_cjk_font(ctx: &egui::Context) {
    use egui::FontDefinitions;

    let mut fonts = FontDefinitions::default();
    // msyh.ttc 的第 1 个 face 是 Microsoft YaHei UI，更接近 Windows 普通桌面控件。
    // 后续字体仅在前一个文件不存在时作为整套界面的回退字体。
    let candidates: &[(&str, u32)] = if cfg!(windows) {
        &[
            ("C:\\Windows\\Fonts\\msyh.ttc", 1),
            ("C:\\Windows\\Fonts\\simhei.ttf", 0),
            ("C:\\Windows\\Fonts\\simsun.ttc", 0),
        ]
    } else {
        &[
            // NotoSansCJK-Regular.ttc 的 face 2 是简体中文(SC); face 0 是日文(JP)。
            ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 2),
            ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 2),
            ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
            ("/usr/share/fonts/wqy-microhei/wqy-microhei.ttc", 0),
        ]
    };
    let mut loaded = false;
    for &(path, face_index) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut data = egui::FontData::from_owned(bytes);
            data.index = face_index;
            fonts
                .font_data
                .insert("system_ui".to_owned(), std::sync::Arc::new(data));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                let family_fonts = fonts.families.entry(family).or_default();
                if cfg!(windows) {
                    // 不再让 Ubuntu-Light/Hack 与中文字体混排，避免字宽、字重和基线不一致。
                    family_fonts.insert(0, "system_ui".to_owned());
                } else {
                    family_fonts.push("system_ui".to_owned());
                }
            }
            loaded = true;
            log::info!("已加载界面字体: {path} (face_index={face_index})");
            break;
        }
    }
    if !loaded {
        log::warn!("未找到预设 CJK 字体,将使用 egui 默认字体");
    }
    ctx.set_fonts(fonts);
}
