//! 外壳:侧栏、顶栏与主工作区骨架。

use super::*;

impl RsouApp {
    // ---------- 侧栏与顶栏 ----------

    pub(crate) fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            Self::brand_mark(ui);
            ui.add_space(1.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("rsou")
                        .size(19.0)
                        .strong()
                        .color(Self::sidebar_text()),
                );
                ui.label(
                    egui::RichText::new("文档检索工具")
                        .size(13.0)
                        .color(Self::sidebar_muted()),
                );
            });
        });

        ui.add_space(25.0);
        let (rule_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter().hline(
            rule_rect.x_range(),
            rule_rect.center().y,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(228, 239, 250, 28)),
        );
        ui.add_space(24.0);
        ui.label(
            egui::RichText::new("功能")
                .size(13.0)
                .strong()
                .color(Self::sidebar_faint()),
        );
        ui.add_space(13.0);

        for page in Page::ALL {
            self.ui_sidebar_entry(ui, page);
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "rsou {}",
                    option_env!("RSOU_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
                ))
                .size(12.0)
                .color(Self::sidebar_faint()),
            );
        });
    }

    pub(crate) fn brand_mark(ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, Self::blue());
        let left = rect.left() + 10.0;
        let right = rect.right() - 10.0;
        let top = rect.top() + 10.0;
        let bottom = rect.bottom() - 10.0;
        let mid = rect.center().y;
        let mark = Stroke::new(1.6, Color32::WHITE);
        painter.line_segment([egui::pos2(left, top), egui::pos2(right, top)], mark);
        painter.line_segment([egui::pos2(left, bottom), egui::pos2(right, bottom)], mark);
        painter.line_segment(
            [egui::pos2(left, top), egui::pos2(rect.center().x, mid)],
            mark,
        );
        painter.line_segment(
            [egui::pos2(right, top), egui::pos2(rect.center().x, mid)],
            mark,
        );
        painter.line_segment(
            [egui::pos2(rect.center().x, mid), egui::pos2(left, bottom)],
            mark,
        );
        painter.line_segment(
            [egui::pos2(rect.center().x, mid), egui::pos2(right, bottom)],
            mark,
        );
    }

    /// 侧栏条目:当前页高亮;左侧圆点表示该页「有在途任务」。
    pub(crate) fn ui_sidebar_entry(&mut self, ui: &mut egui::Ui, page: Page) {
        let active = self.page == page;
        let inflight = self.page_inflight(page);

        let frame = if active {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(8, 10))
                .fill(Self::navy_2())
                .stroke(Stroke::new(
                    1.0,
                    Color32::from_rgba_unmultiplied(116, 170, 226, 82),
                ))
                .corner_radius(CornerRadius::ZERO)
        } else {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 10))
        };
        let inner = frame.show(ui, |ui| {
            ui.set_min_height(39.0);
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                if inflight {
                    // 有在途任务:实心点
                    ui.painter().circle_filled(rect.center(), 5.0, Self::teal());
                } else if active {
                    ui.painter().circle_filled(rect.center(), 7.5, Self::navy());
                    ui.painter().circle_stroke(
                        rect.center(),
                        7.5,
                        Stroke::new(3.5, Color32::from_rgb(121, 169, 229)),
                    );
                } else {
                    ui.painter().circle_stroke(
                        rect.center(),
                        7.5,
                        Stroke::new(1.0, Color32::from_rgb(109, 145, 179)),
                    );
                }
                ui.vertical(|ui| {
                    let hint_color = if active {
                        Color32::from_rgb(184, 204, 227)
                    } else {
                        Self::sidebar_muted()
                    };
                    ui.label(
                        egui::RichText::new(page.title())
                            .size(15.0)
                            .strong()
                            .color(Self::sidebar_text()),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new(page.hint())
                            .size(13.0)
                            .color(hint_color),
                    );
                });
            });
        });
        let response = ui.interact(
            inner.response.rect,
            ui.id().with(("sidebar-page", page.title())),
            egui::Sense::click(),
        );
        if response.clicked() {
            self.page = page;
        }
    }

    /// 顶栏:面包屑 + 右侧状态徽标(索引文档数 / 任务进行中 / 索引不可用)。
    pub(crate) fn ui_topbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("rsou").strong().color(Self::ink()));
            ui.label(egui::RichText::new("/").color(Self::soft()));
            ui.label(egui::RichText::new(self.page.title()).color(Self::muted()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(error) = &self.db_error {
                    ui.label(
                        egui::RichText::new(format!("索引不可用: {error}"))
                            .size(13.0)
                            .color(Self::amber()),
                    );
                } else if let Some(count) = self.doc_count {
                    Self::status_badge(ui, &format!("{count} 篇文档"));
                }
                if self.has_inflight() {
                    Self::status_badge(ui, "任务进行中");
                }
            });
        });
    }

    pub(crate) fn ui_workspace(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(self.page.title())
                .size(30.0)
                .strong()
                .color(Self::ink()),
        );
        ui.add_space(5.0);
        ui.label(
            egui::RichText::new(self.page.description())
                .size(15.0)
                .color(Self::muted()),
        );
        ui.add_space(18.0);

        match self.page {
            Page::Library => self.ui_page_library(ui),
            Page::Search => self.ui_page_search(ui),
            Page::Settings => self.ui_page_settings(ui),
        }
    }
}
