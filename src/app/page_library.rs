//! 资料库页:文档导入、进度卡、两个页签(文件夹摘要 / 单独文件)与右侧失败清单。

use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use rsou_lib::filebrowser;

use super::theme::Icon;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureCategory {
    Unsupported,
    Encrypted,
    Malformed,
    ResourceLimit,
    NeedsOcr,
    FileMissing,
    Io,
    TooLarge,
    Empty,
    Other,
}

impl FailureCategory {
    const ALL: [Self; 10] = [
        Self::Unsupported,
        Self::Encrypted,
        Self::Malformed,
        Self::ResourceLimit,
        Self::NeedsOcr,
        Self::FileMissing,
        Self::Io,
        Self::TooLarge,
        Self::Empty,
        Self::Other,
    ];

    fn code(self) -> Option<rsou_lib::parse::ParseErrorCode> {
        use rsou_lib::parse::ParseErrorCode as Code;
        match self {
            Self::Unsupported => Some(Code::Unsupported),
            Self::Encrypted => Some(Code::Encrypted),
            Self::Malformed => Some(Code::Malformed),
            Self::ResourceLimit => Some(Code::ResourceLimit),
            Self::NeedsOcr => Some(Code::NeedsOcr),
            Self::FileMissing => Some(Code::FileMissing),
            Self::Io => Some(Code::Io),
            Self::TooLarge => Some(Code::TooLarge),
            Self::Empty => Some(Code::Empty),
            Self::Other => None,
        }
    }

    fn from_code(code: Option<&str>) -> Self {
        Self::ALL
            .into_iter()
            .find(|category| category.code().is_some_and(|c| Some(c.as_str()) == code))
            .unwrap_or(Self::Other)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Unsupported => "不支持的格式",
            Self::Encrypted => "文件加密",
            Self::Malformed => "文件损坏",
            Self::ResourceLimit => "资源限制",
            Self::NeedsOcr => "需要 OCR",
            Self::FileMissing => "原文件不存在",
            Self::Io => "读写错误",
            Self::TooLarge => "文件过大",
            Self::Empty => "内容为空",
            Self::Other => "其他",
        }
    }
}

/// 只缓存下标,不复制路径与错误文本;切换分类和重新打开清单都可直接复用。
#[derive(Default)]
pub(super) struct FailureListCache {
    version: Option<u64>,
    by_category: [Vec<usize>; FailureCategory::ALL.len()],
}

impl FailureListCache {
    fn refresh(&mut self, version: u64, documents: &[DocumentRow]) {
        if self.version == Some(version) {
            return;
        }
        for indices in &mut self.by_category {
            indices.clear();
        }
        for (index, document) in documents.iter().enumerate() {
            let category = FailureCategory::from_code(document.parse_error_code.as_deref());
            self.by_category[category as usize].push(index);
        }
        self.version = Some(version);
    }

    fn indices(&self, category: FailureCategory) -> &[usize] {
        &self.by_category[category as usize]
    }
}

/// 行内操作意图(表格闭包结束后再落地,避免借用冲突)。
enum RowAction {
    Open(PathBuf),
    Reveal(PathBuf),
    /// 强制重解析该文件
    Reimport(PathBuf),
    /// 只处理新增或修改的文件,未变化的失败文件也跳过。
    Rescan(PathBuf),
    /// 请求移除单个文档(需二次确认)
    AskRemoveDocument(i64, String),
    /// 请求移除整个来源文件夹(需二次确认)
    AskRemoveFolder(String, u64),
    /// 重解析整个来源文件夹(递归强制重导)
    ReimportFolder(String),
    /// 定位来源文件夹
    RevealFolder(String),
    /// 在资料库内部选中并滚动到来源文件夹。
    LocateFolder(String),
}

impl RsouApp {
    pub(crate) fn ui_page_library(&mut self, ui: &mut egui::Ui) {
        // ---------- 页头:标题 + 说明 + 右侧导入操作 ----------
        let mut want_files = false;
        let mut want_folder = false;
        let db_ready = self.db.is_some();
        // 维护(重建/清空)进行中时导入也禁用,两条写路径互斥。
        let importing = self.import_active || self.maintenance_active;
        Self::page_header_actions(ui, self.page.title(), self.page.description(), |ui| {
            // right_to_left:先加的在最右。
            if Self::primary_button(
                ui,
                Some(Icon::DocPlus),
                "添加文件",
                104.0,
                db_ready && !importing,
            )
            .clicked()
            {
                want_files = true;
            }
            if Self::secondary_button(
                ui,
                Some(Icon::FolderPlus),
                "添加文件夹",
                118.0,
                db_ready && !importing,
            )
            .clicked()
            {
                want_folder = true;
            }
        });
        if want_files {
            self.pending_dialog = Some(DialogRequest::ImportFiles);
        }
        if want_folder {
            self.pending_dialog = Some(DialogRequest::ImportFolder);
        }
        if let Some(error) = self.db_error.clone() {
            ui.add_space(8.0);
            Self::warn_banner(ui, &format!("无法打开索引库,导入与检索不可用: {error}"));
        }
        self.ui_fts_readiness(ui);
        if let Some(notice) = self.library_notice.clone() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(notice)
                    .size(13.0)
                    .color(Self::warning()),
            );
        }
        ui.add_space(Self::HEADER_TO_CONTENT);

        // ---------- 导入进度卡(仅导入中出现) ----------
        if self.import_active {
            let mut cancel = false;
            let cancelling = self.import_progress.cancelling;
            Self::card(ui, |ui| {
                ui.horizontal(|ui| {
                    Self::card_title(
                        ui,
                        if cancelling {
                            "正在取消"
                        } else if self.import_progress.is_ocr {
                            "正在 OCR"
                        } else {
                            "正在导入"
                        },
                    );
                    ui.label(
                        egui::RichText::new(if cancelling {
                            "正在停止后台任务，已入库数据将保留"
                        } else if self.import_progress.is_ocr {
                            "识别完成后自动加入全文索引"
                        } else {
                            "正在解析文档并建立全文索引"
                        })
                        .size(12.0)
                        .color(Self::text_muted()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Self::small_secondary_button(
                            ui,
                            None,
                            if cancelling { "取消中" } else { "取消" },
                            72.0,
                            !cancelling,
                        )
                        .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
                ui.add_space(10.0);
                let c = &self.import_progress.counts;
                let fraction = if c.total == 0 {
                    0.0
                } else {
                    c.processed as f32 / c.total as f32
                };
                Self::progress_bar(ui, fraction, ui.available_width());
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "已处理 {}/{} · 成功 {} · 失败 {} · 跳过 {}",
                        c.processed, c.total, c.ok, c.failed, c.skipped
                    ))
                    .size(13.0)
                    .color(Self::text_secondary()),
                );
                if let Some(path) = &self.import_progress.current_path {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("最近: {}", path.display()))
                                .size(12.0)
                                .color(Self::text_muted()),
                        )
                        .truncate(),
                    );
                }
                for (name, message) in self.import_progress.recent_failures.iter().rev().take(5) {
                    ui.label(
                        egui::RichText::new(format!("{name}: {message}"))
                            .size(12.0)
                            .color(Self::warning()),
                    );
                }
            });
            if cancel {
                self.cancel_import();
            }
            ui.add_space(Self::SECTION_GAP);
        }

        // ---------- 已加入索引的清单(页签栏 + 内容,同一张卡片)----------
        //
        // 页签与内容必须在一张卡片里:拆成两张卡片时,两张卡各自还有 18px
        // 内边距加上卡片间距,页签与内容之间就空出近四十像素。
        //
        // 文档空间里的视口底边:页面起点 + 视口高。工作区包在外层
        // ScrollArea 里,列表想撑满剩余高度必须用这个锚点——直接量
        // clip_rect.bottom()-cursor.top 会随滚动位置变,越滚列表越高。
        let viewport_bottom = ui.cursor().top() + ui.clip_rect().height();
        self.ui_library_tabbed(ui, viewport_bottom);

        // ---------- 危险操作的二次确认弹框 ----------
        self.ui_confirm_modal(ui.ctx());
    }

    /// 「移除」的二次确认弹框(模态)。
    ///
    /// 所有移除入口都只是把动作写进 `pending_confirm`,真正执行只发生在这里——
    /// 这样“必须二次确认”是一条结构性约束,而不是靠每个按钮各自记得。
    fn ui_confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_confirm.clone() else {
            return;
        };
        // 导入/维护进行中时不允许改库,弹框也一并禁止确认。
        let busy = self.import_active || self.maintenance_active;

        let mut confirm = false;
        let mut cancel = false;
        let (title, body, confirm_label) = match &pending {
            PendingConfirm::RemoveDocument { label, .. } => (
                "移除文档",
                format!(
                    "确定要从索引里移除「{label}」吗?\n\n只删除本程序建的索引记录,磁盘上的原文件不会被删除。"
                ),
                "确认移除".to_owned(),
            ),
            PendingConfirm::RemoveFolder { root, doc_count } => (
                "移除文件夹",
                format!("确定要移除这个文件夹及其下 {doc_count} 篇文档的索引吗?\n\n{root}\n\n")
                    + "只删除本程序建的索引记录,磁盘上的文件与文件夹都不会被删除。",
                format!("确认移除({doc_count} 篇)"),
            ),
            PendingConfirm::ClearMissing { doc_count } => (
                "清除不存在的文档",
                format!(
                    "确定要清除 {doc_count} 篇原文件不存在的文档吗？\n\n只删除库内记录及正文，不删除磁盘文件。清除前会复核路径，保留已恢复或无法确认的文档。"
                ),
                format!("确认清除({doc_count} 篇)"),
            ),
        };

        egui::Modal::new(egui::Id::new("rsou-remove-confirm")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(
                egui::RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(Self::text_primary()),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(body)
                    .size(13.0)
                    .color(Self::text_secondary()),
            );
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Self::danger_button(ui, &confirm_label, 130.0, !busy).clicked() {
                        confirm = true;
                    }
                    // 默认焦点在「取消」上:回车不会误删。
                    if Self::secondary_button(ui, None, "取消", 80.0, true).clicked() {
                        cancel = true;
                    }
                });
            });
            // Esc 等同取消(Modal 自带遮罩,点遮罩不关,避免误触)。
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });

        if confirm && !busy {
            self.pending_confirm = None;
            match pending {
                PendingConfirm::RemoveDocument { id, .. } => self.delete_document(id),
                PendingConfirm::RemoveFolder { root, .. } => self.delete_folder(&root),
                PendingConfirm::ClearMissing { .. } => {
                    self.start_maintain(ctx, MaintainKind::ClearMissing)
                }
            }
        } else if cancel {
            self.pending_confirm = None;
        }
    }

    /// 列表滚动区高度:撑满到视口底边(再留卡片底部内边距),下限 360。
    fn list_fill_height(ui: &egui::Ui, fill_bottom: f32) -> f32 {
        (fill_bottom - ui.cursor().top() - Self::CARD_PADDING as f32).max(360.0)
    }

    /// 文档列表表格(虚拟滚动;行操作收集后统一落地)。`filter` 为文件名过滤词。
    fn ui_documents_table(&mut self, ui: &mut egui::Ui, filter: &str, fill_bottom: f32) {
        // 空态文案要看「整库空」还是「本页签空」——库里只有文件夹导入的文档时,
        // 「单独文件」页是空的,但说「资料库为空」就错了。
        let standalone_empty = self.standalone_documents.is_empty();
        let library_empty = standalone_empty && self.folder_summaries.is_empty();
        if library_empty && self.docs_loading {
            Self::empty_note(ui, "正在读取文档列表…");
            return;
        }
        if self.db.is_some()
            && !self.import_active
            && !self.docs_loading
            && (library_empty || standalone_empty)
        {
            Self::empty_note(
                ui,
                if library_empty {
                    "资料库为空,点「添加文件」或「添加文件夹」开始导入。"
                } else {
                    "没有单独添加的文件。用「添加文件」直接加进来的文档会出现在这里。"
                },
            );
            return;
        }

        // 过滤结果按 (过滤词, 列表换代号) 缓存,不每帧重建。
        // 两个页签共享下标缓存,页签也参与缓存键,避免切换后沿用另一张表的下标。
        let key = (
            LibraryTab::Files,
            filter.trim().to_lowercase(),
            self.documents_version,
        );
        if self.filtered_key.as_ref() != Some(&key) {
            self.filtered_rows = standalone_document_indices(&self.standalone_documents, &key.1);
            self.filtered_key = Some(key);
        }
        let documents = &self.standalone_documents;
        let filtered = &self.filtered_rows;

        let mut action: Option<RowAction> = None;
        let busy = self.import_active || self.maintenance_active;
        let scroll_height = Self::list_fill_height(ui, fill_bottom);
        TableBuilder::new(ui)
            .id_salt("rsou_documents_table")
            .striped(true)
            .vscroll(true)
            // auto_shrink(false) 时高度 = min(可用高度, max_scroll_height);
            // 外层是滚动区,可用高度无限,所以滚动区恒为 scroll_height。
            .auto_shrink([false, false])
            .max_scroll_height(scroll_height)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(56.0))
            .column(Column::exact(76.0))
            .column(Column::exact(76.0))
            .column(Column::exact(52.0))
            .column(Column::exact(80.0))
            .column(Column::exact(104.0))
            .column(Column::exact(284.0))
            .header(Self::TABLE_HEADER_HEIGHT, |mut header| {
                for title in [
                    "文件名",
                    "类型",
                    "大小",
                    "文本量",
                    "分块",
                    "状态",
                    "更新时间",
                    "操作",
                ] {
                    header.col(|ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .size(12.0)
                                .strong()
                                .color(Self::text_muted()),
                        );
                    });
                }
            })
            .body(|body| {
                body.rows(Self::TABLE_ROW_HEIGHT, filtered.len(), |mut row| {
                    let doc = &documents[filtered[row.index()]];
                    // 文件名:截断显示,hover 看完整路径
                    row.col(|ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&doc.file_name)
                                    .size(13.0)
                                    .color(Self::text_primary()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&doc.path);
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(&doc.ext)
                                .size(12.0)
                                .color(Self::text_secondary()),
                        );
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(filebrowser::format_size(
                                doc.file_size.max(0) as u64
                            ))
                            .size(12.0)
                            .color(Self::text_secondary()),
                        );
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(filebrowser::format_size(
                                doc.text_length.max(0) as u64
                            ))
                            .size(12.0)
                            .color(Self::text_secondary()),
                        );
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(doc.chunk_count.to_string())
                                .size(12.0)
                                .color(Self::text_secondary()),
                        );
                    });
                    // 状态:已索引=绿;失败=红,hover 显示中文原因
                    row.col(|ui| {
                        if doc.parse_status == "parsed" {
                            ui.label(
                                egui::RichText::new("已索引")
                                    .size(12.0)
                                    .color(Self::success()),
                            );
                        } else {
                            let status = if doc.parse_error_code.as_deref() == Some("FILE_MISSING") {
                                "原文件不存在"
                            } else {
                                "失败"
                            };
                            let label = ui.label(
                                egui::RichText::new(status).size(12.0).color(Self::danger()),
                            );
                            if let Some(message) = &doc.parse_error_message {
                                label.on_hover_text(message);
                            }
                        }
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(util::format_local_time(doc.updated_at))
                                .size(12.0)
                                .color(Self::text_secondary()),
                        );
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if Self::link_button(ui, "打开").clicked() {
                                action = Some(RowAction::Open(PathBuf::from(&doc.path)));
                            }
                            if Self::link_button(ui, "所在目录").clicked() {
                                action = Some(RowAction::Reveal(PathBuf::from(&doc.path)));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::link_button(ui, "增量更新"))
                                .inner
                                .on_hover_text("更新新增或修改的文件，标记原文件不存在的文档；跳过其他未变化文件")
                                .clicked()
                            {
                                action = Some(RowAction::Rescan(PathBuf::from(&doc.path)));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::link_button(ui, "强制重解析"))
                                .inner
                                .clicked()
                            {
                                action = Some(RowAction::Reimport(PathBuf::from(&doc.path)));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::danger_link_button(ui, "移除"))
                                .inner
                                .clicked()
                            {
                                action = Some(RowAction::AskRemoveDocument(
                                    doc.id,
                                    doc.file_name.clone(),
                                ));
                            }
                        });
                    });
                });
            });

        self.apply_row_action(ui, action);
    }

    /// 页签切换:「已添加文件夹」/「单独文件」。
    /// `fill_bottom` 为文档空间里的视口底边(ui_page_library 在页面顶部算出),
    /// 列表内容据此撑满工作区剩余高度。
    fn ui_library_tabbed(&mut self, ui: &mut egui::Ui, fill_bottom: f32) {
        let folders = self.folder_summaries.len();
        let files = self.standalone_documents.len();
        // 页签栏右侧放过滤与失败清单开关:它们作用于下面的列表,
        // 放在导入卡片里既不相关、切页签时也很跳。
        let hint = match self.library_tab {
            LibraryTab::Folders => format!("{folders} 个来源文件夹"),
            LibraryTab::Files => format!("{files} 篇单独添加"),
        };
        // 两个闭包(页签栏 / 内容)不能同时借 self。
        // 过滤词在页签栏输入、内容区同帧就要读到:用 RefCell 让两个闭包共享
        // 同一份文本;渲染结束后写回 self.doc_filter。
        let mut switch_to: Option<LibraryTab> = None;
        let current = self.library_tab;
        let doc_filter = std::cell::RefCell::new(std::mem::take(&mut self.doc_filter));
        let mut show_failures = self.show_failures;
        let failure_count = self.failed_documents.len();
        Self::tabbed_card(
            ui,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    if Self::tab_button(
                        ui,
                        &format!("已添加文件夹({folders})"),
                        current == LibraryTab::Folders,
                    ) {
                        switch_to = Some(LibraryTab::Folders);
                    }
                    if Self::tab_button(
                        ui,
                        &format!("单独文件({files})"),
                        current == LibraryTab::Files,
                    ) {
                        switch_to = Some(LibraryTab::Files);
                    }
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(hint)
                            .size(12.0)
                            .color(Self::text_muted()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(show_failures, format!("失败 {failure_count}"))
                            .clicked()
                        {
                            show_failures = !show_failures;
                        }
                        Self::clearable_input(
                            ui,
                            &mut doc_filter.borrow_mut(),
                            160.0,
                            match current {
                                LibraryTab::Folders => "按文件夹路径过滤",
                                LibraryTab::Files => "按文件名过滤",
                            },
                        );
                    });
                });
            },
            |ui| {
                let filter = doc_filter.borrow();
                match current {
                    LibraryTab::Folders => self.ui_folders_table(ui, &filter, fill_bottom),
                    LibraryTab::Files => self.ui_documents_table(ui, &filter, fill_bottom),
                }
            },
        );
        self.doc_filter = doc_filter.into_inner();
        self.show_failures = show_failures;
        if let Some(tab) = switch_to {
            self.library_tab = tab;
        }
    }

    /// 文件夹页只展示数据库汇总;按可见行读取摘要,不加载或复制子文件列表。
    fn ui_folders_table(&mut self, ui: &mut egui::Ui, filter: &str, fill_bottom: f32) {
        if self.folder_summaries.is_empty() {
            Self::empty_note(
                ui,
                if self.docs_loading {
                    "正在读取文件夹列表…"
                } else {
                    "还没有添加过文件夹。点上方「添加文件夹」,递归导入后可在这里管理。"
                },
            );
            return;
        }

        let key = (
            LibraryTab::Folders,
            filter.trim().to_lowercase(),
            self.documents_version,
        );
        if self.filtered_key.as_ref() != Some(&key) {
            self.filtered_rows = folder_summary_indices(&self.folder_summaries, &key.1);
            self.filtered_key = Some(key);
        }
        if self.filtered_rows.is_empty() {
            Self::empty_note(ui, "没有匹配的文件夹路径。");
            return;
        }

        let scroll_to = self.pending_library_folder.take().and_then(|root| {
            self.filtered_rows
                .iter()
                .position(|&index| self.folder_summaries[index].root == root)
        });
        let folders = &self.folder_summaries;
        let filtered = &self.filtered_rows;
        let selected = self.selected_library_folder.as_deref();
        let busy = self.import_active || self.maintenance_active;
        let mut action = None;
        let mut selection = None;
        let scroll_height = Self::list_fill_height(ui, fill_bottom);
        let mut table = TableBuilder::new(ui)
            .id_salt("rsou_folders_table")
            .striped(true)
            .vscroll(true)
            .auto_shrink([false, false])
            .max_scroll_height(scroll_height)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::exact(72.0))
            .column(Column::exact(72.0))
            .column(Column::exact(72.0))
            .column(Column::exact(284.0));
        if let Some(index) = scroll_to {
            table = table.scroll_to_row(index, Some(egui::Align::Center));
        }
        table
            .header(Self::TABLE_HEADER_HEIGHT, |mut header| {
                for title in ["文件夹路径", "文档数", "已索引", "失败", "操作"] {
                    header.col(|ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .size(12.0)
                                .strong()
                                .color(Self::text_muted()),
                        );
                    });
                }
            })
            .body(|body| {
                body.rows(Self::TABLE_ROW_HEIGHT, filtered.len(), |mut row| {
                    let folder = &folders[filtered[row.index()]];
                    row.set_selected(selected == Some(folder.root.as_str()));
                    row.col(|ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&folder.root)
                                    .size(13.0)
                                    .color(Self::text_primary()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&folder.root);
                    });
                    row.col(|ui| {
                        ui.label(folder.total.to_string());
                    });
                    row.col(|ui| {
                        ui.label(
                            egui::RichText::new(folder.parsed.to_string()).color(Self::success()),
                        );
                    });
                    row.col(|ui| {
                        let color = if folder.failed > 0 {
                            Self::danger()
                        } else {
                            Self::text_muted()
                        };
                        ui.label(egui::RichText::new(folder.failed.to_string()).color(color));
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if Self::link_button(ui, "定位").clicked() {
                                action = Some(RowAction::RevealFolder(folder.root.clone()));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::link_button(ui, "增量更新"))
                                .inner
                                .on_hover_text("递归更新新增或修改的文件，标记原文件不存在的文档；跳过其他未变化文件")
                                .clicked()
                            {
                                action = Some(RowAction::Rescan(PathBuf::from(&folder.root)));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::link_button(ui, "强制重解析"))
                                .inner
                                .on_hover_text("强制重新解析该文件夹下的全部文档")
                                .clicked()
                            {
                                action = Some(RowAction::ReimportFolder(folder.root.clone()));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::danger_link_button(ui, "移除"))
                                .inner
                                .clicked()
                            {
                                action = Some(RowAction::AskRemoveFolder(
                                    folder.root.clone(),
                                    folder.total,
                                ));
                            }
                        });
                    });
                    if row.response().clicked() {
                        selection = Some(folder.root.clone());
                    }
                });
            });
        if let Some(root) = selection {
            self.selected_library_folder = Some(root);
        }
        self.apply_row_action(ui, action);
    }

    /// 行操作统一落地(两个页签共用)。
    fn apply_row_action(&mut self, ui: &egui::Ui, action: Option<RowAction>) {
        let ctx = ui.ctx().clone();
        match action {
            Some(RowAction::LocateFolder(root)) => self.locate_library_folder(root),
            Some(RowAction::Open(path)) => {
                if let Err(error) = platform::open_path(&path) {
                    self.library_notice = Some(error);
                }
            }
            Some(RowAction::Reveal(path)) => {
                if let Err(error) = platform::reveal_in_folder(&path) {
                    self.library_notice = Some(error);
                }
            }
            Some(RowAction::Rescan(path)) => self.start_rescan(&ctx, path),
            Some(RowAction::Reimport(path)) => self.start_import(&ctx, vec![path], true),
            Some(RowAction::AskRemoveDocument(id, label)) => {
                self.pending_confirm = Some(PendingConfirm::RemoveDocument { id, label });
            }
            Some(RowAction::AskRemoveFolder(root, doc_count)) => {
                self.pending_confirm = Some(PendingConfirm::RemoveFolder { root, doc_count });
            }
            Some(RowAction::ReimportFolder(root)) => {
                self.start_import(&ctx, vec![PathBuf::from(root)], true);
            }
            Some(RowAction::RevealFolder(root)) => {
                if let Err(error) = platform::reveal_in_folder(&PathBuf::from(root)) {
                    self.library_notice = Some(error);
                }
            }
            None => {}
        }
    }

    fn locate_library_folder(&mut self, root: String) {
        if !self
            .folder_summaries
            .iter()
            .any(|folder| folder.root == root)
        {
            self.library_notice = Some("所属文件夹已不在资料库列表中。".to_owned());
            return;
        }
        self.library_tab = LibraryTab::Folders;
        self.doc_filter.clear();
        self.selected_library_folder = Some(root.clone());
        self.pending_library_folder = Some(root);
    }

    /// 清单摘要最多两行,完整路径与错误按需在悬停提示中排版。
    fn failure_summary(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
        let mut job = egui::text::LayoutJob::simple(
            text.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            color,
            ui.available_width(),
        );
        job.wrap.max_rows = 2;
        ui.add(egui::Label::new(job).wrap()).on_hover_text(text)
    }

    /// 顶部筛选固定,清单使用等高虚拟行,仅对视口内的记录排版。
    pub(super) fn ui_failures_panel(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<RowAction> = None;
        let busy = self.import_active || self.maintenance_active;
        let mut close = false;
        ui.horizontal(|ui| {
            Self::card_title(ui, "失败清单");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Self::link_button(ui, "关闭").clicked() {
                    close = true;
                }
            });
        });
        // 关闭这一帧也不再排版列表;下一帧撤下侧栏,释放给主工作区。
        if close {
            self.show_failures = false;
            ui.ctx().request_repaint();
            return;
        }
        self.failure_list_cache
            .refresh(self.documents_version, &self.failed_documents);
        let counts =
            FailureCategory::ALL.map(|category| self.failure_list_cache.indices(category).len());
        let ocr_count = counts[FailureCategory::NeedsOcr as usize];
        let missing_count = counts[FailureCategory::FileMissing as usize];
        if ui
            .add_enabled(
                !busy && missing_count > 0,
                egui::Button::new(format!("一键清除不存在文档（{missing_count}）")),
            )
            .on_hover_text(
                "只清除原文件不存在的库记录；保留已恢复或无法确认的文档，不删除磁盘文件。",
            )
            .clicked()
        {
            self.pending_confirm = Some(PendingConfirm::ClearMissing {
                doc_count: missing_count,
            });
        }
        if ui
            .add_enabled(
                !busy && ocr_count > 0,
                egui::Button::new(format!("全部 OCR（{ocr_count}）")),
            )
            .on_hover_text("识别全部“需要 OCR”的失败文档,成功后自动加入全文索引。")
            .clicked()
        {
            self.start_ocr(ui.ctx());
        }
        let previous_filter = self.failure_filter;
        // 重试成功或移除后,所选类别消失则回到全部。
        if self.failure_filter.is_some_and(|c| counts[c as usize] == 0) {
            self.failure_filter = None;
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.failure_filter,
                None,
                format!("全部 {}", self.failed_documents.len()),
            );
            for category in FailureCategory::ALL {
                let count = counts[category as usize];
                if count > 0 {
                    ui.selectable_value(
                        &mut self.failure_filter,
                        Some(category),
                        format!("{} {count}", category.label()),
                    );
                }
            }
        });
        Self::thin_divider(ui);
        if self.failed_documents.is_empty() {
            Self::empty_note(ui, "没有失败的文档。");
            return;
        }
        let documents = &self.failed_documents;
        let filtered = self
            .failure_filter
            .map(|category| self.failure_list_cache.indices(category));
        let count = filtered.map_or(documents.len(), <[usize]>::len);
        // 文件名一行、路径和原因各最多两行,操作固定两行以兼容最窄侧栏。
        // 高度不随单条文本长度变化,才能在打开/调整宽度时跳过所有屏外记录。
        let row_height = 5.0 * ui.text_style_height(&egui::TextStyle::Body)
            + 2.0 * ui.spacing().interact_size.y
            + 6.0 * ui.spacing().item_spacing.y
            + 16.0;
        let mut table = TableBuilder::new(ui)
            .id_salt("failure_list_table")
            .vscroll(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::top_down(egui::Align::Min))
            .column(Column::remainder().clip(true));
        if previous_filter != self.failure_filter {
            table = table.vertical_scroll_offset(0.0);
        }
        table.body(|body| {
            body.rows(row_height, count, |mut row| {
                let index = filtered.map_or(row.index(), |indices| indices[row.index()]);
                let doc = &documents[index];
                row.set_overline(row.index() > 0);
                row.col(|ui| {
                    ui.push_id(doc.id, |ui| {
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(egui::RichText::new(&doc.file_name).strong())
                                .truncate(),
                        )
                        .on_hover_text(&doc.file_name);
                        Self::failure_summary(ui, &doc.path, Self::text_primary());
                        let message = doc.parse_error_message.as_deref().unwrap_or("解析失败");
                        Self::failure_summary(ui, message, Self::warning());
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled_ui(doc.source_root.is_some(), |ui| {
                                    Self::link_button(ui, "定位所属文件夹")
                                })
                                .inner
                                .on_disabled_hover_text("此文档为单独添加,没有所属入库文件夹。")
                                .clicked()
                                && let Some(root) = &doc.source_root
                            {
                                action = Some(RowAction::LocateFolder(root.clone()));
                            }
                            if Self::link_button(ui, "打开所在目录").clicked() {
                                action = Some(RowAction::Reveal(PathBuf::from(&doc.path)));
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::link_button(ui, "重试"))
                                .inner
                                .clicked()
                            {
                                action = Some(RowAction::Reimport(PathBuf::from(&doc.path)));
                            }
                            if ui
                                .add_enabled_ui(!busy, |ui| Self::danger_link_button(ui, "移除"))
                                .inner
                                .clicked()
                            {
                                action = Some(RowAction::AskRemoveDocument(
                                    doc.id,
                                    doc.file_name.clone(),
                                ));
                            }
                        });
                    });
                });
            });
        });
        self.apply_row_action(ui, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_folder_selects_summary_and_requests_scroll() {
        let mut app = RsouApp::new_state(&egui::Context::default());
        app.folder_summaries = vec![folder_summary("/a", 100000, 1)];
        app.library_tab = LibraryTab::Files;
        app.doc_filter = "不匹配".to_owned();

        app.locate_library_folder("/a".to_owned());

        assert!(app.library_tab == LibraryTab::Folders);
        assert!(app.doc_filter.is_empty());
        assert_eq!(app.selected_library_folder.as_deref(), Some("/a"));
        assert_eq!(app.pending_library_folder.as_deref(), Some("/a"));
        assert!(app.standalone_documents.is_empty());
    }

    #[test]
    fn locate_missing_folder_reports_without_changing_navigation() {
        let mut app = RsouApp::new_state(&egui::Context::default());
        app.library_tab = LibraryTab::Files;
        app.doc_filter = "保留".to_owned();

        app.locate_library_folder("/missing".to_owned());

        assert!(app.library_tab == LibraryTab::Files);
        assert_eq!(app.doc_filter, "保留");
        assert!(app.pending_library_folder.is_none());
        assert!(app.library_notice.is_some());
    }

    fn folder_summary(root: &str, total: u64, failed: u64) -> SourceRootSummary {
        SourceRootSummary {
            root: root.to_owned(),
            total,
            parsed: total - failed,
            failed,
        }
    }

    #[test]
    fn folders_filter_only_summary_paths() {
        let folders = vec![
            folder_summary("/Reports/年度资料", 100000, 12),
            folder_summary("/办公/合同", 20, 2),
        ];
        assert_eq!(folder_summary_indices(&folders, ""), vec![0, 1]);
        assert_eq!(folder_summary_indices(&folders, "report"), vec![0]);
        assert_eq!(folder_summary_indices(&folders, "/办公/合同"), vec![1]);
        assert!(folder_summary_indices(&folders, "子文件.docx").is_empty());
        assert!(folder_summary_indices(&folders, "100000").is_empty());
    }

    /// 造一行文档(只需 source_root 与文件名对这两个页签的选择有意义)。
    fn row(id: i64, name: &str, source_root: Option<&str>) -> DocumentRow {
        DocumentRow {
            id,
            path: format!("/{name}"),
            file_name: name.to_owned(),
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
            indexed_at: Some(0),
            source_root: source_root.map(str::to_owned),
            updated_at: 0,
        }
    }

    #[test]
    fn failure_cache_groups_known_and_unknown_codes() {
        let mut documents: Vec<_> = FailureCategory::ALL
            .into_iter()
            .enumerate()
            .map(|(index, category)| {
                let mut document = row(index as i64, "失败.pdf", None);
                document.parse_error_code = category.code().map(|code| code.as_str().to_owned());
                document
            })
            .collect();
        let mut unknown = row(10, "未知错误.pdf", None);
        unknown.parse_error_code = Some("FUTURE_ERROR".to_owned());
        documents.push(unknown);

        let mut cache = FailureListCache::default();
        cache.refresh(0, &documents);
        for (index, category) in FailureCategory::ALL.into_iter().enumerate() {
            if category == FailureCategory::Other {
                assert_eq!(cache.indices(category), &[index, 10]);
            } else {
                assert_eq!(cache.indices(category), &[index]);
            }
        }
    }

    #[test]
    fn failure_cache_keeps_large_list_indices_in_order() {
        let categories = [
            FailureCategory::NeedsOcr,
            FailureCategory::FileMissing,
            FailureCategory::Malformed,
        ];
        let documents: Vec<_> = (0..1500)
            .map(|index| {
                let mut document = row(index as i64, "失败.pdf", Some("/资料"));
                document.parse_error_code = categories[index % categories.len()]
                    .code()
                    .map(|code| code.as_str().to_owned());
                document
            })
            .collect();

        let mut cache = FailureListCache::default();
        cache.refresh(1, &documents);
        // 重新打开或滚动不换代,复用同一份分类下标。
        let cached = cache.indices(FailureCategory::NeedsOcr).as_ptr();
        cache.refresh(1, &documents);
        assert_eq!(cache.indices(FailureCategory::NeedsOcr).as_ptr(), cached);
        for (offset, category) in categories.into_iter().enumerate() {
            let expected: Vec<_> = (offset..documents.len())
                .step_by(categories.len())
                .collect();
            assert_eq!(cache.indices(category), expected);
        }
        assert_eq!(
            FailureCategory::ALL
                .into_iter()
                .map(|category| cache.indices(category).len())
                .sum::<usize>(),
            documents.len()
        );
    }

    #[test]
    fn failure_cache_refreshes_changed_removed_and_empty_documents() {
        let mut documents = vec![row(1, "甲.pdf", None), row(2, "乙.pdf", None)];
        documents[0].parse_error_code = Some("NEEDS_OCR".to_owned());
        documents[1].parse_error_code = Some("FILE_MISSING".to_owned());
        let mut cache = FailureListCache::default();
        cache.refresh(1, &documents);
        assert_eq!(cache.indices(FailureCategory::NeedsOcr), &[0]);
        assert_eq!(cache.indices(FailureCategory::FileMissing), &[1]);

        // 条数相同但错误码变化,也必须按换代号重建。
        documents[1].parse_error_code = Some("IO".to_owned());
        cache.refresh(2, &documents);
        assert!(cache.indices(FailureCategory::FileMissing).is_empty());
        assert_eq!(cache.indices(FailureCategory::Io), &[1]);

        documents.remove(0);
        cache.refresh(3, &documents);
        assert!(cache.indices(FailureCategory::NeedsOcr).is_empty());
        assert_eq!(cache.indices(FailureCategory::Io), &[0]);

        cache.refresh(4, &[]);
        for category in FailureCategory::ALL {
            assert!(cache.indices(category).is_empty());
        }
    }

    #[test]
    fn files_tab_lists_only_standalone_documents() {
        // 回归测试:曾经「单独文件」页的数目对、但列表把文件夹的子文件也列了
        // 出来(过滤时漏了 source_root 这一维)。
        let docs = vec![
            row(1, "文件夹里的.txt", Some("/a")),
            row(2, "单独加的.txt", None),
            row(3, "另一个文件夹里的.txt", Some("/b")),
        ];

        let files = standalone_document_indices(&docs, "");
        assert_eq!(files, vec![1], "只应列出 source_root 为空的文档");
        let names: Vec<&str> = files.iter().map(|i| docs[*i].file_name.as_str()).collect();
        assert_eq!(names, ["单独加的.txt"]);

        // 文件夹页只展示来源摘要,不使用文档下标或加载子文件。
    }

    #[test]
    fn files_tab_stays_correct_under_filter() {
        let docs = vec![
            row(1, "合同甲.txt", Some("/a")),
            row(2, "合同乙.txt", None),
            row(3, "合同丙.txt", None),
            row(4, "无关.txt", None),
        ];
        // 过滤词与页签两个条件必须**同时**生效(与,不是或)。
        let files = standalone_document_indices(&docs, "合同");
        let names: Vec<&str> = files.iter().map(|i| docs[*i].file_name.as_str()).collect();
        assert_eq!(names, ["合同乙.txt", "合同丙.txt"]);
    }

    #[test]
    fn files_tab_is_empty_when_library_only_has_folders() {
        let docs = vec![row(1, "a.txt", Some("/a")), row(2, "b.txt", Some("/a"))];
        assert!(standalone_document_indices(&docs, "").is_empty());
        assert_eq!(docs.iter().filter(|d| d.source_root.is_some()).count(), 2);
    }

    #[test]
    fn visible_indices_are_not_affected_by_row_order() {
        // 下标必须指回原数组,不是「筛完之后的顺序」。
        let docs = vec![row(1, "x.txt", Some("/a")), row(2, "y.txt", None)];
        let files = standalone_document_indices(&docs, "");
        assert_eq!(files, vec![1]);
        assert_eq!(docs[files[0]].file_name, "y.txt");
    }

    #[test]
    fn gap_between_tab_bar_and_content_is_small() {
        // 回归测试:页签与内容曾经是两张卡片,中间空出近四十像素。
        // 这里量一下真实几何:记下页签按钮底边与内容首行的顶边之差。
        use std::cell::Cell;
        let ctx = egui::Context::default();
        let tab_bottom = Cell::new(0.0f32);
        let content_top = Cell::new(0.0f32);
        let content_top_ref = Cell::new(false);
        for _ in 0..3 {
            let mut out = ctx.run_ui(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::app::RsouApp::tabbed_card(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                let r = ui.add(egui::Button::new("已添加文件夹(1)"));
                                tab_bottom.set(r.rect.bottom());
                            });
                        },
                        |ui| {
                            let r = ui.label("内容首行");
                            if !content_top_ref.get() {
                                content_top.set(r.rect.top());
                                content_top_ref.set(true);
                            }
                        },
                    );
                });
            });
            out.textures_delta.clear();
        }
        let gap = content_top.get() - tab_bottom.get();
        // 全宽分隔线(1px)+ 内容上内边距(14px)≈ 15px。
        // 旧的两卡片结构是 ~40。阈值取 24:足够宽松地容忍样式微调,
        // 又能把「又变回两张卡片」这种回归挡住。
        assert!(
            (0.0..24.0).contains(&gap),
            "页签与内容之间的间隙应小于 24px,实际 {gap}px"
        );
    }

    #[test]
    fn tabbed_panel_renders_without_panic() {
        // tabbed_card 是自绘布局代码(页签栏 + 全宽分隔线 + 内容),
        // 负尺寸矩形/内边距算错都会 panic。空跑几帧兜底。
        let ctx = egui::Context::default();
        for _ in 0..3 {
            let mut out = ctx.run_ui(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::app::RsouApp::tabbed_card(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                let _ = RsouApp::tab_button(ui, "已添加文件夹(1)", true);
                                let _ = RsouApp::tab_button(ui, "单独文件(0)", false);
                            });
                        },
                        |ui| {
                            ui.label("内容区");
                        },
                    );
                });
            });
            out.textures_delta.clear();
        }
    }

    #[test]
    fn cancelling_progress_card_shows_waiting_state() {
        fn collect_text(shape: &egui::epaint::Shape, labels: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(text) => labels.push(text.galley.job.text.clone()),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect_text(shape, labels);
                    }
                }
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let mut app = RsouApp::new_state(&ctx);
        app.import_active = true;
        app.import_progress.cancelling = true;
        for _ in 0..3 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| app.ui_page_library(ui),
            );
            let mut labels = Vec::new();
            for shape in &output.shapes {
                collect_text(&shape.shape, &mut labels);
            }
            assert!(labels.iter().any(|s| s == "正在取消"));
            assert!(labels.iter().any(|s| s == "取消中"));
            assert!(labels.iter().any(|s| s.contains("正在停止后台任务")));
            assert!(!labels.iter().any(|s| s == "正在导入"));
            output.textures_delta.clear();
        }
    }

    #[test]
    fn file_name_matches_is_case_insensitive_and_empty_means_all() {
        assert!(file_name_matches("Report.DOCX", "report"));
        assert!(file_name_matches("任何名字", ""));
        assert!(!file_name_matches("a.txt", "b"));
    }
}
