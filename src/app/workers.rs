//! 应用行为层:构造、索引库打开、后台任务轮询与对话框编排(非 UI 代码)。

use super::*;

impl RsouApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        Self::configure_ui_style(&cc.egui_ctx);
        let mut app = Self::new_state();
        app.open_store();
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

    /// 每帧非阻塞地收取后台任务消息。
    ///
    /// 阶段 2 的导入 worker 与阶段 3 的检索 worker 都从这里 poll:
    /// 消息里带世代号,发起方换代后晚到的旧结果直接移交后台析构
    /// (`drop_in_background`),不在 UI 线程做大释放。
    pub(crate) fn poll_workers(&mut self) {
        // 本阶段还没有后台任务;保留统一的 poll 入口。
        self.refresh_doc_count();
    }

    /// 顶栏徽标用的文档总数(每帧一次 count 查询,索引量级下代价可忽略)。
    fn refresh_doc_count(&mut self) {
        self.doc_count = self.db.as_ref().and_then(|db| {
            db.query_row("SELECT count(*) FROM documents", [], |row| row.get(0))
                .ok()
        });
    }

    /// Linux:驱动内置文件对话框(不依赖 XDG Portal / zenity)
    #[cfg(target_os = "linux")]
    pub(crate) fn drive_dialog(&mut self, ctx: &egui::Context) {
        // 1. 新请求:建对话框(初始目录沿用上次的位置)
        if let Some(request) = self.pending_dialog.take() {
            let dir = self.last_dir.clone();
            let dialog = match request {
                DialogRequest::ImportFiles => FileDialog::open(
                    "选择文档",
                    "添加到资料库",
                    dir,
                    file_dialog::document_filters(),
                ),
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
                    DialogRequest::ImportFiles => {
                        self.library_notice = Some(format!(
                            "已选择文件: {}(导入将在阶段 2 接入)",
                            path.display()
                        ));
                    }
                }
            }
        }
    }

    /// 其他平台:系统原生对话框(rfd;阻塞调用,所以放在帧末)
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn drive_dialog(&mut self, _ctx: &egui::Context) {
        let Some(request) = self.pending_dialog.take() else {
            return;
        };
        match request {
            DialogRequest::ImportFiles => {
                let picked = rfd::FileDialog::new()
                    .add_filter(
                        "文档(全部支持格式)",
                        &[
                            "doc", "docx", "docm", "ppt", "pps", "pot", "pptx", "pptm", "ppsx",
                            "ppsm", "xls", "xlsx", "xlsm", "xlsb", "odt", "ods", "odp", "rtf",
                            "epub", "csv", "pdf", "txt", "md",
                        ],
                    )
                    .add_filter("所有文件", &["*"])
                    .pick_file();
                if let Some(path) = picked {
                    self.library_notice = Some(format!(
                        "已选择文件: {}(导入将在阶段 2 接入)",
                        path.display()
                    ));
                }
            }
        }
    }
}
