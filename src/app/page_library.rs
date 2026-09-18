//! 资料库页:文档导入、进度卡、两个页签(文件夹树 / 单独文件)与失败清单抽屉。

use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use egui_ltreeview::{Action as TreeAction, NodeBuilder, TreeView};
use rsou_lib::filebrowser;

use super::*;

/// 行内操作意图(表格/树闭包结束后再落地,避免借用冲突)。
enum RowAction {
    Open(PathBuf),
    Reveal(PathBuf),
    /// 强制重解析该文件
    Reimport(PathBuf),
    /// 请求移除单个文档(需二次确认)
    AskRemoveDocument(i64, String),
    /// 请求移除整个来源文件夹(需二次确认)
    AskRemoveFolder(String, usize),
    /// 重解析整个来源文件夹(递归强制重导)
    ReimportFolder(String),
    /// 定位来源文件夹
    RevealFolder(String),
}

impl RsouApp {
    pub(crate) fn ui_page_library(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = self.db_error.clone() {
            Self::work_panel(ui, "索引", "索引库不可用", "", None, |ui| {
                ui.label(
                    egui::RichText::new(format!("无法打开索引库,导入与检索不可用:\n{error}"))
                        .color(Self::amber()),
                );
            });
            ui.add_space(13.0);
        }

        // ---------- 顶部操作行 ----------
        let mut want_files = false;
        let mut want_folder = false;
        let db_ready = self.db.is_some();
        // 维护(重建/清空)进行中时导入也禁用,两条写路径互斥。
        let importing = self.import_active || self.maintenance_active;
        Self::work_panel(
            ui,
            "文档",
            "导入文档",
            "支持 Word/Excel/PPT/PDF/文本/电子书",
            None,
            |ui| {
                ui.horizontal(|ui| {
                    if Self::primary_button(ui, "添加文件", 96.0, db_ready && !importing).clicked()
                    {
                        want_files = true;
                    }
                    ui.add_enabled_ui(db_ready && !importing, |ui| {
                        if Self::secondary_button(ui, "添加文件夹", 110.0).clicked() {
                            want_folder = true;
                        }
                    });
                    ui.add_space(14.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.doc_filter)
                            .desired_width(180.0)
                            .hint_text("按文件名过滤"),
                    );
                    ui.checkbox(&mut self.show_failures, "显示失败清单");
                });
                if let Some(notice) = &self.library_notice {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(notice).size(13.0).color(Self::teal()));
                }
            },
        );
        if want_files {
            self.pending_dialog = Some(DialogRequest::ImportFiles);
        }
        if want_folder {
            self.pending_dialog = Some(DialogRequest::ImportFolder);
        }
        ui.add_space(13.0);

        // ---------- 已加入索引的清单:两个页签 ----------
        self.ui_library_tabs(ui);
        ui.add_space(13.0);

        // ---------- 导入进度卡 ----------
        if self.import_active {
            let mut cancel = false;
            Self::work_panel(
                ui,
                "进度",
                "正在导入",
                "解析在后台线程并行,写库串行",
                None,
                |ui| {
                    let c = &self.import_progress.counts;
                    let fraction = if c.total == 0 {
                        0.0
                    } else {
                        c.processed as f32 / c.total as f32
                    };
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width((ui.available_width() - 120.0).max(80.0))
                            .text(format!("已处理 {}/{}", c.processed, c.total)),
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "成功 {} · 失败 {} · 跳过 {}",
                                c.ok, c.failed, c.skipped
                            ))
                            .size(13.0)
                            .color(Self::muted()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if Self::secondary_button(ui, "取消", 72.0).clicked() {
                                cancel = true;
                            }
                        });
                    });
                    if let Some(path) = &self.import_progress.current_path {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!("最近: {}", path.display()))
                                .size(12.0)
                                .color(Self::soft()),
                        );
                    }
                    for (name, message) in self.import_progress.recent_failures.iter().rev().take(5)
                    {
                        ui.label(
                            egui::RichText::new(format!("{name}: {message}"))
                                .size(12.0)
                                .color(Self::amber()),
                        );
                    }
                },
            );
            if cancel {
                self.cancel_import();
            }
            ui.add_space(13.0);
        }

        // ---------- 文档清单(两个页签)----------
        match self.library_tab {
            LibraryTab::Folders => self.ui_folder_tree(ui),
            LibraryTab::Files => self.ui_documents_table(ui),
        }
        ui.add_space(13.0);

        // ---------- 失败清单抽屉 ----------
        if self.show_failures {
            self.ui_failures_drawer(ui);
        }

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
        };

        egui::Modal::new(egui::Id::new("rsou-remove-confirm")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(
                egui::RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(Self::ink()),
            );
            ui.add_space(8.0);
            ui.label(egui::RichText::new(body).size(13.0).color(Self::muted()));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                // 默认焦点在「取消」上:回车不会误删。
                if Self::secondary_button(ui, "取消", 80.0).clicked() {
                    cancel = true;
                }
                if Self::primary_button(ui, &confirm_label, 150.0, !busy).clicked() {
                    confirm = true;
                }
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
            }
        } else if cancel {
            self.pending_confirm = None;
        }
    }

    /// 文档列表表格(虚拟滚动;行操作收集后统一落地)。
    fn ui_documents_table(&mut self, ui: &mut egui::Ui) {
        // 空态文案要看「整库空」还是「本页签空」——库里只有文件夹导入的文档时,
        // 「单独文件」页是空的,但说「资料库为空」就错了。
        let library_empty = self.documents.is_empty();
        let standalone_empty = self.documents.iter().all(|d| d.source_root.is_some());
        if library_empty && self.docs_loading {
            Self::work_panel(ui, "列表", "单独文件", "", None, |ui| {
                ui.label(
                    egui::RichText::new("正在读取文档列表…")
                        .size(15.0)
                        .color(Self::muted()),
                );
            });
            return;
        }
        if self.db.is_some()
            && !self.import_active
            && !self.docs_loading
            && (library_empty || standalone_empty)
        {
            Self::work_panel(ui, "列表", "单独文件", "", None, |ui| {
                ui.label(
                    egui::RichText::new(if library_empty {
                        "资料库为空,点「添加文件」或「添加文件夹」开始导入。"
                    } else {
                        "没有单独添加的文件。用「添加文件」直接加进来的文档会出现在这里。"
                    })
                    .size(15.0)
                    .color(Self::muted()),
                );
            });
            return;
        }

        // 过滤结果按 (过滤词, 列表换代号) 缓存,不每帧重建。
        // 本表只服务「单独文件」页(「文件夹」页是另一棵树),所以键里不需要页签。
        let key = (
            self.doc_filter.trim().to_lowercase(),
            self.documents_version,
        );
        if self.filtered_key.as_ref() != Some(&key) {
            self.filtered_docs = standalone_document_indices(&self.documents, &key.0);
            self.filtered_key = Some(key);
        }
        let documents = &self.documents;
        let filtered = &self.filtered_docs;

        let mut action: Option<RowAction> = None;
        let busy = self.import_active || self.maintenance_active;
        Self::work_panel(
            ui,
            "列表",
            "单独文件",
            &format!("{} 篇", filtered.len()),
            None,
            |ui| {
                TableBuilder::new(ui)
                    .id_salt("rsou_documents_table")
                    .striped(true)
                    .vscroll(true)
                    .min_scrolled_height(360.0)
                    .max_scroll_height(360.0)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::remainder().at_least(160.0).clip(true))
                    .column(Column::exact(56.0))
                    .column(Column::exact(76.0))
                    .column(Column::exact(76.0))
                    .column(Column::exact(52.0))
                    .column(Column::exact(80.0))
                    .column(Column::exact(104.0))
                    .column(Column::exact(196.0))
                    .header(26.0, |mut header| {
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
                                        .color(Self::muted()),
                                );
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(26.0, filtered.len(), |mut row| {
                            let doc = &documents[filtered[row.index()]];
                            // 文件名:截断显示,hover 看完整路径
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(&doc.file_name)
                                        .size(13.0)
                                        .color(Self::ink()),
                                )
                                .on_hover_text(&doc.path);
                            });
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(&doc.ext)
                                        .size(12.0)
                                        .color(Self::muted()),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(filebrowser::format_size(
                                        doc.file_size.max(0) as u64,
                                    ))
                                    .size(12.0)
                                    .color(Self::muted()),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(filebrowser::format_size(
                                        doc.text_length.max(0) as u64,
                                    ))
                                    .size(12.0)
                                    .color(Self::muted()),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(doc.chunk_count.to_string())
                                        .size(12.0)
                                        .color(Self::muted()),
                                );
                            });
                            // 状态徽标:已索引=teal;失败=amber,hover 显示中文原因
                            row.col(|ui| {
                                if doc.parse_status == "parsed" {
                                    ui.label(
                                        egui::RichText::new("已索引")
                                            .size(12.0)
                                            .color(Self::teal()),
                                    );
                                } else {
                                    let label = ui.label(
                                        egui::RichText::new("失败").size(12.0).color(Self::amber()),
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
                                        .color(Self::muted()),
                                );
                            });
                            row.col(|ui| {
                                ui.horizontal(|ui| {
                                    if link_button(ui, "打开") {
                                        action = Some(RowAction::Open(PathBuf::from(&doc.path)));
                                    }
                                    if link_button(ui, "所在目录") {
                                        action = Some(RowAction::Reveal(PathBuf::from(&doc.path)));
                                    }
                                    if ui.add_enabled(!busy, link("重解析")).clicked() {
                                        action =
                                            Some(RowAction::Reimport(PathBuf::from(&doc.path)));
                                    }
                                    if ui.add_enabled(!busy, link("移除")).clicked() {
                                        action = Some(RowAction::AskRemoveDocument(
                                            doc.id,
                                            doc.file_name.clone(),
                                        ));
                                    }
                                });
                            });
                        });
                    });
            },
        );

        self.apply_row_action(ui, action);
    }

    /// 页签切换:「已添加文件夹」/「单独文件」。
    fn ui_library_tabs(&mut self, ui: &mut egui::Ui) {
        let folders = self.folder_groups.len();
        let files = self
            .documents
            .iter()
            .filter(|d| d.source_root.is_none())
            .count();
        Self::work_panel(
            ui,
            "清单",
            "已加入索引",
            "文件夹可展开查看下属文档",
            None,
            |ui| {
                ui.horizontal(|ui| {
                    // 用自绘按钮而不是 egui 的 SelectableLabel:后者在本地主题
                    // (全零圆角 + 自定色板)下选中态几乎看不出来。
                    if tab_button(
                        ui,
                        &format!("已添加文件夹({folders})"),
                        self.library_tab == LibraryTab::Folders,
                    ) {
                        self.library_tab = LibraryTab::Folders;
                    }
                    if tab_button(
                        ui,
                        &format!("单独文件({files})"),
                        self.library_tab == LibraryTab::Files,
                    ) {
                        self.library_tab = LibraryTab::Files;
                    }
                    ui.add_space(14.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.doc_filter)
                            .desired_width(180.0)
                            .hint_text("按文件名过滤"),
                    );
                    ui.checkbox(&mut self.show_failures, "显示失败清单");
                });
                if let Some(notice) = &self.library_notice {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(notice).size(13.0).color(Self::teal()));
                }
            },
        );
    }

    /// 文件夹页:树形展示「来源文件夹 → 文档」,文件夹可展开。
    fn ui_folder_tree(&mut self, ui: &mut egui::Ui) {
        if self.folder_groups.is_empty() {
            let loading = self.docs_loading;
            Self::work_panel(ui, "文件夹", "已添加文件夹", "", None, |ui| {
                ui.label(
                    egui::RichText::new(if loading {
                        "正在读取列表…"
                    } else {
                        "还没有添加过文件夹。点上方「添加文件夹」,导入后可在这里展开查看。"
                    })
                    .size(15.0)
                    .color(Self::muted()),
                );
            });
            return;
        }

        let busy = self.import_active || self.maintenance_active;
        let filter = self.doc_filter.trim().to_lowercase();
        // 先在闭包外把要用的东西取出来:闭包里要同时 &mut self(folder_expanded)
        // 与读 folder_groups,不能同时借。
        let groups: Vec<(String, Vec<DocumentRow>, usize)> = self
            .folder_groups
            .iter()
            .map(|g| {
                let matched = g.matched(&filter);
                (g.root.clone(), g.documents.clone(), matched)
            })
            .collect();
        // 有过滤词时,一个都没命中的文件夹整体隐藏(否则满屏空文件夹)。
        let visible: Vec<&(String, Vec<DocumentRow>, usize)> = groups
            .iter()
            .filter(|(_, _, matched)| filter.is_empty() || *matched > 0)
            .collect();
        let total_docs: usize = visible.iter().map(|(_, _, m)| *m).sum();

        let mut action: Option<RowAction> = None;
        let mut node_count = 0usize;
        Self::work_panel(
            ui,
            "文件夹",
            "已添加文件夹",
            &format!("{} 个文件夹 · {} 篇", visible.len(), total_docs),
            None,
            |ui| {
                if visible.is_empty() {
                    ui.label(
                        egui::RichText::new("没有匹配的文件名。")
                            .size(14.0)
                            .color(Self::muted()),
                    );
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("rsou_folder_tree")
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let state = &mut self.library_tree_state;
                        let (_, tree_actions) = TreeView::new(ui.id().with("rsou-folder-tree"))
                            .allow_multi_selection(false)
                            .allow_drag_and_drop(false)
                            .show_state(ui, state, |builder| {
                                for (root, documents, matched) in &visible {
                                    let folder_id = LibraryNode::Folder(root.clone());
                                    // 目录节点:label 用闭包以便挂行内按钮。
                                    let root_owned = root.clone();
                                    let doc_count = documents.len();
                                    let matched = *matched;
                                    // 文件夹行的三个操作:定位 / 重解析 / 移除。
                                    // 挂在 label_ui 上(行内右侧),与文档行一致;
                                    // 点按钮不会触发树的选中/展开(已验证)。
                                    let folder_slot: std::rc::Rc<
                                        std::cell::RefCell<Option<RowAction>>,
                                    > = std::rc::Rc::new(std::cell::RefCell::new(None));
                                    let folder_slot_in = folder_slot.clone();
                                    let root_for_reimport = root.clone();
                                    let root_for_reveal = root.clone();
                                    let root_for_remove = root.clone();
                                    let busy_here = busy;
                                    if builder.node(
                                        NodeBuilder::dir(folder_id).label_ui(move |ui| {
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "{root_owned}  ({matched}/{doc_count})"
                                                    ))
                                                    .size(13.0)
                                                    .color(RsouApp::ink()),
                                                );
                                                // 右侧三个操作。宽度有限的窗口里
                                                // 会挤,但树本身可横向滚动。
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui
                                                            .add_enabled(!busy_here, link("移除"))
                                                            .on_hover_text(
                                                                "从索引移除该文件夹及其下全部文档(不动磁盘文件)",
                                                            )
                                                            .clicked()
                                                        {
                                                            *folder_slot_in.borrow_mut() =
                                                                Some(RowAction::AskRemoveFolder(
                                                                    root_for_remove.clone(),
                                                                    doc_count,
                                                                ));
                                                        }
                                                        if ui
                                                            .add_enabled(
                                                                !busy_here,
                                                                link("重解析"),
                                                            )
                                                            .on_hover_text(
                                                                "强制重新解析该文件夹下的全部文档",
                                                            )
                                                            .clicked()
                                                        {
                                                            *folder_slot_in.borrow_mut() =
                                                                Some(RowAction::ReimportFolder(
                                                                    root_for_reimport.clone(),
                                                                ));
                                                        }
                                                        if link_button(ui, "定位") {
                                                            *folder_slot_in.borrow_mut() =
                                                                Some(RowAction::RevealFolder(
                                                                    root_for_reveal.clone(),
                                                                ));
                                                        }
                                                    },
                                                );
                                            });
                                        }),
                                    ) {
                                        for document in documents {
                                            // 过滤词下只展示命中的文档。
                                            if !file_name_matches(&document.file_name, &filter) {
                                                continue;
                                            }
                                            node_count += 1;
                                            let id = document.id;
                                            let name = document.file_name.clone();
                                            let status_ok = document.parse_status == "parsed";
                                            let busy_here = busy;
                                            // label_ui 的闭包按值捕获,而闭包结束后还要读结果,
                                            // 所以用 Rc<RefCell> 共享槽位。
                                            let local: std::rc::Rc<std::cell::RefCell<Option<RowAction>>> =
                                                std::rc::Rc::new(std::cell::RefCell::new(None));
                                            let local_in = local.clone();
                                            builder.node(
                                                NodeBuilder::leaf(LibraryNode::Document(id))
                                                    .label_ui(move |ui| {
                                                        // 行内:文件名 + 状态 + 按钮。
                                                        // 按钮放在行的右侧;点按钮不会
                                                        // 触发树的选择/展开(已验证)。
                                                        ui.horizontal(|ui| {
                                                            let color = if status_ok {
                                                                RsouApp::ink()
                                                            } else {
                                                                RsouApp::amber()
                                                            };
                                                            ui.label(
                                                                egui::RichText::new(&name)
                                                                    .size(13.0)
                                                                    .color(color),
                                                            );
                                                            if !status_ok {
                                                                ui.label(
                                                                    egui::RichText::new("失败")
                                                                        .size(11.0)
                                                                        .color(RsouApp::amber()),
                                                                );
                                                            }
                                                            ui.with_layout(
                                                                egui::Layout::right_to_left(
                                                                    egui::Align::Center,
                                                                ),
                                                                |ui| {
                                                                    if ui
                                                                        .add_enabled(
                                                                            !busy_here,
                                                                            link("移除"),
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        *local_in.borrow_mut() = Some(
                                                                            RowAction::AskRemoveDocument(
                                                                                id,
                                                                                name.clone(),
                                                                            ),
                                                                        );
                                                                    }
                                                                },
                                                            );
                                                        });
                                                    }),
                                            );
                                            if let Some(a) = local.borrow_mut().take() {
                                                action = Some(a);
                                            }
                                        }
                                        if let Some(a) = folder_slot.borrow_mut().take() {
                                            action = Some(a);
                                        }
                                        builder.close_dir();
                                    } else if let Some(a) = folder_slot.borrow_mut().take() {
                                        // 折叠状态下点按钮:label_ui 仍会渲染,
                                        // 这里同样要取出动作。
                                        action = Some(a);
                                    }
                                }
                            });
                        // 树自带的 Action:双击/回车激活 = 打开文件。
                        for tree_action in tree_actions {
                            if let TreeAction::Activate(activate) = tree_action {
                                for node in activate.selected {
                                    if let LibraryNode::Document(id) = node
                                        && let Some(document) = self
                                            .folder_groups
                                            .iter()
                                            .flat_map(|g| g.documents.iter())
                                            .find(|d| d.id == id)
                                    {
                                        action = Some(RowAction::Open(PathBuf::from(&document.path)));
                                    }
                                }
                            }
                        }
                    });
            },
        );
        let _ = node_count;
        self.apply_row_action(ui, action);
    }

    /// 行操作统一落地(两个页签共用)。
    fn apply_row_action(&mut self, ui: &egui::Ui, action: Option<RowAction>) {
        let ctx = ui.ctx().clone();
        match action {
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

    /// 失败清单抽屉:文件名 + 中文原因 + 重试/移除。
    fn ui_failures_drawer(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<RowAction> = None;
        let busy = self.import_active || self.maintenance_active;
        Self::work_panel(
            ui,
            "失败",
            "失败清单",
            &format!("{} 个文件", self.failed_documents.len()),
            None,
            |ui| {
                if self.failed_documents.is_empty() {
                    ui.label(
                        egui::RichText::new("没有失败的文档。")
                            .size(14.0)
                            .color(Self::muted()),
                    );
                    return;
                }
                for doc in &self.failed_documents {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&doc.file_name)
                                .size(13.0)
                                .strong()
                                .color(Self::ink()),
                        )
                        .on_hover_text(&doc.path);
                        ui.label(
                            egui::RichText::new(
                                doc.parse_error_message.as_deref().unwrap_or("解析失败"),
                            )
                            .size(12.0)
                            .color(Self::amber()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(!busy, link("移除")).clicked() {
                                action = Some(RowAction::AskRemoveDocument(
                                    doc.id,
                                    doc.file_name.clone(),
                                ));
                            }
                            if ui.add_enabled(!busy, link("重试")).clicked() {
                                action = Some(RowAction::Reimport(PathBuf::from(&doc.path)));
                            }
                        });
                    });
                    ui.add_space(2.0);
                }
            },
        );
        self.apply_row_action(ui, action);
    }
}

/// 行内文字按钮(不强调)。
fn link(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).size(12.0).color(RsouApp::blue())).frame(false)
}

fn link_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(link(text)).clicked()
}

/// 页签按钮:选中态用蓝底白字,未选中是素面。
///
/// 不用 `SelectableLabel`:本地主题把圆角与描边都置零,选中态几乎看不出来。
fn tab_button(ui: &mut egui::Ui, text: &str, selected: bool) -> bool {
    let (fill, text_color) = if selected {
        (RsouApp::blue(), Color32::WHITE)
    } else {
        (RsouApp::surface(), RsouApp::muted())
    };
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(13.0).color(text_color))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, RsouApp::line()))
            .min_size(egui::vec2(0.0, 28.0)),
    )
    .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无头渲染资料库页的树形视图。
    ///
    /// 树是自绘布局代码,里面任何 panic(`expect`、索引越界、负尺寸矩形)都会让
    /// 整个应用卡死、用户连「移除」都点不到——所以在 CI 里空跑几帧兜底。
    /// 视觉效果仍需人工确认,自动化测试不覆盖外观。
    fn render_tree(groups: Vec<FolderGroup>, filter: &str, frames: usize) {
        let ctx = egui::Context::default();
        let mut state = egui_ltreeview::TreeViewState::<LibraryNode>::default();
        for _ in 0..frames {
            let mut output = ctx.run_ui(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let visible: Vec<_> = groups
                        .iter()
                        .filter(|g| filter.is_empty() || g.matched(filter) > 0)
                        .collect();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let (_, _actions) = TreeView::new(ui.id().with("t"))
                            .allow_multi_selection(false)
                            .show_state(ui, &mut state, |builder| {
                                for group in &visible {
                                    let docs: Vec<_> = group
                                        .documents
                                        .iter()
                                        .filter(|d| file_name_matches(&d.file_name, filter))
                                        .collect();
                                    let id = LibraryNode::Folder(group.root.clone());
                                    // 目录节点带行内操作(与生产代码同一条 label_ui 路径)。
                                    let slot: std::rc::Rc<std::cell::RefCell<Option<RowAction>>> =
                                        std::rc::Rc::new(std::cell::RefCell::new(None));
                                    let slot_in = slot.clone();
                                    let root = group.root.clone();
                                    let count = group.documents.len();
                                    if builder.node(NodeBuilder::dir(id).label_ui(move |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(format!("{root} ({count})"));
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if link_button(ui, "移除") {
                                                        *slot_in.borrow_mut() =
                                                            Some(RowAction::AskRemoveFolder(
                                                                root.clone(),
                                                                count,
                                                            ));
                                                    }
                                                    let _ = link_button(ui, "重解析");
                                                    let _ = link_button(ui, "定位");
                                                },
                                            );
                                        });
                                    })) {
                                        for document in docs {
                                            let name = document.file_name.clone();
                                            builder.node(
                                                NodeBuilder::leaf(LibraryNode::Document(
                                                    document.id,
                                                ))
                                                .label_ui(move |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label(name.clone());
                                                        if link_button(ui, "移除") {
                                                            // 与生产代码同构:写槽位。
                                                        }
                                                    });
                                                }),
                                            );
                                        }
                                        builder.close_dir();
                                    }
                                }
                            });
                    });
                });
            });
            // 无头运行不落地纹理,丢掉前必须先清空(否则 epaint 会 panic)
            output.textures_delta.clear();
        }
    }

    fn group(root: &str, names: &[&str]) -> FolderGroup {
        FolderGroup {
            root: root.to_owned(),
            documents: names
                .iter()
                .enumerate()
                .map(|(i, name)| DocumentRow {
                    id: i as i64 + 1,
                    path: format!("{root}/{name}"),
                    file_name: (*name).to_owned(),
                    title: String::new(),
                    ext: "docx".to_owned(),
                    file_type: "word".to_owned(),
                    file_size: 1024,
                    file_mtime_ms: 0,
                    parse_status: if i % 3 == 0 { "failed" } else { "parsed" }.to_owned(),
                    parse_error_code: None,
                    parse_error_message: None,
                    text_length: 100,
                    chunk_count: 1,
                    indexed_at: Some(0),
                    source_root: Some(root.to_owned()),
                    updated_at: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn renders_folder_tree_without_panic() {
        // 空列表
        render_tree(Vec::new(), "", 2);
        // 单文件夹多文档(含失败状态)
        render_tree(
            vec![group("/home/u/docs", &["a.docx", "b.pdf", "c.txt"])],
            "",
            3,
        );
        // 多文件夹 + 中文路径
        render_tree(
            vec![
                group("/home/u/办公文档", &["林圣杰三好.docx", "说明.pdf"]),
                group(r"C:\Users\u\Desktop\合同", &["甲.docx"]),
            ],
            "",
            3,
        );
    }

    #[test]
    fn renders_folder_tree_with_filter_without_panic() {
        let groups = vec![group("/d", &["合同甲.docx", "合同乙.docx", "无关.pdf"])];
        // 命中部分文档
        render_tree(groups.clone(), "合同", 2);
        // 一个都不命中(整组被隐藏)
        render_tree(groups, "不存在的名字", 2);
    }

    #[test]
    fn renders_tree_with_long_windows_paths_without_panic() {
        // 曾经的 \\?\ 前缀正是因为路径可能超 MAX_PATH 才出现;这里用一条
        // 很长的中文路径压一压布局(截断/换行/负尺寸矩形都容易在这里暴露)。
        let long = format!(
            "{}\\办公文档",
            r"C:\Users\ljw\Desktop".to_owned() + &"\\子目录".repeat(30)
        );
        render_tree(
            vec![
                group(
                    &long,
                    &["林圣杰三好.docx", "很长的文件名".repeat(20).as_str()],
                ),
                group("/短", &["a.txt"]),
            ],
            "",
            3,
        );
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

        // 「文件夹」页不走这个函数(它是树,数据来自 folder_groups),所以
        // 这里不测它——避免把「两个页签共用一条规则」的错觉固化进测试。
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
    fn matched_counts_only_matching_documents() {
        let g = group("/d", &["合同甲.docx", "合同乙.docx", "无关.pdf"]);
        assert_eq!(g.matched(""), 3, "空过滤词应全命中");
        assert_eq!(g.matched("合同"), 2);
        assert_eq!(g.matched("无关"), 1);
        assert_eq!(g.matched("没有这个"), 0);
    }

    #[test]
    fn file_name_matches_is_case_insensitive_and_empty_means_all() {
        assert!(file_name_matches("Report.DOCX", "report"));
        assert!(file_name_matches("任何名字", ""));
        assert!(!file_name_matches("a.txt", "b"));
    }
}
