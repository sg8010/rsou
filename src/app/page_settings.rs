//! 设置页:数据目录与索引信息(本阶段展示真实路径;维护动作阶段 4 接入)。

use super::*;

impl RsouApp {
    pub(crate) fn ui_page_settings(&mut self, ui: &mut egui::Ui) {
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
            },
        );
        ui.add_space(13.0);
        Self::work_panel(
            ui,
            "维护",
            "索引维护",
            "重建 / 优化 / 完整性检查将在阶段 4 接入",
            None,
            |ui| {
                ui.label(
                    egui::RichText::new(
                        "索引重建、optimize 与完整性检查将在阶段 4 接入;\
                         索引文件只应由本程序(或注册了同名 tokenizer 的连接)打开。",
                    )
                    .size(15.0)
                    .color(Self::muted()),
                );
            },
        );
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
