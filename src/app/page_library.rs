//! 资料库页:文档导入、进度卡、文档表格与失败清单抽屉。

use std::path::PathBuf;

use egui_extras::{Column, TableBuilder};
use rsou_lib::filebrowser;

use super::*;

/// 行内操作意图(表格闭包结束后再落地,避免借用冲突)。
enum RowAction {
    Open(PathBuf),
    Reveal(PathBuf),
    /// 强制重解析该文件
    Reimport(PathBuf),
    Delete(i64),
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

        // ---------- 文档表格 ----------
        self.ui_documents_table(ui);
        ui.add_space(13.0);

        // ---------- 失败清单抽屉 ----------
        if self.show_failures {
            self.ui_failures_drawer(ui);
        }
    }

    /// 文档列表表格(虚拟滚动;行操作收集后统一落地)。
    fn ui_documents_table(&mut self, ui: &mut egui::Ui) {
        if self.documents.is_empty() && self.docs_loading {
            Self::work_panel(ui, "列表", "文档列表", "", None, |ui| {
                ui.label(
                    egui::RichText::new("正在读取文档列表…")
                        .size(15.0)
                        .color(Self::muted()),
                );
            });
            return;
        }
        if self.documents.is_empty()
            && self.db.is_some()
            && !self.import_active
            && !self.docs_loading
        {
            Self::work_panel(ui, "列表", "文档列表", "", None, |ui| {
                ui.label(
                    egui::RichText::new("资料库为空,点「添加文件」或「添加文件夹」开始导入。")
                        .size(15.0)
                        .color(Self::muted()),
                );
            });
            return;
        }

        // 过滤结果按 (过滤词, 列表换代号) 缓存,不每帧重建。
        let key = (
            self.doc_filter.trim().to_lowercase(),
            self.documents_version,
        );
        if self.filtered_key.as_ref() != Some(&key) {
            let filter = &key.0;
            self.filtered_docs = self
                .documents
                .iter()
                .enumerate()
                .filter(|(_, d)| filter.is_empty() || d.file_name.to_lowercase().contains(filter))
                .map(|(i, _)| i)
                .collect();
            self.filtered_key = Some(key);
        }
        let documents = &self.documents;
        let filtered = &self.filtered_docs;

        let mut action: Option<RowAction> = None;
        let busy = self.import_active || self.maintenance_active;
        Self::work_panel(
            ui,
            "列表",
            "文档列表",
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
                                        action = Some(RowAction::Delete(doc.id));
                                    }
                                });
                            });
                        });
                    });
            },
        );

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
            Some(RowAction::Delete(id)) => self.delete_document(id),
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
                                action = Some(RowAction::Delete(doc.id));
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
        let ctx = ui.ctx().clone();
        match action {
            Some(RowAction::Reimport(path)) => self.start_import(&ctx, vec![path], true),
            Some(RowAction::Delete(id)) => self.delete_document(id),
            _ => {}
        }
    }
}

/// 行内文字按钮(不强调)。
fn link(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).size(12.0).color(RsouApp::blue())).frame(false)
}

fn link_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(link(text)).clicked()
}
