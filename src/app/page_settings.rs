//! 设置页:数据位置、索引统计、索引维护与解析选项/关于。

use rsou_lib::filebrowser;
use rsou_lib::repo;

use super::*;

impl RsouApp {
    pub(crate) fn ui_page_settings(&mut self, ui: &mut egui::Ui) {
        self.ui_settings_location(ui);
        ui.add_space(13.0);
        self.ui_settings_stats(ui);
        ui.add_space(13.0);
        self.ui_settings_maintenance(ui);
        ui.add_space(13.0);
        self.ui_settings_parse(ui);
    }

    /// 卡片 1:数据位置(路径展示 + 打开/复制)。
    fn ui_settings_location(&mut self, ui: &mut egui::Ui) {
        // 按钮动作在面板闭包外落地,避免借用冲突。
        enum LocationAction {
            OpenDataDir,
            OpenLog,
            CopyPath,
        }
        let mut action = None;
        Self::work_panel(
            ui,
            "位置",
            "数据与日志",
            "索引文件、临时目录与启动日志的位置",
            None,
            |ui| {
                Self::path_row(ui, "数据目录", &self.dirs.data_dir);
                Self::path_row(ui, "索引文件", &self.dirs.db_path);
                Self::path_row(ui, "临时目录", &self.dirs.tmp_dir);
                let log_text = self
                    .startup_log_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "不可用(启动日志未创建)".to_owned());
                Self::text_row(ui, "启动日志", &log_text);
                if let Some(error) = &self.db_error {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("索引库状态: {error}"))
                            .size(13.0)
                            .color(Self::amber()),
                    );
                } else if self.db.is_some() {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("索引库状态: 正常")
                            .size(13.0)
                            .color(Self::teal()),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if link_button(ui, "打开数据目录") {
                        action = Some(LocationAction::OpenDataDir);
                    }
                    if link_button(ui, "打开日志") {
                        action = Some(LocationAction::OpenLog);
                    }
                    if link_button(ui, "复制路径") {
                        action = Some(LocationAction::CopyPath);
                    }
                });
                if let Some(notice) = &self.settings_notice {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(notice).size(12.0).color(Self::muted()));
                }
            },
        );
        match action {
            Some(LocationAction::OpenDataDir) => {
                self.settings_notice = platform::open_path(&self.dirs.data_dir).err();
            }
            Some(LocationAction::OpenLog) => {
                self.settings_notice = match &self.startup_log_path {
                    Some(path) if path.exists() => platform::open_path(path).err(),
                    _ => Some("启动日志文件不存在".to_owned()),
                };
            }
            Some(LocationAction::CopyPath) => {
                ui.ctx().copy_text(self.dirs.data_dir.display().to_string());
                self.settings_notice = Some("已复制数据目录路径".to_owned());
            }
            None => {}
        }
    }

    /// 卡片 2:索引统计(缓存值 + 手动刷新)。
    fn ui_settings_stats(&mut self, ui: &mut egui::Ui) {
        let mut want_refresh = false;
        Self::work_panel(
            ui,
            "统计",
            "索引统计",
            "文档、分块与索引文件体量",
            None,
            |ui| {
                if let Some(stats) = &self.index_stats {
                    Self::text_row(
                        ui,
                        "文档",
                        &format!(
                            "{} 篇(已索引 {} · 失败 {})",
                            stats.documents, stats.parsed, stats.failed
                        ),
                    );
                    Self::text_row(ui, "分块", &format!("{} 个", stats.chunks));
                    Self::text_row(
                        ui,
                        "FTS 行数",
                        &format!("{} 行(每篇文档一行)", stats.fts_rows),
                    );
                    Self::text_row(
                        ui,
                        "原文字节",
                        &filebrowser::format_size(stats.text_bytes.max(0) as u64),
                    );
                    Self::text_row(ui, "索引文件", &filebrowser::format_size(stats.db_bytes));
                } else {
                    ui.label(
                        egui::RichText::new("暂无统计数据(索引库不可用或未读取)。")
                            .size(14.0)
                            .color(Self::muted()),
                    );
                }
                ui.add_space(8.0);
                if Self::secondary_button(ui, "刷新", 88.0)
                    .on_hover_text("重新读取索引统计")
                    .clicked()
                {
                    want_refresh = true;
                }
            },
        );
        if want_refresh {
            self.refresh_index_stats();
        }
    }

    /// 卡片 3:索引维护(四个操作 + 重建进度 + 二次确认清空)。
    fn ui_settings_maintenance(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<MaintainKind> = None;
        let mut clear_confirm = false;
        let mut clear_cancel = false;
        let busy = self.maintenance_active || self.import_active;
        Self::work_panel(
            ui,
            "维护",
            "索引维护",
            "检查一致性,必要时重建或优化全文索引",
            if self.maintenance_active {
                Some("维护中")
            } else {
                None
            },
            |ui| {
                ui.label(
                    egui::RichText::new(
                        "索引文件只应由本程序打开(内置自定义分词器)。\
                         维护与导入互斥:任一进行中另一组按钮都会禁用。",
                    )
                    .size(13.0)
                    .color(Self::muted()),
                );
                ui.add_space(8.0);
                // 维护与导入互斥:任一方进行中,维护按钮整体置灰。
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if Self::secondary_button(ui, "完整性检查", 110.0)
                            .on_hover_text("检查 SQLite 与全文索引的一致性")
                            .clicked()
                        {
                            action = Some(MaintainKind::Check);
                        }
                        if Self::secondary_button(ui, "重建全文索引", 120.0)
                            .on_hover_text("按文档与原文全文重写 FTS 表")
                            .clicked()
                        {
                            action = Some(MaintainKind::Rebuild);
                        }
                        if Self::secondary_button(ui, "优化", 70.0)
                            .on_hover_text("FTS optimize + WAL 截断 + VACUUM")
                            .clicked()
                        {
                            action = Some(MaintainKind::Optimize);
                        }
                        if self.confirm_clear {
                            if Self::secondary_button(ui, "确认清空(不可恢复)", 160.0).clicked()
                            {
                                clear_confirm = true;
                            }
                            if Self::secondary_button(ui, "取消", 70.0).clicked() {
                                clear_cancel = true;
                            }
                        } else if Self::secondary_button(ui, "清空资料库", 110.0)
                            .on_hover_text("删除全部文档与索引(保留设置)")
                            .clicked()
                        {
                            action = Some(MaintainKind::Clear);
                        }
                    });
                });
                // 重建进度条(维护线程写原子量,这里只读)
                if self.maintenance_active
                    && let Some(progress) = &self.maintain_progress
                {
                    let done = progress.done.load(Ordering::Relaxed);
                    let total = progress.total.load(Ordering::Relaxed);
                    if total > 0 {
                        ui.add_space(6.0);
                        ui.add(
                            egui::ProgressBar::new(done as f32 / total as f32)
                                .text(format!("重建中 {done}/{total}")),
                        );
                    }
                }
                if let Some(result) = &self.maintain_result {
                    ui.add_space(6.0);
                    let (text, ok) = match result {
                        Ok(message) => (message.as_str(), true),
                        Err(message) => (message.as_str(), false),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(text).size(13.0).color(if ok {
                            Self::teal()
                        } else {
                            Self::amber()
                        }));
                        if self.maintain_inconsistent {
                            ui.add_enabled_ui(!busy, |ui| {
                                if Self::secondary_button(ui, "立即重建", 90.0).clicked() {
                                    action = Some(MaintainKind::Rebuild);
                                }
                            });
                        }
                    });
                }
            },
        );
        if clear_confirm {
            self.confirm_clear = false;
            self.start_maintain(ui.ctx(), MaintainKind::Clear);
        } else if clear_cancel {
            self.confirm_clear = false;
        } else {
            match action {
                // 「清空资料库」先进入二次确认,不直接跑。
                Some(MaintainKind::Clear) => self.confirm_clear = true,
                Some(kind) => self.start_maintain(ui.ctx(), kind),
                None => {}
            }
        }
    }

    /// 卡片 4:解析选项 + 关于。
    fn ui_settings_parse(&mut self, ui: &mut egui::Ui) {
        let mut new_max_mb = None;
        Self::work_panel(
            ui,
            "选项",
            "解析选项",
            "导入时的文件体积上限",
            None,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("单文件最大体积")
                            .size(14.0)
                            .strong()
                            .color(Self::ink()),
                    );
                    let mut mb = self.max_file_mb;
                    if ui
                        .add(
                            egui::DragValue::new(&mut mb)
                                .range(1..=repo::MAX_FILE_MB_LIMIT)
                                .suffix(" MB"),
                        )
                        .changed()
                    {
                        new_max_mb = Some(mb);
                    }
                    ui.label(
                        egui::RichText::new("超过上限的文件会记为导入失败(TOO_LARGE)")
                            .size(12.0)
                            .color(Self::muted()),
                    );
                });
            },
        );
        if let Some(mb) = new_max_mb {
            self.set_max_file_mb(mb);
        }

        ui.add_space(13.0);
        Self::work_panel(ui, "关于", "rsou", "版本与路径", None, |ui| {
            Self::text_row(ui, "版本", env!("CARGO_PKG_VERSION"));
            Self::path_row(ui, "数据目录", &self.dirs.data_dir);
            let log_text = self
                .startup_log_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "不可用".to_owned());
            Self::text_row(ui, "启动日志", &log_text);
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("索引文件只应由本程序打开(内置自定义分词器)。")
                    .size(12.0)
                    .color(Self::muted()),
            );
        });
    }

    fn path_row(ui: &mut egui::Ui, label: &str, path: &std::path::Path) {
        Self::text_row(ui, label, &path.display().to_string());
    }

    fn text_row(ui: &mut egui::Ui, label: &str, value: &str) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .size(14.0)
                    .strong()
                    .color(Self::ink()),
            );
            ui.label(egui::RichText::new(value).size(14.0).color(Self::muted()));
        });
        ui.add_space(4.0);
    }
}

/// 行内文字按钮(与资料库/检索页同款)。
fn link_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.0).color(RsouApp::blue())).frame(false),
    )
    .clicked()
}
