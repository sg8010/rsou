//! 检索页:全文搜索与预览(本阶段为占位卡片)。

use super::*;

impl RsouApp {
    pub(crate) fn ui_page_search(&mut self, ui: &mut egui::Ui) {
        Self::work_panel(
            ui,
            "检索",
            "全文检索",
            "查询、过滤与高亮将在阶段 3 接入",
            None,
            |ui| {
                ui.label(
                    egui::RichText::new(
                        "关键词查询、字段限定、过滤与命中高亮将在阶段 3 接入;\
                         中文短语与两字词由自建逐字 tokenizer 支撑。",
                    )
                    .size(15.0)
                    .color(Self::muted()),
                );
            },
        );
    }
}
