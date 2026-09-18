//! 应用行为层:构造、索引库打开、后台任务轮询与对话框编排(非 UI 代码)。
//!
//! 线程模型:导入流水线跑在 `rsou-import` 线程,产出 `import::ImportEvent`;
//! GUI 每帧 `poll_workers` 消费事件更新状态——主线程是状态的唯一写入方,
//! worker 不直接触碰 GUI。写库连接由导入线程自己打开(GUI 的连接不跨线程)。

use std::sync::mpsc;

use rsou_lib::import::{self, FileOutcome, ImportOptions};
use rsou_lib::repo;

use super::*;

/// 最近失败列表的上限(超出丢最旧的)
const MAX_RECENT_FAILURES: usize = 20;

impl RsouApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        Self::configure_ui_style(&cc.egui_ctx);
        let mut app = Self::new_state();
        app.open_store();
        // 启动时先读一遍,资料库页首次进入就有数据
        app.refresh_documents();
        app
    }

    /// 带启动完成标记的构造入口,供二进制入口的启动 watchdog 使用。
    pub fn new_with_startup_marker(
        cc: &eframe::CreationContext<'_>,
        startup_ready: Arc<AtomicBool>,
        startup_log_path: Option<PathBuf>,
    ) -> Self {
        let mut app = Self::new(cc);
        app.startup_ready = Some(startup_ready);
        app.startup_log_path = startup_log_path;
        app
    }

    /// 打开索引库(ReadWrite,保证空库也能建表)。
    /// 失败不 panic:把中文原因写进状态,页面直接显示。
    fn open_store(&mut self) {
        match store::open(&self.dirs.db_path, OpenMode::ReadWrite) {
            Ok(connection) => {
                self.db = Some(connection);
                self.db_error = None;
            }
            Err(error) => {
                self.db = None;
                self.db_error = Some(format!("{error:#}"));
            }
        }
    }

    /// 启动一次导入(已在导入中则忽略;GUI 一次只跑一个导入)。
    pub(crate) fn start_import(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>, force: bool) {
        if self.import_active || inputs.is_empty() || self.db.is_none() {
            return;
        }
        self.import_active = true;
        self.import_progress = ImportProgress::default();
        self.import_gen += 1;
        let generation = self.import_gen;
        let cancel = Arc::new(AtomicBool::new(false));
        self.import_cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::channel::<ImportEvent>();
        self.import_rx = Some((generation, rx));

        let db_path = self.dirs.db_path.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("rsou-import".to_owned())
            .spawn(move || {
                let result = import::run_import(
                    &db_path,
                    inputs,
                    ImportOptions {
                        force,
                        ..ImportOptions::default()
                    },
                    cancel,
                    &mut |event| {
                        if tx.send(event).is_ok() {
                            ctx.request_repaint();
                        }
                    },
                );
                if let Err(error) = result {
                    log::error!("导入线程失败: {error:#}");
                }
            });
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(error) => {
                self.import_active = false;
                self.import_rx = None;
                self.import_cancel = None;
                self.library_notice = Some(format!("无法启动导入线程: {error}"));
            }
        }
    }

    /// 取消正在进行的导入(置标志;worker 在文件粒度上停)。
    pub(crate) fn cancel_import(&mut self) {
        if let Some(cancel) = &self.import_cancel {
            cancel.store(true, Ordering::Relaxed);
            self.library_notice = Some("正在取消导入…".to_owned());
        }
    }

    /// 重新读取文档列表缓存(进入资料库页 / 导入结束 / 周期性刷新时调用)。
    pub(crate) fn refresh_documents(&mut self) {
        let Some(conn) = &self.db else { return };
        match repo::list_documents(conn) {
            Ok(documents) => {
                // 旧列表可能很大,换下来后台丢,避免在 GUI 线程上跑析构
                let old = std::mem::replace(&mut self.documents, documents);
                drop_in_background(old);
            }
            Err(error) => {
                self.library_notice = Some(format!("读取文档列表失败: {error:#}"));
            }
        }
        self.failed_documents = repo::list_failed_documents(conn).unwrap_or_default();
    }

    /// 删除一篇文档并刷新缓存(用 GUI 连接做一次短写;导入中禁用由页面把关)。
    pub(crate) fn delete_document(&mut self, id: i64) {
        let Some(conn) = self.db.as_mut() else {
            return;
        };
        match repo::delete_document(conn, id) {
            Ok(()) => {
                self.library_notice = Some("已移除文档".to_owned());
                self.refresh_documents();
            }
            Err(error) => self.library_notice = Some(format!("移除失败: {error:#}")),
        }
    }

    /// 每帧非阻塞地收取后台任务消息;回收已结束的 worker 线程。
    pub(crate) fn poll_workers(&mut self) {
        self.poll_import();
        self.workers.retain(|worker| !worker.is_finished());
    }

    /// 消费导入事件:更新进度、按节奏刷新文档列表。
    ///
    /// 消息里带世代号,换代后晚到的旧事件直接丢弃。
    fn poll_import(&mut self) {
        let mut file_done = 0usize;
        let mut finished = false;
        if let Some((generation, rx)) = &self.import_rx
            && *generation == self.import_gen
        {
            while let Ok(event) = rx.try_recv() {
                match event {
                    ImportEvent::Scanned { total } => {
                        self.import_progress.counts.total = total;
                    }
                    ImportEvent::FileDone {
                        path,
                        outcome,
                        counts,
                    } => {
                        self.import_progress.counts = counts;
                        self.import_progress.current_path = Some(path.clone());
                        if let FileOutcome::Failed { message, .. } = outcome {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            self.import_progress
                                .recent_failures
                                .push_back((name, message));
                            while self.import_progress.recent_failures.len() > MAX_RECENT_FAILURES {
                                self.import_progress.recent_failures.pop_front();
                            }
                        }
                        file_done += 1;
                    }
                    ImportEvent::Finished {
                        counts, cancelled, ..
                    } => {
                        self.import_progress.counts = counts;
                        self.import_progress.cancelled = cancelled;
                        self.import_active = false;
                        finished = true;
                        self.library_notice = Some(if cancelled {
                            format!(
                                "导入已取消:处理 {}/{} 个文件",
                                counts.processed, counts.total
                            )
                        } else {
                            format!(
                                "导入完成:成功 {}、失败 {}、跳过 {}(共 {})",
                                counts.ok, counts.failed, counts.skipped, counts.total
                            )
                        });
                    }
                }
            }
        }
        if finished {
            self.import_rx = None;
            self.import_cancel = None;
        }
        // 文档列表刷新:导入结束立刻刷;导入中每 20 个文件刷一次,不每事件刷
        if finished || file_done >= 20 {
            self.refresh_documents();
        }
    }

    /// Linux:驱动内置文件对话框(不依赖 XDG Portal / zenity)
    #[cfg(target_os = "linux")]
    pub(crate) fn drive_dialog(&mut self, ctx: &egui::Context) {
        // 1. 新请求:建对话框(初始目录沿用上次的位置)
        if let Some(request) = self.pending_dialog.take() {
            let dir = self.last_dir.clone();
            let dialog = match request {
                DialogRequest::ImportFiles => FileDialog::open(
                    "选择要导入的文档",
                    "添加到资料库",
                    dir,
                    file_dialog::document_filters(),
                ),
                DialogRequest::ImportFolder => {
                    FileDialog::pick_folder("选择要导入的文件夹", "添加到资料库", dir)
                }
            };
            self.dialog = Some(ActiveDialog { request, dialog });
        }
        // 2. 已显示的对话框:画一帧并处理结果
        let Some(active) = &mut self.dialog else {
            return;
        };
        let action = active.dialog.ui(ctx);
        // 记住用户停留的目录,下次从这里打开(即便这次取消了)
        self.last_dir = Some(active.dialog.dir().to_path_buf());
        let request = active.request;
        match action {
            DialogAction::None => {}
            DialogAction::Cancelled => self.dialog = None,
            DialogAction::Picked(path) => {
                self.dialog = None;
                match request {
                    DialogRequest::ImportFiles | DialogRequest::ImportFolder => {
                        self.start_import(ctx, vec![path], false);
                    }
                }
            }
        }
    }

    /// 其他平台:系统原生对话框(rfd;阻塞调用,所以放在帧末)
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn drive_dialog(&mut self, ctx: &egui::Context) {
        let Some(request) = self.pending_dialog.take() else {
            return;
        };
        match request {
            DialogRequest::ImportFiles => {
                let picked = rfd::FileDialog::new()
                    .add_filter(
                        "文档(全部支持格式)",
                        rsou_lib::parse::supported_extensions(),
                    )
                    .add_filter("所有文件", &["*"])
                    .pick_file();
                if let Some(path) = picked {
                    self.start_import(ctx, vec![path], false);
                }
            }
            DialogRequest::ImportFolder => {
                let picked = rfd::FileDialog::new().pick_folder();
                if let Some(path) = picked {
                    self.start_import(ctx, vec![path], false);
                }
            }
        }
    }
}
