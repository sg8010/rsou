//! 资料库页:文档导入与管理(本阶段为占位卡片 + 文件选择入口)。

use super::*;

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

        let mut pick = false;
        Self::work_panel(
            ui,
            "文档",
            "导入文档",
            "解析与索引进度将在阶段 2 接入",
            None,
            |ui| {
                ui.label(
                    egui::RichText::new(
                        "导入、去重、失败清单与文档表格将在阶段 2 接入;\
                         当前可先用「添加文件」验证文件对话框。",
                    )
                    .size(15.0)
                    .color(Self::muted()),
                );
                ui.add_space(10.0);
                if Self::primary_button(ui, "添加文件", 120.0, self.db.is_some()).clicked() {
                    pick = true;
                }
                if let Some(notice) = &self.library_notice {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(notice).size(13.0).color(Self::teal()));
                }
            },
        );
        if pick {
            self.pending_dialog = Some(DialogRequest::ImportFiles);
        }
    }
}
