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
mod platform;
mod shell;
mod theme;
mod util;
mod workers;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};
use rsou_lib::import::ImportEvent;
use rsou_lib::query::Scope;
use rsou_lib::repo::{DocumentRow, ImportCounts};
use rsou_lib::search::SearchResponse;
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
    /// 为资料库选择一个文件夹,递归导入
    ImportFolder,
}

/// 导入进度(界面展示用)。
#[derive(Default)]
pub(crate) struct ImportProgress {
    pub counts: ImportCounts,
    /// 最近处理完的文件
    pub current_path: Option<PathBuf>,
    /// 最近的失败(文件名 + 中文原因),最多留 20 条
    pub recent_failures: VecDeque<(String, String)>,
    pub cancelled: bool,
}

/// 检索 worker 回传的消息(带世代号,过期结果丢弃)。
pub(crate) struct SearchMsg {
    pub generation: u64,
    pub result: Result<SearchResponse, String>,
}

/// 预览文本 worker 回传的消息。
pub(crate) struct PreviewMsg {
    pub generation: u64,
    pub document_id: i64,
    pub text: Option<String>,
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
    /// 界面连接:启动时 ReadWrite 打开(保证空库能建表),之后承担读查询
    /// 与零散小写(移除文档);导入写库走导入线程自己的连接,不共享这条。
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
    /// 文档列表缓存(资料库页表格;不每帧查库,按事件刷新)
    documents: Vec<DocumentRow>,
    /// 解析失败的文档(失败清单抽屉;与 documents 同一次刷新)
    failed_documents: Vec<DocumentRow>,
    /// 文件名过滤(内存过滤缓存列表)
    doc_filter: String,
    /// 「显示失败清单」抽屉开关
    show_failures: bool,
    /// 上一个页面(进资料库页时刷新文档列表用)
    prev_page: Page,
    /// 导入进度(进度卡与顶栏徽标)
    import_progress: ImportProgress,
    /// 导入线程事件通道(世代号随通道存,防上一轮残留接收端被误用)
    import_rx: Option<(u64, Receiver<ImportEvent>)>,
    /// 导入取消标志(GUI 置位,worker 在文件粒度上停)
    import_cancel: Option<Arc<AtomicBool>>,
    /// 导入世代号
    import_gen: u64,
    /// 还在运行的后台线程(join 在 Drop 时做)
    workers: Vec<JoinHandle<()>>,
    /// 是否有导入任务在后台进行(侧栏状态点的语义就是「有在途任务」)
    import_active: bool,
    /// 是否有检索任务在后台进行
    search_active: bool,
    /// 检索输入框内容
    search_query: String,
    /// 检索范围(全部/标题/正文)
    search_scope: Scope,
    /// 精确模式开关(默认开;关 = jieba 宽松;feature 关闭时恒为精确)
    search_exact: bool,
    /// 文件类型过滤(file_type 取值集合;空 = 不限)
    search_types: std::collections::BTreeSet<String>,
    /// 目录前缀过滤(规范化路径前缀)
    search_path_prefix: String,
    /// 最近一次检索结果(新结果回来前保留展示)
    search_result: Option<SearchResponse>,
    /// 检索/语法错误文案(状态行显示,不弹窗)
    search_error: Option<String>,
    /// 检索结果通道(世代号随通道存)
    search_rx: Option<(u64, Receiver<SearchMsg>)>,
    /// 检索世代号
    search_gen: u64,
    /// 预览面板的文档 id
    preview_doc_id: Option<i64>,
    /// 预览文本(plain_text;大文档渲染时按命中窗口截断)
    preview_text: Option<String>,
    /// 预览文本通道
    preview_rx: Option<(u64, Receiver<PreviewMsg>)>,
    /// 预览世代号
    preview_gen: u64,
    /// 预览正在加载中
    preview_loading: bool,
    /// 点片段后待滚动的 plain_text 字节偏移(渲染一次后清除)
    pending_scroll: Option<usize>,
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
            documents: Vec::new(),
            failed_documents: Vec::new(),
            doc_filter: String::new(),
            show_failures: false,
            prev_page: Page::Library,
            import_progress: ImportProgress::default(),
            import_rx: None,
            import_cancel: None,
            import_gen: 0,
            workers: Vec::new(),
            import_active: false,
            search_active: false,
            search_query: String::new(),
            search_scope: Scope::All,
            search_exact: true,
            search_types: std::collections::BTreeSet::new(),
            search_path_prefix: String::new(),
            search_result: None,
            search_error: None,
            search_rx: None,
            search_gen: 0,
            preview_doc_id: None,
            preview_text: None,
            preview_rx: None,
            preview_gen: 0,
            preview_loading: false,
            pending_scroll: None,
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

impl Drop for RsouApp {
    fn drop(&mut self) {
        // 有在途导入时先置取消再 join:关窗不等 worker 跑完长任务
        if let Some(cancel) = &self.import_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
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
                        // 进入资料库页时刷新文档列表缓存(不每帧查库)
                        if self.page == Page::Library && self.prev_page != Page::Library {
                            self.refresh_documents();
                        }
                        self.prev_page = self.page;
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
/// 文档列表缓存换代时的旧列表经它释放。
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
