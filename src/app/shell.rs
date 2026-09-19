//! 外壳:侧栏、顶栏与主工作区骨架。

use super::theme::Icon;
use super::*;

/// 侧栏内边距(左/右/上/下)。
pub(crate) const SIDEBAR_MARGIN: egui::Margin = egui::Margin {
    left: 10,
    right: 10,
    top: 20,
    bottom: 14,
};

/// 顶栏内边距(左/右;上下为 0,内容在 44px 里垂直居中)。
pub(crate) const TOPBAR_MARGIN: egui::Margin = egui::Margin {
    left: 24,
    right: 24,
    top: 0,
    bottom: 0,
};

impl RsouApp {
    // ---------- 侧栏与顶栏 ----------

    pub(crate) fn ui_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(ui.available_width());
        // 品牌区:rsou + 副标题(与导航图标左对齐,右移 12px)。
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("rsou")
                        .size(25.0)
                        .strong()
                        .color(Self::accent()),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("文档检索工具")
                        .size(12.0)
                        .color(Self::text_secondary()),
                );
            });
        });

        ui.add_space(22.0);
        for page in Page::ALL {
            self.ui_sidebar_entry(ui, page);
            ui.add_space(4.0);
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(format!(
                    "v{}",
                    option_env!("RSOU_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
                ))
                .size(12.0)
                .color(Self::text_muted()),
            );
        });
    }

    /// 侧栏右边的 1px 分隔线(画满整个侧栏高度)。
    ///
    /// 面板 Frame 的 stroke 只能四边同画,单边分隔线得手绘;
    /// `max_rect + margin` 还原出面板完整矩形。
    pub(crate) fn paint_sidebar_border(ui: &mut egui::Ui) {
        let panel = ui.max_rect() + SIDEBAR_MARGIN;
        ui.painter().with_clip_rect(panel).vline(
            panel.right() - 0.5,
            panel.y_range(),
            Stroke::new(1.0, Self::border()),
        );
    }

    /// 侧栏导航项:图标 + 文字;选中 = 浅蓝底蓝字;在途任务 = 右侧 6px 小圆点。
    pub(crate) fn ui_sidebar_entry(&mut self, ui: &mut egui::Ui, page: Page) {
        let active = self.page == page;
        let inflight = self.page_inflight(page);
        let icon = match page {
            Page::Library => Icon::Folder,
            Page::Search => Icon::Search,
            Page::Settings => Icon::Gear,
        };

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), Self::NAV_ITEM_HEIGHT),
            egui::Sense::click(),
        );
        if ui.is_rect_visible(rect) {
            let (fill, icon_color, text_color) = if active {
                (Self::accent_soft(), Self::accent(), Self::accent())
            } else if response.hovered() {
                (
                    Self::nav_hover(),
                    Self::text_secondary(),
                    Self::text_primary(),
                )
            } else {
                (
                    Color32::TRANSPARENT,
                    Self::text_secondary(),
                    Self::text_primary(),
                )
            };
            if fill != Color32::TRANSPARENT {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(Self::NAV_RADIUS), fill);
            }
            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 12.0 + 9.0, rect.center().y),
                egui::vec2(18.0, 18.0),
            );
            Self::paint_icon(ui.painter(), icon, icon_rect, icon_color);
            ui.painter().text(
                egui::pos2(icon_rect.right() + 10.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                page.title(),
                egui::FontId::proportional(14.0),
                text_color,
            );
            if inflight {
                ui.painter().circle_filled(
                    egui::pos2(rect.right() - 14.0, rect.center().y),
                    3.0,
                    Self::accent(),
                );
            }
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            self.page = page;
        }
    }

    /// 顶栏左侧的品牌方块(蓝色小方块 + 白色放大镜,呼应「检索」)。
    pub(crate) fn brand_mark(ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(5), Self::accent());
        let mark = Stroke::new(1.5, Color32::WHITE);
        let lens = egui::pos2(rect.left() + 9.0, rect.top() + 9.0);
        painter.circle_stroke(lens, 4.0, mark);
        painter.line_segment(
            [
                egui::pos2(lens.x + 3.0, lens.y + 3.0),
                egui::pos2(rect.right() - 5.0, rect.bottom() - 5.0),
            ],
            mark,
        );
    }

    /// 顶栏:品牌块 + 面包屑;右侧状态徽标(索引不可用 / 导入中 / 文档数)。
    pub(crate) fn ui_topbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            Self::brand_mark(ui);
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("rsou")
                    .size(13.0)
                    .color(Self::text_primary()),
            );
            ui.label(
                egui::RichText::new("/")
                    .size(13.0)
                    .color(Self::text_muted()),
            );
            ui.label(
                egui::RichText::new(self.page.title())
                    .size(13.0)
                    .color(Self::text_secondary()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(error) = &self.db_error {
                    Self::status_badge(
                        ui,
                        Some(Icon::Warn),
                        &format!("索引不可用: {error}"),
                        Self::warning_soft(),
                        Self::warning(),
                    );
                } else if self.import_active {
                    let c = &self.import_progress.counts;
                    Self::status_badge(
                        ui,
                        Some(Icon::Loader),
                        &format!("导入中 {}/{}", c.processed, c.total),
                        Self::accent_soft(),
                        Self::accent(),
                    );
                } else {
                    Self::status_badge(
                        ui,
                        Some(Icon::Doc),
                        &format!("文档 {} 篇", self.documents.len()),
                        Self::accent_soft(),
                        Self::accent(),
                    );
                }
            });
        });
        // 顶栏底部分隔线(通栏整宽)。
        let panel = ui.max_rect() + TOPBAR_MARGIN;
        ui.painter().with_clip_rect(panel).hline(
            panel.x_range(),
            panel.bottom() - 0.5,
            Stroke::new(1.0, Self::border()),
        );
    }

    /// 主工作区:各页面自己渲染页头(标题/说明/右侧操作)。
    pub(crate) fn ui_workspace(&mut self, ui: &mut egui::Ui) {
        match self.page {
            Page::Library => self.ui_page_library(ui),
            Page::Search => self.ui_page_search(ui),
            Page::Settings => self.ui_page_settings(ui),
        }
    }
}
