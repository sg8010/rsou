//! 应用行为层:构造、索引库打开、后台任务轮询与对话框编排(非 UI 代码)。
//!
//! 线程模型:导入流水线跑在 `rsou-import` 线程,产出 `import::ImportEvent`;
//! GUI 每帧 `poll_workers` 消费事件更新状态——主线程是状态的唯一写入方,
//! worker 不直接触碰 GUI。写库连接由导入线程自己打开(GUI 的连接不跨线程)。

use std::sync::mpsc;
use std::time::Duration;

use rsou_lib::dict;
use rsou_lib::import::{self, FileOutcome, ImportOptions};
use rsou_lib::maintain;
use rsou_lib::repo;
use rsou_lib::search::{self, Filters, SearchRequest};

use super::*;

/// 最近失败列表的上限(超出丢最旧的)
const MAX_RECENT_FAILURES: usize = 20;

impl RsouApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        Self::configure_ui_style(&cc.egui_ctx);
        let mut app = Self::new_state(&cc.egui_ctx);
        app.open_store();
        // 启动时后台读一遍,资料库页首次进入就有数据
        app.request_documents_refresh();
        app.load_settings();
        app
    }

    /// 读 settings 里的用户配置,并加载数据目录下的用户词典。
    fn load_settings(&mut self) {
        self.reload_dict();
        let Some(conn) = &self.db else { return };
        self.max_file_mb = repo::get_setting(conn, "max_file_mb")
            .ok()
            .flatten()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|mb| (1..=repo::MAX_FILE_MB_LIMIT).contains(mb))
            .unwrap_or(repo::DEFAULT_MAX_FILE_MB);
    }

    /// 加载 / 重新加载用户词典(启动与设置页「重新加载」都走这里)。
    ///
    /// 词典只在查询期生效,重载后不需要重建索引;但**当前展示的检索结果是旧
    /// 词典算出来的**,调用方要自己重跑检索。
    pub(crate) fn reload_dict(&mut self) {
        let report = dict::load(&self.dirs.data_dir);
        for file in [&report.user_words, &report.synonyms] {
            for problem in &file.problems {
                log::warn!("用户词典 {} {}", file.path.display(), problem);
            }
        }
        self.dict_report = report;
    }

    /// 设置页 DragValue 变更时持久化(GUI 连接的零散小写)。
    pub(crate) fn set_max_file_mb(&mut self, mb: u64) {
        self.max_file_mb = mb;
        if let Some(conn) = &self.db
            && let Err(error) = repo::set_setting(conn, "max_file_mb", &mb.to_string())
        {
            log::warn!("保存 max_file_mb 失败: {error:#}");
        }
    }

    /// 重新读取索引统计(进入设置页 / 维护完成 / 手动刷新时调用)。
    pub(crate) fn refresh_index_stats(&mut self) {
        let Some(conn) = &self.db else { return };
        match maintain::index_stats(conn, &self.dirs.db_path) {
            Ok(stats) => self.index_stats = Some(stats),
            Err(error) => log::warn!("读取索引统计失败: {error:#}"),
        }
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

    /// 启动一次导入(已在导入中或索引维护中则忽略;GUI 一次只跑一个导入)。
    pub(crate) fn start_import(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>, force: bool) {
        self.start_import_with_options(
            ctx,
            inputs,
            ImportOptions {
                force,
                preserve_source_root: force,
                ..Default::default()
            },
        );
    }

    pub(crate) fn start_rescan(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.start_import_with_options(
            ctx,
            vec![path],
            ImportOptions {
                skip_unchanged_failed: true,
                preserve_source_root: true,
                ..Default::default()
            },
        );
    }

    fn start_import_with_options(
        &mut self,
        ctx: &egui::Context,
        inputs: Vec<PathBuf>,
        options: ImportOptions,
    ) {
        if self.import_active || self.maintenance_active || inputs.is_empty() || self.db.is_none() {
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

        // 单文件体积上限从 settings 读(GUI 连接只做这一次小读)。
        let max_file_bytes = self
            .db
            .as_ref()
            .map(repo::max_file_bytes)
            .unwrap_or(repo::DEFAULT_MAX_FILE_MB * 1024 * 1024);
        let db_path = self.dirs.db_path.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("rsou-import".to_owned())
            .spawn(move || {
                let result = import::run_import(
                    &db_path,
                    inputs,
                    ImportOptions {
                        max_file_bytes,
                        ..options
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

    /// 后台重读文档列表缓存(进入资料库页 / 导入节奏刷新 / 删除与维护后调用)。
    /// 已有读取在途时只记 pending,在途结果落地后再发一次,保证读到最终状态。
    pub(crate) fn request_documents_refresh(&mut self) {
        if self.db.is_none() {
            return;
        }
        if self.docs_rx.is_some() {
            self.docs_refresh_pending = true;
            return;
        }
        self.docs_gen += 1;
        let generation = self.docs_gen;
        self.docs_loading = true;
        self.docs_dirty = false;
        let (tx, rx) = mpsc::channel::<DocsMsg>();
        self.docs_rx = Some((generation, rx));

        let db_path = self.dirs.db_path.clone();
        let ctx = self.egui_ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("rsou-docs".to_owned())
            .spawn(move || {
                // 读取线程自己开只读连接;库不可用时把中文原因带回。
                let result = store::open(&db_path, OpenMode::ReadOnly)
                    .and_then(|conn| {
                        Ok(DocsSnapshot {
                            documents: repo::list_documents(&conn)?,
                            failed: repo::list_failed_documents(&conn)?,
                            folder_groups: repo::list_documents_by_source_root(&conn)?
                                .into_iter()
                                .map(|(root, documents)| FolderGroup { root, documents })
                                .collect(),
                        })
                    })
                    .map_err(|error| format!("{error:#}"));
                if tx.send(DocsMsg { generation, result }).is_ok() {
                    ctx.request_repaint();
                }
            });
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(error) => {
                self.docs_loading = false;
                self.docs_rx = None;
                self.library_notice = Some(format!("无法启动文档列表读取线程: {error}"));
            }
        }
    }

    /// 使在途的文档列表读取结果作废(删除/清空之后调用):
    /// 落地时按世代丢弃,再由补读拿最终状态。
    pub(crate) fn invalidate_documents_snapshot(&mut self) {
        self.docs_gen += 1;
    }

    /// 移除一个来源文件夹及其全部归属文档(确认框之后调用)。
    pub(crate) fn delete_folder(&mut self, source_root: &str) {
        let Some(conn) = self.db.as_mut() else {
            return;
        };
        match repo::delete_source_root(conn, source_root) {
            Ok(removed) => {
                self.library_notice = Some(format!("已移除文件夹索引(共 {removed} 篇文档)"));
                // 先从缓存里摘掉,避免后台读取落地前树里还显示已删项。
                self.folder_groups.retain(|g| g.root != source_root);
                self.documents
                    .retain(|d| d.source_root.as_deref() != Some(source_root));
                self.failed_documents
                    .retain(|d| d.source_root.as_deref() != Some(source_root));
                self.documents_version += 1;
                self.invalidate_documents_snapshot();
                self.request_documents_refresh();
            }
            Err(error) => self.library_notice = Some(format!("移除文件夹失败: {error:#}")),
        }
    }

    /// 删除一篇文档并刷新缓存(用 GUI 连接做一次短写;导入中禁用由页面把关)。
    pub(crate) fn delete_document(&mut self, id: i64) {
        let Some(conn) = self.db.as_mut() else {
            return;
        };
        match repo::delete_document(conn, id) {
            Ok(()) => {
                self.library_notice = Some("已移除文档".to_owned());
                // 先从缓存里摘掉,避免后台读取落地前表格还显示已删行
                self.documents.retain(|d| d.id != id);
                self.failed_documents.retain(|d| d.id != id);
                self.documents_version += 1;
                self.invalidate_documents_snapshot();
                self.request_documents_refresh();
            }
            Err(error) => self.library_notice = Some(format!("移除失败: {error:#}")),
        }
    }

    /// 发起一次检索(空查询不触发;检索线程每次新建、只读连接)。
    /// 不检查 GUI 连接:worker 自己开只读连接,库不可用时错误走状态行。
    pub(crate) fn start_search(&mut self, ctx: &egui::Context) {
        let query = self.search_query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.search_active = true;
        self.search_error = None;
        self.search_gen += 1;
        let generation = self.search_gen;
        let (tx, rx) = mpsc::channel::<SearchMsg>();
        self.search_rx = Some((generation, rx));

        let db_path = self.dirs.db_path.clone();
        let ctx = ctx.clone();
        // 无 jieba feature 时宽松模式退化为精确(库层已保证,这里固定精确保持一致)。
        // 时间范围:最近 N 天 → mtime 下界(Unix 毫秒)。
        let mtime_from_ms = self
            .search_mtime_days
            .map(|days| repo::now_ms().saturating_sub(days.saturating_mul(86_400_000) as i64));
        let request = SearchRequest {
            query,
            scope: self.search_scope,
            loose: !self.search_exact,
            filters: Filters {
                file_types: self.search_types.iter().cloned().collect(),
                mtime_from_ms,
                mtime_to_ms: None,
                path_prefix: {
                    let prefix = self.search_path_prefix.trim();
                    if prefix.is_empty() {
                        None
                    } else {
                        Some(prefix.to_owned())
                    }
                },
            },
            max_documents: 100,
            max_fragments_per_document: search::DEFAULT_MAX_FRAGMENTS,
        };
        let spawned = std::thread::Builder::new()
            .name("rsou-search".to_owned())
            .spawn(move || {
                // 检索线程自己开只读连接;库不存在时把中文原因带回状态行。
                let result = store::open(&db_path, OpenMode::ReadOnly)
                    .and_then(|conn| search::search(&conn, &request))
                    .map_err(|error| format!("{error:#}"));
                if tx.send(SearchMsg { generation, result }).is_ok() {
                    ctx.request_repaint();
                }
            });
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(error) => {
                self.search_active = false;
                self.search_rx = None;
                self.search_error = Some(format!("无法启动检索线程: {error}"));
            }
        }
    }

    /// 加载预览文本(同一 worker 模式:线程读库,UI 线程只收消息)。
    pub(crate) fn start_preview(&mut self, document_id: i64) {
        self.preview_loading = true;
        self.preview_gen += 1;
        let generation = self.preview_gen;
        let (tx, rx) = mpsc::channel::<PreviewMsg>();
        self.preview_rx = Some((generation, rx));

        let db_path = self.dirs.db_path.clone();
        let spawned = std::thread::Builder::new()
            .name("rsou-preview".to_owned())
            .spawn(move || {
                let text = store::open(&db_path, OpenMode::ReadOnly)
                    .ok()
                    .and_then(|conn| {
                        search::plain_text_for_preview(&conn, document_id)
                            .ok()
                            .flatten()
                    });
                let _ = tx.send(PreviewMsg {
                    generation,
                    document_id,
                    text,
                });
            });
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(error) => {
                self.preview_loading = false;
                self.preview_rx = None;
                self.search_error = Some(format!("无法启动预览线程: {error}"));
            }
        }
    }

    /// 切到该文档预览的指定命中批次,并记录滚动目标(plain_text 字节偏移)。
    pub(crate) fn focus_preview(&mut self, document_id: i64, hit_index: usize, byte_offset: usize) {
        if let Some(hit) = self.search_result.as_ref().and_then(|result| {
            result
                .documents
                .iter()
                .find(|hit| hit.document.id == document_id)
        }) {
            self.search_locations.insert(hit.group_id, document_id);
            if self.location_popup_group != Some(hit.group_id) {
                self.location_popup_group = None;
            }
        }
        self.preview_hit_index = hit_index;
        self.pending_scroll = Some(byte_offset);
        self.preview_anchor = byte_offset;
        if self.preview_doc_id != Some(document_id) || self.preview_text.is_none() {
            self.preview_doc_id = Some(document_id);
            let old = self.preview_text.take();
            if let Some(old) = old {
                drop_in_background(old);
            }
            self.start_preview(document_id);
        }
    }

    /// 清空预览窗格(文档/文本/导航/缓存);世代+1,在途的预览响应按过期丢弃。
    fn clear_preview(&mut self) {
        self.location_popup_group = None;
        self.preview_doc_id = None;
        self.preview_hit_index = 0;
        self.preview_anchor = 0;
        self.pending_scroll = None;
        self.preview_spans_cache = None;
        self.preview_gen += 1;
        self.preview_rx = None;
        self.preview_loading = false;
        if let Some(old) = self.preview_text.take() {
            drop_in_background(old);
        }
    }

    /// 启动一次索引维护(检查/重建/优化/清空共用一条 rsou-maintain 通道;
    /// 已在维护或导入中则忽略,GUI 一次只跑一个)。
    pub(crate) fn start_maintain(&mut self, ctx: &egui::Context, kind: MaintainKind) {
        if self.maintenance_active || self.import_active {
            return;
        }
        self.maintenance_active = true;
        self.maintain_result = None;
        self.maintain_gen += 1;
        let generation = self.maintain_gen;
        let (tx, rx) = mpsc::channel::<MaintainMsg>();
        self.maintain_rx = Some((generation, rx));
        let progress = Arc::new(MaintainProgress::default());
        self.maintain_progress = Some(progress.clone());

        let db_path = self.dirs.db_path.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("rsou-maintain".to_owned())
            .spawn(move || {
                let result =
                    store::open(&db_path, OpenMode::ReadWrite).and_then(|mut conn| match kind {
                        MaintainKind::Check => maintain::check_integrity(&conn, 2000)
                            .map(|report| (report.summary(), !report.is_consistent())),
                        MaintainKind::Rebuild => {
                            maintain::rebuild_fts(&mut conn, &mut |done, total| {
                                progress.done.store(done, Ordering::Relaxed);
                                progress.total.store(total, Ordering::Relaxed);
                                ctx.request_repaint();
                            })
                            .map(|rows| (format!("全文索引已重建: {rows} 条"), false))
                        }
                        MaintainKind::Optimize => {
                            maintain::optimize(&conn).map(|()| ("索引已优化".to_owned(), false))
                        }
                        MaintainKind::Clear => maintain::clear_all(&mut conn)
                            .map(|()| ("资料库已清空".to_owned(), false)),
                    });
                let (result, inconsistent) = match result {
                    Ok((message, inconsistent)) => (Ok(message), inconsistent),
                    Err(error) => (Err(format!("{error:#}")), false),
                };
                if tx
                    .send(MaintainMsg {
                        generation,
                        kind,
                        result,
                        inconsistent,
                    })
                    .is_ok()
                {
                    ctx.request_repaint();
                }
            });
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(error) => {
                self.maintenance_active = false;
                self.maintain_rx = None;
                self.maintain_progress = None;
                self.maintain_result = Some(Err(format!("无法启动维护线程: {error}")));
            }
        }
    }

    /// 每帧非阻塞地收取后台任务消息;回收已结束的 worker 线程
    /// (已完成的 join 掉收 panic 信息,未完成的继续留在列表里)。
    pub(crate) fn poll_workers(&mut self) {
        self.poll_import();
        self.poll_docs();
        self.poll_search();
        self.poll_preview();
        self.poll_maintain();
        let mut still_running = Vec::with_capacity(self.workers.len());
        for worker in std::mem::take(&mut self.workers) {
            if worker.is_finished() {
                if let Err(error) = worker.join() {
                    log::warn!("后台线程异常结束: {error:?}");
                }
            } else {
                still_running.push(worker);
            }
        }
        self.workers = still_running;
    }

    /// 消费维护结果;完成后刷新统计与文档列表缓存。
    fn poll_maintain(&mut self) {
        if let Some((generation, rx)) = &self.maintain_rx
            && *generation == self.maintain_gen
        {
            let mut msg = None;
            while let Ok(event) = rx.try_recv() {
                msg = Some(event);
            }
            if let Some(MaintainMsg {
                generation,
                kind,
                result,
                inconsistent,
            }) = msg
                && generation == self.maintain_gen
            {
                self.maintenance_active = false;
                self.maintain_rx = None;
                self.maintain_progress = None;
                self.maintain_result = Some(result);
                self.maintain_inconsistent = inconsistent;
                self.refresh_index_stats();
                // 清空后文档列表必须跟着空;重建/优化后也顺手刷新,成本一样。
                if !matches!(kind, MaintainKind::Check) {
                    self.invalidate_documents_snapshot();
                    self.request_documents_refresh();
                }
            }
        }
    }

    /// 消费文档列表读取结果。世代不匹配(删除/清空已作废)的快照直接丢弃;
    /// 作废或在途期间有过刷新请求都在落地后补读一次,保证读到最终状态。
    fn poll_docs(&mut self) {
        let Some((_, rx)) = &self.docs_rx else {
            return;
        };
        let mut msg = None;
        while let Ok(event) = rx.try_recv() {
            msg = Some(event);
        }
        let Some(DocsMsg { generation, result }) = msg else {
            return;
        };
        self.docs_loading = false;
        self.docs_rx = None;
        self.docs_last_refresh = Some(std::time::Instant::now());
        let stale = generation != self.docs_gen;
        if !stale {
            match result {
                Ok(snapshot) => {
                    // 旧列表可能很大,换下来后台丢,避免在 GUI 线程上跑析构
                    let old = std::mem::replace(&mut self.documents, snapshot.documents);
                    drop_in_background(old);
                    let old_groups =
                        std::mem::replace(&mut self.folder_groups, snapshot.folder_groups);
                    drop_in_background(old_groups);
                    self.failed_documents = snapshot.failed;
                    self.documents_version += 1;
                }
                Err(error) => {
                    self.library_notice = Some(format!("读取文档列表失败: {error}"));
                }
            }
        }
        // 过期快照(删除/清空后已作废)直接丢弃;作废或在途期间有过刷新请求都补读一次拿最终状态。
        if stale || self.docs_refresh_pending {
            self.docs_refresh_pending = false;
            self.request_documents_refresh();
        }
    }

    /// 消费检索结果;世代号不匹配(新查询已发出)时丢弃过期消息。
    fn poll_search(&mut self) {
        if let Some((generation, rx)) = &self.search_rx
            && *generation == self.search_gen
        {
            let mut msg = None;
            while let Ok(event) = rx.try_recv() {
                msg = Some(event);
            }
            if let Some(SearchMsg { generation, result }) = msg
                && generation == self.search_gen
            {
                self.search_active = false;
                self.search_rx = None;
                match result {
                    Ok(response) => {
                        self.search_error = None;
                        self.search_locations.clear();
                        self.location_popup_group = None;
                        if let Some(hit) = response
                            .documents
                            .iter()
                            .find(|hit| Some(hit.document.id) == self.preview_doc_id)
                        {
                            self.search_locations.insert(hit.group_id, hit.document.id);
                        }
                        // 预览属于结果集:选中的文档不在新结果里就整体清掉,
                        // 否则上一次搜索的预览文本会挂在空结果旁边。
                        let still_hit = self.preview_doc_id.is_some_and(|id| {
                            response.documents.iter().any(|d| d.document.id == id)
                        });
                        if !still_hit {
                            self.clear_preview();
                        }
                        if let Some(old) = self.search_result.replace(response) {
                            drop_in_background(old);
                        }
                    }
                    Err(error) => self.search_error = Some(error),
                }
            }
        }
    }

    /// 消费预览文本;同样按世代号丢过期消息。
    fn poll_preview(&mut self) {
        if let Some((generation, rx)) = &self.preview_rx
            && *generation == self.preview_gen
        {
            let mut msg = None;
            while let Ok(event) = rx.try_recv() {
                msg = Some(event);
            }
            if let Some(PreviewMsg {
                generation,
                document_id,
                text,
            }) = msg
                && generation == self.preview_gen
            {
                self.preview_loading = false;
                self.preview_rx = None;
                if self.preview_doc_id == Some(document_id)
                    && let Some(old) = text.and_then(|t| self.preview_text.replace(t))
                {
                    drop_in_background(old);
                }
            }
        }
    }

    /// 消费导入事件:更新进度、按节奏后台刷新文档列表。
    ///
    /// 消息里带世代号,换代后晚到的旧事件直接丢弃。
    fn poll_import(&mut self) {
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
                        self.docs_dirty = true;
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
        // 文档列表后台刷新:导入结束立刻读一次(pending 机制保证在途读取
        // 落地后还会再读,最终状态一定刷新);导入中至少间隔一秒才发一次。
        if finished
            || (self.docs_dirty
                && self.docs_rx.is_none()
                && self
                    .docs_last_refresh
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(1)))
        {
            self.request_documents_refresh();
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

#[cfg(test)]
mod tests {
    use super::*;
    use rsou_lib::query::CompiledQuery;
    use rsou_lib::search::DocumentHit;

    fn doc(id: i64) -> DocumentHit {
        DocumentHit {
            group_id: id,
            document: DocumentRow {
                id,
                path: format!("/doc{id}.txt"),
                file_name: format!("doc{id}.txt"),
                title: String::new(),
                ext: "txt".to_owned(),
                file_type: "text".to_owned(),
                file_size: 1,
                file_mtime_ms: 0,
                parse_status: "parsed".to_owned(),
                parse_error_code: None,
                parse_error_message: None,
                text_length: 1,
                chunk_count: 1,
                indexed_at: None,
                source_root: None,
                updated_at: 0,
            },
            title_highlights: Vec::new(),
            hits: Vec::new(),
            total_hits: 0,
            best_rank: 0.0,
        }
    }

    fn response(ids: &[i64]) -> SearchResponse {
        SearchResponse {
            documents: ids.iter().map(|&id| doc(id)).collect(),
            total_hits: 0,
            total_documents: ids.len(),
            total_groups: ids.len(),
            elapsed_ms: 0.0,
            diagnostics: Default::default(),
            compiled: CompiledQuery {
                match_expr: String::new(),
                literals: Vec::new(),
            },
        }
    }

    /// 以检索线程相同的方式把一条结果送进通道并消费。
    fn push_result(app: &mut RsouApp, result: SearchResponse) {
        app.search_gen += 1;
        let (tx, rx) = mpsc::channel::<SearchMsg>();
        app.search_rx = Some((app.search_gen, rx));
        app.search_active = true;
        tx.send(SearchMsg {
            generation: app.search_gen,
            result: Ok(result),
        })
        .unwrap();
        app.poll_search();
    }

    #[test]
    fn result_without_previewed_doc_clears_preview() {
        // 回归:第二次搜索无命中时,预览窗格还显示着上次选中的文档。
        // poll_search 只替换 search_result,从不动 preview_text。
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        push_result(&mut app, response(&[1, 2]));
        app.preview_doc_id = Some(1);
        app.preview_text = Some("第一篇的原文".to_owned());
        app.preview_hit_index = 1;
        app.preview_anchor = 5;
        app.pending_scroll = Some(5);
        app.preview_spans_cache = Some(PreviewSpanCache {
            preview_gen: 0,
            base: 0,
            window_len: 1,
            literals: Vec::new(),
            spans: Vec::new(),
        });
        push_result(&mut app, response(&[]));
        assert!(
            app.preview_doc_id.is_none(),
            "预览不应停留在已不在结果集的文档"
        );
        assert!(app.preview_text.is_none());
        assert_eq!(app.preview_hit_index, 0);
        assert_eq!(app.preview_anchor, 0);
        assert!(app.pending_scroll.is_none());
        assert!(app.preview_spans_cache.is_none());
    }

    #[test]
    fn result_keeping_previewed_doc_keeps_preview() {
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        push_result(&mut app, response(&[1, 2]));
        app.preview_doc_id = Some(2);
        app.preview_text = Some("原文".to_owned());
        let old_gen = app.preview_gen;
        // 新结果集仍含该文档:预览保留(高亮随新结果重算)。
        push_result(&mut app, response(&[2, 3]));
        assert_eq!(app.preview_doc_id, Some(2));
        assert_eq!(app.preview_text.as_deref(), Some("原文"));
        assert_eq!(app.preview_gen, old_gen);
    }

    #[test]
    fn selected_duplicate_is_remembered_and_disappearing_location_clears_preview() {
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        let mut grouped = response(&[1, 2, 3]);
        grouped.documents[1].group_id = 1;
        grouped.total_groups = 2;
        push_result(&mut app, grouped);
        // 文本已加载时聚焦不需要启动读取线程。
        app.preview_doc_id = Some(2);
        app.preview_text = Some("副本的正文".to_owned());
        app.focus_preview(2, 0, 6);
        assert_eq!(app.search_locations.get(&1), Some(&2));
        assert_eq!(app.pending_scroll, Some(6));

        app.preview_doc_id = Some(3);
        app.focus_preview(3, 0, 0);
        assert_eq!(
            app.search_locations.get(&1),
            Some(&2),
            "查看别组不丢失位置选择"
        );

        app.preview_doc_id = Some(2);
        app.location_popup_group = Some(1);
        push_result(&mut app, response(&[1, 3]));
        assert!(
            app.preview_doc_id.is_none(),
            "代表位置仍存在也不能保留已消失副本的预览"
        );
        assert!(app.preview_text.is_none());
        assert!(app.search_locations.is_empty());
        assert!(app.location_popup_group.is_none());
    }
}
