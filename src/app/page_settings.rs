//! 设置页:数据位置、索引统计、索引维护与解析选项/关于。

use rsou_lib::filebrowser;
use rsou_lib::repo;

use super::theme::Icon;
use super::*;

/// 两列卡片的最小宽度:低于它时卡片改为上下堆叠(窄窗/高 DPI)。
const TWO_COL_MIN_WIDTH: f32 = 780.0;

impl RsouApp {
    pub(crate) fn ui_page_settings(&mut self, ui: &mut egui::Ui) {
        Self::page_header(ui, self.page.title(), self.page.description());
        ui.add_space(Self::HEADER_TO_CONTENT);

        // 第一行:数据位置 | 索引统计(窄窗时上下堆叠)。
        if ui.available_width() >= TWO_COL_MIN_WIDTH {
            let col = (ui.available_width() - Self::SECTION_GAP) / 2.0;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(col, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.ui_settings_location(ui),
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(col, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.ui_settings_stats(ui),
                );
            });
        } else {
            self.ui_settings_location(ui);
            ui.add_space(Self::SECTION_GAP);
            self.ui_settings_stats(ui);
        }
        ui.add_space(Self::SECTION_GAP);

        // 第二行:索引维护(操作多,占整行)。
        self.ui_settings_maintenance(ui);
        ui.add_space(Self::SECTION_GAP);

        // 第三行:解析选项 | 关于。
        if ui.available_width() >= TWO_COL_MIN_WIDTH {
            let col = (ui.available_width() - Self::SECTION_GAP) / 2.0;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(col, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.ui_settings_parse(ui),
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(col, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.ui_settings_about(ui),
                );
            });
        } else {
            self.ui_settings_parse(ui);
            ui.add_space(Self::SECTION_GAP);
            self.ui_settings_about(ui);
        }
    }

    /// 卡片 1:数据位置(路径展示 + 打开/复制)。
    fn ui_settings_location(&mut self, ui: &mut egui::Ui) {
        // 按钮动作在卡片闭包外落地,避免借用冲突。
        enum LocationAction {
            OpenDataDir,
            OpenLog,
            CopyPath,
        }
        let mut action = None;
        Self::card(ui, |ui| {
            Self::card_title(ui, "数据位置");
            Self::thin_divider(ui);
            Self::field_row(ui, "数据目录", &self.dirs.data_dir.display().to_string());
            Self::field_row(ui, "索引文件", &self.dirs.db_path.display().to_string());
            Self::field_row(ui, "临时目录", &self.dirs.tmp_dir.display().to_string());
            let log_text = self
                .startup_log_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "不可用(启动日志未创建)".to_owned());
            Self::field_row(ui, "启动日志", &log_text);
            if let Some(error) = &self.db_error {
                ui.add_space(6.0);
                Self::status_badge(
                    ui,
                    Some(Icon::Warn),
                    &format!("索引库状态: {error}"),
                    Self::danger_soft(),
                    Self::danger(),
                );
            } else if self.db.is_some() {
                ui.add_space(6.0);
                Self::status_badge(
                    ui,
                    Some(Icon::CheckCircle),
                    "索引库状态: 正常",
                    Self::success_soft(),
                    Self::success(),
                );
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if Self::small_secondary_button(ui, Some(Icon::External), "打开数据目录", 0.0, true)
                    .clicked()
                {
                    action = Some(LocationAction::OpenDataDir);
                }
                if Self::small_secondary_button(ui, Some(Icon::External), "打开日志", 0.0, true)
                    .clicked()
                {
                    action = Some(LocationAction::OpenLog);
                }
                if Self::small_secondary_button(ui, Some(Icon::Copy), "复制路径", 0.0, true)
                    .clicked()
                {
                    action = Some(LocationAction::CopyPath);
                }
            });
            if let Some(notice) = &self.settings_notice {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(notice)
                        .size(12.0)
                        .color(Self::text_secondary()),
                );
            }
        });
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
        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                Self::card_title(ui, "索引统计");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Self::small_secondary_button(ui, Some(Icon::Refresh), "刷新", 0.0, true)
                        .on_hover_text("重新读取索引统计")
                        .clicked()
                    {
                        want_refresh = true;
                    }
                });
            });
            Self::thin_divider(ui);
            if let Some(stats) = &self.index_stats {
                Self::field_row(
                    ui,
                    "文档",
                    &format!(
                        "{} 篇(已索引 {} · 失败 {})",
                        stats.documents, stats.parsed, stats.failed
                    ),
                );
                Self::field_row(ui, "分块", &format!("{} 个", stats.chunks));
                Self::field_row(
                    ui,
                    "FTS 行数",
                    &format!("{} 行(每篇文档一行)", stats.fts_rows),
                );
                Self::field_row(
                    ui,
                    "原文字节",
                    &filebrowser::format_size(stats.text_bytes.max(0) as u64),
                );
                Self::field_row(ui, "索引文件", &filebrowser::format_size(stats.db_bytes));
            } else {
                Self::empty_note(ui, "暂无统计数据(索引库不可用或未读取)。");
            }
        });
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
        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                Self::card_title(ui, "索引维护");
                if self.maintenance_active {
                    Self::status_badge(
                        ui,
                        Some(Icon::Loader),
                        "维护中",
                        Self::accent_soft(),
                        Self::accent(),
                    );
                }
            });
            Self::thin_divider(ui);
            ui.label(
                egui::RichText::new(
                    "索引文件只应由本程序打开(内置自定义分词器)。\
                     维护与导入互斥:任一进行中另一组按钮都会禁用。",
                )
                .size(13.0)
                .color(Self::text_secondary()),
            );
            ui.add_space(10.0);
            // 维护与导入互斥:任一方进行中,维护按钮整体置灰。
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if Self::small_secondary_button(ui, Some(Icon::Shield), "完整性检查", 0.0, true)
                        .on_hover_text("检查 SQLite 与全文索引的一致性")
                        .clicked()
                    {
                        action = Some(MaintainKind::Check);
                    }
                    if Self::small_secondary_button(
                        ui,
                        Some(Icon::Refresh),
                        "重建全文索引",
                        0.0,
                        true,
                    )
                    .on_hover_text("按文档与原文全文重写 FTS 表")
                    .clicked()
                    {
                        action = Some(MaintainKind::Rebuild);
                    }
                    if Self::small_secondary_button(ui, Some(Icon::Database), "优化", 0.0, true)
                        .on_hover_text("FTS optimize + WAL 截断 + VACUUM")
                        .clicked()
                    {
                        action = Some(MaintainKind::Optimize);
                    }
                    if self.confirm_clear {
                        if Self::small_danger_outline_button(
                            ui,
                            Some(Icon::Trash),
                            "确认清空(不可恢复)",
                            0.0,
                            true,
                        )
                        .clicked()
                        {
                            clear_confirm = true;
                        }
                        if Self::small_secondary_button(ui, None, "取消", 0.0, true).clicked() {
                            clear_cancel = true;
                        }
                    } else if Self::small_danger_outline_button(
                        ui,
                        Some(Icon::Trash),
                        "清空资料库",
                        0.0,
                        true,
                    )
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
                    ui.add_space(10.0);
                    Self::progress_bar(
                        ui,
                        done as f32 / total as f32,
                        (ui.available_width() - 110.0).max(80.0),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("重建中 {done}/{total}"))
                            .size(12.0)
                            .color(Self::text_secondary()),
                    );
                }
            }
            if let Some(result) = &self.maintain_result {
                ui.add_space(8.0);
                let (text, ok) = match result {
                    Ok(message) => (message.as_str(), true),
                    Err(message) => (message.as_str(), false),
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(text).size(13.0).color(if ok {
                        Self::success()
                    } else {
                        Self::danger()
                    }));
                    if self.maintain_inconsistent {
                        ui.add_enabled_ui(!busy, |ui| {
                            if Self::small_secondary_button(ui, None, "立即重建", 0.0, true)
                                .clicked()
                            {
                                action = Some(MaintainKind::Rebuild);
                            }
                        });
                    }
                });
            }
        });
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

    /// 卡片 4:解析选项。
    fn ui_settings_parse(&mut self, ui: &mut egui::Ui) {
        let mut new_max_mb = None;
        Self::card(ui, |ui| {
            Self::card_title(ui, "解析选项");
            Self::thin_divider(ui);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("单文件最大体积")
                        .size(13.0)
                        .color(Self::text_secondary()),
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
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("超过上限的文件会记为导入失败(TOO_LARGE)")
                    .size(12.0)
                    .color(Self::text_muted()),
            );
        });
        if let Some(mb) = new_max_mb {
            self.set_max_file_mb(mb);
        }
    }

    /// 卡片 5:关于。
    fn ui_settings_about(&mut self, ui: &mut egui::Ui) {
        Self::card(ui, |ui| {
            Self::card_title(ui, "关于");
            Self::thin_divider(ui);
            Self::field_row(ui, "版本", env!("CARGO_PKG_VERSION"));
            Self::field_row(ui, "数据目录", &self.dirs.data_dir.display().to_string());
            let log_text = self
                .startup_log_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "不可用".to_owned());
            Self::field_row(ui, "启动日志", &log_text);
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("索引文件只应由本程序打开(内置自定义分词器)。")
                    .size(12.0)
                    .color(Self::text_muted()),
            );
        });
    }

    /// 字段行:88px 标签列 + 值(长路径自动换行,不撑破卡片)。
    fn field_row(ui: &mut egui::Ui, label: &str, value: &str) {
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(88.0, 18.0), egui::Sense::hover());
            ui.painter().text(
                egui::pos2(rect.left(), rect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(13.0),
                Self::text_secondary(),
            );
            ui.add(egui::Label::new(
                egui::RichText::new(value)
                    .size(13.0)
                    .color(Self::text_primary()),
            ));
        });
        ui.add_space(6.0);
    }
}
