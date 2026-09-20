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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};
use rsou_lib::dict::DictReport;
use rsou_lib::import::ImportEvent;
use rsou_lib::maintain::IndexStats;
use rsou_lib::query::Scope;
use rsou_lib::repo::{DocumentRow, ImportCounts};
use rsou_lib::search::{SearchResponse, Span};
use rsou_lib::store::{self, DataDirs, OpenMode};
use rusqlite::Connection;
use shell::SIDEBAR_MARGIN;

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
            Self::Search => "搜索",
            Self::Settings => "设置",
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

/// 资料库列表页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LibraryTab {
    #[default]
    Folders,
    Files,
}

/// 一个「已添加文件夹」及其下属文档。
#[derive(Debug, Clone)]
pub(crate) struct FolderGroup {
    /// 规范化绝对路径(即 documents.source_root)
    pub root: String,
    pub documents: Vec<DocumentRow>,
}

impl FolderGroup {
    /// 未命中过滤(或原名包含过滤词)的文档数。
    pub fn matched(&self, filter: &str) -> usize {
        self.documents
            .iter()
            .filter(|d| file_name_matches(&d.file_name, filter))
            .count()
    }
}

/// 树形视图的节点标识(egui_ltreeview 需要 Clone+Eq+Hash)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LibraryNode {
    /// 一个来源文件夹(值是 source_root 路径)
    Folder(String),
    /// 一篇文档(值是 documents.id)
    Document(i64),
}

/// 「单独文件」页该展示哪些文档——返回在 `documents` 里的下标。
///
/// 抽成纯函数是因为这条规则很容易写漏:`documents` 里同时含文件夹带来的文档,
/// 本页必须**只**保留 `source_root` 为空的。漏掉这一条的表现是「数目对、
/// 但列表把文件夹的子文件也列了出来」——数目由另一处算出,列表却来自这里,
/// 所以表面上不容易发现。
///
/// 注意这个函数不管「文件夹」页:那一页是树,数据来自 `folder_groups`
/// (已按 source_root 分组),不走下标过滤。
pub(crate) fn standalone_document_indices(documents: &[DocumentRow], filter: &str) -> Vec<usize> {
    documents
        .iter()
        .enumerate()
        .filter(|(_, d)| d.source_root.is_none() && file_name_matches(&d.file_name, filter))
        .map(|(i, _)| i)
        .collect()
}

/// 文档文件名是否命中过滤词(空词 = 全部命中)。
///
/// 抽成函数是为了让「文件夹页按匹配数隐藏空文件夹」与「单独文件页过滤」
/// 用同一条规则,不会出现两处不一致。
pub(crate) fn file_name_matches(file_name: &str, filter: &str) -> bool {
    filter.is_empty() || file_name.to_lowercase().contains(filter)
}

/// 需要二次确认的危险操作。
///
/// 所有「移除」都必须经过它:选中的动作先落到 `RsouApp::pending_confirm`,
/// 由 `ui_confirm_modal` 弹模态框,确认后才真执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingConfirm {
    /// 移除单个文档
    RemoveDocument { id: i64, label: String },
    /// 移除整个文件夹(连同其下全部文档的索引)
    RemoveFolder { root: String, doc_count: usize },
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

/// 一次文档列表读取的全部结果。
///
/// 一个结构体而不是元组:字段加到三个之后,元组位置靠数,很容易搞错顺序。
pub(crate) struct DocsSnapshot {
    /// 全部文档(单独文件页与旧调用方用)
    pub documents: Vec<DocumentRow>,
    /// 失败清单
    pub failed: Vec<DocumentRow>,
    /// 文件夹页用的分组
    pub folder_groups: Vec<FolderGroup>,
}

/// 文档列表 worker 回传的消息(文档列表 + 失败清单 + 文件夹分组)。
pub(crate) struct DocsMsg {
    pub generation: u64,
    pub result: Result<DocsSnapshot, String>,
}

/// 预览高亮定位缓存:同一预览文本、同一窗口、同一组字面量时不重算 locate_literals。
pub(crate) struct PreviewSpanCache {
    pub preview_gen: u64,
    pub base: usize,
    pub window_len: usize,
    pub literals: Vec<String>,
    pub spans: Vec<Span>,
}

/// 索引维护任务的种类(设置页四个按钮一一对应)。
#[derive(Clone, Copy)]
pub(crate) enum MaintainKind {
    Check,
    Rebuild,
    Optimize,
    Clear,
}

/// 维护 worker 回传的消息。`inconsistent` 仅在 kind==Check 时有意义:
/// 检查不一致时卡片上额外出现「立即重建」按钮。
pub(crate) struct MaintainMsg {
    pub generation: u64,
    pub kind: MaintainKind,
    pub result: Result<String, String>,
    pub inconsistent: bool,
}

/// 重建进度(维护线程写、UI 线程读)。
#[derive(Default)]
pub(crate) struct MaintainProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
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
    /// 界面上下文(后台线程读完数据后 request_repaint 用)
    egui_ctx: egui::Context,
    /// 启动日志路径(设置页展示用)
    startup_log_path: Option<PathBuf>,
    /// 资料库页的操作提示(文件已选中等)
    library_notice: Option<String>,
    /// 文档列表缓存(资料库页表格;后台线程读取,按事件刷新)
    documents: Vec<DocumentRow>,
    /// 解析失败的文档(失败清单抽屉;与 documents 同一次刷新)
    failed_documents: Vec<DocumentRow>,
    /// documents 换代号:每次替换/修改 +1,过滤缓存据它失效
    documents_version: u64,
    /// 过滤后的 documents 下标(表格渲染复用;filtered_key 命中时不重算)
    filtered_docs: Vec<usize>,
    /// 过滤缓存键:(小写过滤词, documents_version)
    filtered_key: Option<(String, u64)>,
    /// 文档列表后台读取通道(世代号随通道存)
    docs_rx: Option<(u64, Receiver<DocsMsg>)>,
    /// 文档列表读取世代号
    docs_gen: u64,
    /// 文档列表正在后台读取
    docs_loading: bool,
    /// 有读取在途时又收到刷新请求:在途结果落地后补读一次
    docs_refresh_pending: bool,
    /// 导入中有文件处理完(需要节流刷新列表)
    docs_dirty: bool,
    /// 上次文档列表读取落地时间(节流用)
    docs_last_refresh: Option<std::time::Instant>,
    /// 文件名过滤(内存过滤缓存列表)
    doc_filter: String,
    /// 资料库列表页签:文件夹 / 单独文件
    library_tab: LibraryTab,
    /// 来源文件夹分组缓存(文件夹页树形视图)
    folder_groups: Vec<FolderGroup>,
    /// 树形视图的折叠/选择状态(egui_ltreeview,跨帧保留)
    library_tree_state: egui_ltreeview::TreeViewState<LibraryNode>,
    /// 过滤期间被自动展开的文件夹节点;过滤清空后收回为关,
    /// 只记「原本没开」的节点,不覆盖用户手动展开的状态
    filter_auto_opened: Vec<LibraryNode>,
    /// 右侧失败清单开关,仅由用户操作改变
    show_failures: bool,
    failure_filter: Option<page_library::FailureCategory>,
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
    /// 时间范围过滤(None = 全部;Some(N) = 最近 N 天)
    search_mtime_days: Option<u64>,
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
    /// 当前搜索内每个内容组手动选择的位置。
    search_locations: std::collections::HashMap<i64, i64>,
    /// 正在展开路径选择浮层的内容组。
    location_popup_group: Option<i64>,
    /// 当前预览的命中批次(对应检索结果中保存的命中片段)
    preview_hit_index: usize,
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
    /// 预览窗口的稳定中心(plain_text 字节偏移;只在 focus_preview 时更新)
    preview_anchor: usize,
    /// 预览高亮区间缓存(窗口/字面量不变时复用 locate_literals 结果)
    preview_spans_cache: Option<PreviewSpanCache>,
    /// 索引统计缓存(设置页卡片;进入设置页/维护完成后刷新,不每帧查)
    index_stats: Option<IndexStats>,
    /// 是否有索引维护任务在后台进行
    maintenance_active: bool,
    /// 维护任务消息通道(世代号随通道存)
    maintain_rx: Option<(u64, Receiver<MaintainMsg>)>,
    /// 维护世代号
    maintain_gen: u64,
    /// 重建进度(维护线程写、UI 读;仅 Rebuild 任务期间有意义)
    maintain_progress: Option<Arc<MaintainProgress>>,
    /// 最近一次维护任务的结果文案(卡片内显示)
    maintain_result: Option<Result<String, String>>,
    /// 上次完整性检查不一致(显示「立即重建」按钮)
    maintain_inconsistent: bool,
    /// 「清空资料库」二次确认开关
    confirm_clear: bool,
    /// 设置页的操作提示(复制路径 / 打开目录结果等)
    settings_notice: Option<String>,
    /// 单文件体积上限(MB;settings.max_file_mb,设置页 DragValue)
    max_file_mb: u64,
    /// 最近一次用户词典加载结果(启动时与设置页「重新加载」时更新)
    dict_report: DictReport,
    /// 词典卡片的两个「快速添加」输入框(用户词 / 同义词组)
    dict_user_word_input: String,
    dict_synonym_input: String,
    /// 词典卡片的操作提示(与数据位置卡片的提示分开显示)
    dict_notice: Option<String>,
    /// 已点击待处理的对话框请求(帧末统一处理)
    pending_dialog: Option<DialogRequest>,
    /// 待二次确认的危险操作(选中后弹确认框;None = 没有)
    pending_confirm: Option<PendingConfirm>,
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
    fn new_state(ctx: &egui::Context) -> Self {
        Self {
            page: Page::Library,
            db: None,
            db_error: None,
            dirs: store::data_dirs(),
            egui_ctx: ctx.clone(),
            startup_log_path: None,
            library_notice: None,
            documents: Vec::new(),
            failed_documents: Vec::new(),
            documents_version: 0,
            filtered_docs: Vec::new(),
            filtered_key: None,
            docs_rx: None,
            docs_gen: 0,
            docs_loading: false,
            docs_refresh_pending: false,
            docs_dirty: false,
            docs_last_refresh: None,
            doc_filter: String::new(),
            library_tab: LibraryTab::default(),
            folder_groups: Vec::new(),
            library_tree_state: egui_ltreeview::TreeViewState::default(),
            filter_auto_opened: Vec::new(),
            show_failures: false,
            failure_filter: None,
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
            search_mtime_days: None,
            search_result: None,
            search_error: None,
            search_rx: None,
            search_gen: 0,
            preview_doc_id: None,
            search_locations: std::collections::HashMap::new(),
            location_popup_group: None,
            preview_hit_index: 0,
            preview_text: None,
            preview_rx: None,
            preview_gen: 0,
            preview_loading: false,
            pending_scroll: None,
            preview_anchor: 0,
            preview_spans_cache: None,
            index_stats: None,
            maintenance_active: false,
            maintain_rx: None,
            maintain_gen: 0,
            maintain_progress: None,
            maintain_result: None,
            maintain_inconsistent: false,
            confirm_clear: false,
            settings_notice: None,
            max_file_mb: rsou_lib::repo::DEFAULT_MAX_FILE_MB,
            dict_report: DictReport::default(),
            dict_user_word_input: String::new(),
            dict_synonym_input: String::new(),
            dict_notice: None,
            pending_dialog: None,
            pending_confirm: None,
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
            .exact_size(Self::SIDEBAR_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(Self::surface())
                    .inner_margin(SIDEBAR_MARGIN),
            )
            .show(ui, |ui| {
                self.ui_sidebar(ui);
                Self::paint_sidebar_border(ui);
            });

        if self.page == Page::Library && self.show_failures {
            egui::Panel::right("library_failures")
                .default_size(360.0)
                .min_size(240.0)
                .max_size((ui.available_width() * 0.5).max(240.0))
                .resizable(true)
                .frame(egui::Frame::new().fill(Self::surface()).inner_margin(16))
                .show(ui, |ui| self.ui_failures_panel(ui));
        }

        egui::CentralPanel::default()
            // 页面工作区:浅灰底,左右 24 / 上 22 / 下 24 的统一页边距。
            .frame(
                egui::Frame::new()
                    .fill(Self::canvas())
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 24,
                        top: 22,
                        bottom: 24,
                    }),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("workspace_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        // 进入资料库页时后台刷新文档列表缓存(不每帧查库)
                        if self.page == Page::Library && self.prev_page != Page::Library {
                            self.request_documents_refresh();
                        }
                        // 进入设置页时刷新索引统计(同样不每帧查库)
                        if self.page == Page::Settings && self.prev_page != Page::Settings {
                            self.refresh_index_stats();
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
