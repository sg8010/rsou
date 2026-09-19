//! 视觉主题:design tokens(色板/圆角/间距/尺寸)、通用控件与全局样式。
//!
//! 统一视觉规范:浅灰工作区 + 白色内容卡片 + 蓝色主色,小圆角、低对比边框、
//! 极弱阴影、高信息密度。页面代码只使用这里的 token 与组件,不另造颜色/尺寸。

use super::*;

/// 简单线条图标(16~18px、约 1.5px stroke;颜色跟随所在控件文本色)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    Folder,
    FolderPlus,
    Doc,
    DocPlus,
    Search,
    Gear,
    External,
    Refresh,
    Trash,
    Shield,
    Database,
    Copy,
    Warn,
    CheckCircle,
    Loader,
}

/// 按钮三态配色。
#[derive(Clone, Copy)]
struct BtnLook {
    fill: Color32,
    stroke: Stroke,
    text: Color32,
}

impl RsouApp {
    // ---------- Design tokens:颜色 ----------

    pub(crate) fn canvas() -> Color32 {
        Color32::from_rgb(0xF5, 0xF7, 0xFA)
    }

    pub(crate) fn surface() -> Color32 {
        Color32::from_rgb(0xFF, 0xFF, 0xFF)
    }

    pub(crate) fn surface_subtle() -> Color32 {
        Color32::from_rgb(0xFA, 0xFB, 0xFC)
    }

    pub(crate) fn border() -> Color32 {
        Color32::from_rgb(0xE3, 0xE8, 0xEF)
    }

    pub(crate) fn border_strong() -> Color32 {
        Color32::from_rgb(0xD0, 0xD7, 0xE2)
    }

    pub(crate) fn divider() -> Color32 {
        Color32::from_rgb(0xE9, 0xED, 0xF3)
    }

    pub(crate) fn text_primary() -> Color32 {
        Color32::from_rgb(0x17, 0x20, 0x33)
    }

    pub(crate) fn text_secondary() -> Color32 {
        Color32::from_rgb(0x66, 0x70, 0x85)
    }

    pub(crate) fn text_muted() -> Color32 {
        Color32::from_rgb(0x98, 0xA2, 0xB3)
    }

    pub(crate) fn text_disabled() -> Color32 {
        Color32::from_rgb(0xB8, 0xC0, 0xCC)
    }

    pub(crate) fn accent() -> Color32 {
        Color32::from_rgb(0x16, 0x77, 0xFF)
    }

    pub(crate) fn accent_hover() -> Color32 {
        Color32::from_rgb(0x0F, 0x6D, 0xE8)
    }

    pub(crate) fn accent_soft() -> Color32 {
        Color32::from_rgb(0xEA, 0xF3, 0xFF)
    }

    pub(crate) fn accent_border() -> Color32 {
        Color32::from_rgb(0xBF, 0xD8, 0xFF)
    }

    pub(crate) fn success() -> Color32 {
        Color32::from_rgb(0x16, 0xA3, 0x4A)
    }

    pub(crate) fn success_soft() -> Color32 {
        Color32::from_rgb(0xEA, 0xF8, 0xEF)
    }

    pub(crate) fn warning() -> Color32 {
        Color32::from_rgb(0xD9, 0x77, 0x06)
    }

    pub(crate) fn warning_soft() -> Color32 {
        Color32::from_rgb(0xFF, 0xF6, 0xE8)
    }

    pub(crate) fn danger() -> Color32 {
        Color32::from_rgb(0xE5, 0x48, 0x4D)
    }

    pub(crate) fn danger_soft() -> Color32 {
        Color32::from_rgb(0xFF, 0xF0, 0xF0)
    }

    pub(crate) fn danger_hover() -> Color32 {
        Color32::from_rgb(0xD1, 0x38, 0x3D)
    }

    pub(crate) fn highlight_bg() -> Color32 {
        Color32::from_rgb(0xFF, 0xF0, 0xA6)
    }

    pub(crate) fn highlight_text() -> Color32 {
        Color32::from_rgb(0x6B, 0x52, 0x00)
    }

    pub(crate) fn nav_hover() -> Color32 {
        Color32::from_rgb(0xF3, 0xF7, 0xFC)
    }

    /// 选中的搜索结果卡片底色。
    pub(crate) fn selected_soft() -> Color32 {
        Color32::from_rgb(0xF7, 0xFB, 0xFF)
    }

    /// 禁用控件底色。
    pub(crate) fn disabled_bg() -> Color32 {
        Color32::from_rgb(0xEE, 0xF1, 0xF5)
    }

    /// 进度条轨道色。
    pub(crate) fn track() -> Color32 {
        Color32::from_rgb(0xEA, 0xEE, 0xF4)
    }

    // ---------- Design tokens:尺寸/圆角/间距 ----------

    pub(crate) const CARD_RADIUS: u8 = 8;
    pub(crate) const BUTTON_RADIUS: u8 = 6;
    pub(crate) const INPUT_RADIUS: u8 = 6;
    pub(crate) const NAV_RADIUS: u8 = 6;
    pub(crate) const BADGE_RADIUS: u8 = 10;
    pub(crate) const MODAL_RADIUS: u8 = 8;

    pub(crate) const SECTION_GAP: f32 = 16.0;
    pub(crate) const CARD_PADDING: i8 = 18;
    pub(crate) const TITLE_TO_DESC: f32 = 6.0;
    pub(crate) const HEADER_TO_CONTENT: f32 = 18.0;

    pub(crate) const BUTTON_HEIGHT: f32 = 36.0;
    pub(crate) const TAB_HEIGHT: f32 = 36.0;
    pub(crate) const TABLE_HEADER_HEIGHT: f32 = 34.0;
    pub(crate) const TABLE_ROW_HEIGHT: f32 = 34.0;
    pub(crate) const NAV_ITEM_HEIGHT: f32 = 44.0;
    pub(crate) const SIDEBAR_WIDTH: f32 = 196.0;
    pub(crate) const TOPBAR_HEIGHT: f32 = 44.0;

    // ---------- 框架:页面 Header / 卡片 / 分隔线 ----------

    /// 页面标题 + 说明(每页统一)。
    pub(crate) fn page_header(ui: &mut egui::Ui, title: &str, desc: &str) {
        ui.label(
            egui::RichText::new(title)
                .size(28.0)
                .strong()
                .color(Self::text_primary()),
        );
        ui.add_space(Self::TITLE_TO_DESC);
        ui.label(
            egui::RichText::new(desc)
                .size(14.0)
                .color(Self::text_secondary()),
        );
    }

    /// 页面标题 + 说明,右侧放页面级操作(如资料库的「添加文件/添加文件夹」)。
    pub(crate) fn page_header_actions(
        ui: &mut egui::Ui,
        title: &str,
        desc: &str,
        add_actions: impl FnOnce(&mut egui::Ui),
    ) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .size(28.0)
                        .strong()
                        .color(Self::text_primary()),
                );
                ui.add_space(Self::TITLE_TO_DESC);
                ui.label(
                    egui::RichText::new(desc)
                        .size(14.0)
                        .color(Self::text_secondary()),
                );
            });
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                add_actions,
            );
        });
    }

    /// 白色内容卡片:1px 边框 + 8px 圆角 + 极弱阴影。
    /// 内边距为 0,由调用方决定内部 padding(页签栏等需要顶到卡片边缘)。
    pub(crate) fn card_frame() -> egui::Frame {
        egui::Frame::new()
            .inner_margin(egui::Margin::ZERO)
            .fill(Self::surface())
            .stroke(Stroke::new(1.0, Self::border()))
            .corner_radius(CornerRadius::same(Self::CARD_RADIUS))
            .shadow(Shadow {
                offset: [0, 1],
                blur: 6,
                spread: 0,
                color: Color32::from_black_alpha(10),
            })
    }

    /// 卡片 + 标准 18px 内边距。
    pub(crate) fn card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
        Self::card_frame().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(Self::CARD_PADDING))
                .show(ui, add_contents);
        });
    }

    /// 卡片内标题(16px semibold)+ 与正文的间距。
    pub(crate) fn card_title(ui: &mut egui::Ui, title: &str) {
        ui.label(
            egui::RichText::new(title)
                .size(16.0)
                .strong()
                .color(Self::text_primary()),
        );
    }

    /// 卡片标题下的一条细分隔线(跨当前可用宽度)。
    pub(crate) fn thin_divider(ui: &mut egui::Ui) {
        ui.add_space(10.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter().hline(
            rect.x_range(),
            rect.center().y,
            Stroke::new(1.0, Self::divider()),
        );
        ui.add_space(10.0);
    }

    /// 页签卡片:页签栏(左右 18px 内边距)+ 全宽分隔线 + 内容(18px 内边距)。
    ///
    /// 页签与内容必须在同一张卡片里,否则两层内边距会把页签和内容隔开近
    /// 四十像素,看着像两张无关的卡片。下划线页签的分隔线要顶到卡片两侧,
    /// 所以页签栏不带底部内边距、分隔线用卡片的完整宽度手绘。
    pub(crate) fn tabbed_card(
        ui: &mut egui::Ui,
        tab_bar: impl FnOnce(&mut egui::Ui),
        add_contents: impl FnOnce(&mut egui::Ui),
    ) {
        Self::card_frame().show(ui, |ui| {
            let card_width = ui.max_rect().width();
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: Self::CARD_PADDING,
                    right: Self::CARD_PADDING,
                    top: 8,
                    bottom: 0,
                })
                .show(ui, tab_bar);
            // 全宽分隔线:页签栏与内容之间的“下划线”。
            let y = ui.cursor().top();
            let left = ui.cursor().left() - Self::CARD_PADDING as f32;
            ui.painter().hline(
                left..=(left + card_width),
                y,
                Stroke::new(1.0, Self::divider()),
            );
            ui.add_space(1.0);
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: Self::CARD_PADDING,
                    right: Self::CARD_PADDING,
                    top: 14,
                    bottom: Self::CARD_PADDING,
                })
                .show(ui, add_contents);
        });
    }

    /// 下划线式页签按钮:选中 = 蓝色文字 + 底部 2px 蓝色下划线。
    pub(crate) fn tab_button(ui: &mut egui::Ui, text: &str, selected: bool) -> bool {
        let galley = ui.painter().layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(14.0),
            Self::text_secondary(),
        );
        let width = galley.size().x + 24.0;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, Self::TAB_HEIGHT), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let color = if selected {
                Self::accent()
            } else if response.hovered() {
                Self::text_primary()
            } else {
                Self::text_secondary()
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(14.0),
                color,
            );
            if selected {
                let line = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 2.0, rect.bottom() - 2.0),
                    egui::vec2(rect.width() - 4.0, 2.0),
                );
                ui.painter()
                    .rect_filled(line, CornerRadius::ZERO, Self::accent());
            }
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response.clicked()
    }

    // ---------- 按钮 ----------

    /// 自绘按钮:egui 的 `Button::fill` 会盖掉 hover 态,要 hover/禁用各自
    /// 一套颜色只能自己量尺寸、自己画。
    fn button(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        min_width: f32,
        height: f32,
        enabled: bool,
        idle: BtnLook,
        hover: BtnLook,
        disabled: BtnLook,
    ) -> egui::Response {
        let enabled = enabled && ui.is_enabled();
        let font = egui::FontId::proportional(14.0);
        let icon_slot = if icon.is_some() { 24.0 } else { 0.0 };
        let text_w = ui
            .painter()
            .layout_no_wrap(text.to_owned(), font.clone(), idle.text)
            .size()
            .x;
        let width = min_width.max(text_w + icon_slot + 24.0);
        let sense = if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
        let look = if !enabled {
            disabled
        } else if response.hovered() {
            hover
        } else {
            idle
        };
        if ui.is_rect_visible(rect) {
            ui.painter().rect(
                rect,
                CornerRadius::same(Self::BUTTON_RADIUS),
                look.fill,
                look.stroke,
                egui::StrokeKind::Inside,
            );
            let galley = ui
                .painter()
                .layout_no_wrap(text.to_owned(), font, look.text);
            let total = icon_slot + galley.size().x;
            let mut x = rect.center().x - total / 2.0;
            if let Some(icon) = icon {
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(x + 9.0, rect.center().y),
                    egui::vec2(18.0, 18.0),
                );
                Self::paint_icon(ui.painter(), icon, icon_rect, look.text);
                x += icon_slot;
            }
            ui.painter().galley(
                egui::pos2(x, rect.center().y - galley.size().y / 2.0),
                galley,
                look.text,
            );
        }
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response
    }

    /// 主按钮:蓝底白字。
    pub(crate) fn primary_button(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        min_width: f32,
        enabled: bool,
    ) -> egui::Response {
        Self::button(
            ui,
            icon,
            text,
            min_width,
            Self::BUTTON_HEIGHT,
            enabled,
            BtnLook {
                fill: Self::accent(),
                stroke: Stroke::NONE,
                text: Color32::WHITE,
            },
            BtnLook {
                fill: Self::accent_hover(),
                stroke: Stroke::NONE,
                text: Color32::WHITE,
            },
            BtnLook {
                fill: Self::disabled_bg(),
                stroke: Stroke::NONE,
                text: Self::text_disabled(),
            },
        )
    }

    /// 次按钮:白底灰边;hover 变蓝字蓝边。
    pub(crate) fn secondary_button(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        min_width: f32,
        enabled: bool,
    ) -> egui::Response {
        Self::button(
            ui,
            icon,
            text,
            min_width,
            Self::BUTTON_HEIGHT,
            enabled,
            BtnLook {
                fill: Self::surface(),
                stroke: Stroke::new(1.0, Self::border_strong()),
                text: Self::text_primary(),
            },
            BtnLook {
                fill: Self::accent_soft(),
                stroke: Stroke::new(1.0, Self::accent_border()),
                text: Self::accent(),
            },
            BtnLook {
                fill: Self::surface_subtle(),
                stroke: Stroke::new(1.0, Self::border()),
                text: Self::text_disabled(),
            },
        )
    }

    /// 小号次按钮(30px):命中批次导航、卡片内行内操作等场景。
    pub(crate) fn small_secondary_button(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        min_width: f32,
        enabled: bool,
    ) -> egui::Response {
        Self::button(
            ui,
            icon,
            text,
            min_width,
            30.0,
            enabled,
            BtnLook {
                fill: Self::surface(),
                stroke: Stroke::new(1.0, Self::border()),
                text: Self::text_primary(),
            },
            BtnLook {
                fill: Self::accent_soft(),
                stroke: Stroke::new(1.0, Self::accent_border()),
                text: Self::accent(),
            },
            BtnLook {
                fill: Self::surface_subtle(),
                stroke: Stroke::new(1.0, Self::border()),
                text: Self::text_disabled(),
            },
        )
    }

    /// 小号危险描边按钮(30px):维护区「清空资料库」等行内危险操作。
    pub(crate) fn small_danger_outline_button(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        min_width: f32,
        enabled: bool,
    ) -> egui::Response {
        Self::button(
            ui,
            icon,
            text,
            min_width,
            30.0,
            enabled,
            BtnLook {
                fill: Self::surface(),
                stroke: Stroke::new(1.0, Self::danger()),
                text: Self::danger(),
            },
            BtnLook {
                fill: Self::danger_soft(),
                stroke: Stroke::new(1.0, Self::danger()),
                text: Self::danger(),
            },
            BtnLook {
                fill: Self::surface_subtle(),
                stroke: Stroke::new(1.0, Self::border()),
                text: Self::text_disabled(),
            },
        )
    }

    /// 危险实心按钮(弹框里的确认动作)。
    pub(crate) fn danger_button(
        ui: &mut egui::Ui,
        text: &str,
        min_width: f32,
        enabled: bool,
    ) -> egui::Response {
        Self::button(
            ui,
            None,
            text,
            min_width,
            Self::BUTTON_HEIGHT,
            enabled,
            BtnLook {
                fill: Self::danger(),
                stroke: Stroke::NONE,
                text: Color32::WHITE,
            },
            BtnLook {
                fill: Self::danger_hover(),
                stroke: Stroke::NONE,
                text: Color32::WHITE,
            },
            BtnLook {
                fill: Self::disabled_bg(),
                stroke: Stroke::NONE,
                text: Self::text_disabled(),
            },
        )
    }

    /// 行内文字操作(表格/树行的「打开」「重解析」等):12px 蓝字,hover 深蓝。
    pub(crate) fn link_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
        Self::text_link(ui, text, Self::accent(), Self::accent_hover())
    }

    /// 行内文字操作,hover 变红(「移除」专用)。
    pub(crate) fn danger_link_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
        Self::text_link(ui, text, Self::accent(), Self::danger())
    }

    fn text_link(ui: &mut egui::Ui, text: &str, idle: Color32, hover: Color32) -> egui::Response {
        let enabled = ui.is_enabled();
        let font = egui::FontId::proportional(12.0);
        let galley = ui
            .painter()
            .layout_no_wrap(text.to_owned(), font.clone(), idle);
        let size = egui::vec2(galley.size().x + 8.0, (galley.size().y + 8.0).max(20.0));
        let sense = if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(size, sense);
        if ui.is_rect_visible(rect) {
            let color = if !enabled {
                Self::text_disabled()
            } else if response.hovered() {
                hover
            } else {
                idle
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                font,
                color,
            );
        }
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response
    }

    /// 蓝底白勾复选框(自带样式;egui 原生 checkbox 画不出选中蓝底)。
    pub(crate) fn accent_checkbox(
        ui: &mut egui::Ui,
        checked: &mut bool,
        text: &str,
    ) -> egui::Response {
        let enabled = ui.is_enabled();
        let font = egui::FontId::proportional(14.0);
        let galley =
            ui.painter()
                .layout_no_wrap(text.to_owned(), font.clone(), Self::text_primary());
        let box_size = 16.0;
        let size = egui::vec2(
            box_size + 6.0 + galley.size().x,
            (galley.size().y + 4.0).max(22.0),
        );
        let sense = if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, mut response) = ui.allocate_exact_size(size, sense);
        if response.clicked() {
            *checked = !*checked;
            response.mark_changed();
        }
        if ui.is_rect_visible(rect) {
            let box_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + box_size / 2.0, rect.center().y),
                egui::vec2(box_size, box_size),
            );
            let (fill, stroke) = if !enabled {
                (Self::disabled_bg(), Stroke::new(1.0, Self::border()))
            } else if *checked {
                (Self::accent(), Stroke::new(1.0, Self::accent()))
            } else if response.hovered() {
                (Self::surface(), Stroke::new(1.0, Self::accent()))
            } else {
                (Self::surface(), Stroke::new(1.0, Self::border_strong()))
            };
            ui.painter().rect(
                box_rect,
                CornerRadius::same(4),
                fill,
                stroke,
                egui::StrokeKind::Inside,
            );
            if *checked {
                let c = box_rect.center();
                let s = box_size;
                ui.painter().add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x - s * 0.22, c.y + s * 0.01),
                        egui::pos2(c.x - s * 0.04, c.y + s * 0.19),
                        egui::pos2(c.x + s * 0.24, c.y - s * 0.17),
                    ],
                    Stroke::new(1.6, Color32::WHITE),
                ));
            }
            ui.painter().text(
                egui::pos2(box_rect.right() + 6.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                font,
                if enabled {
                    Self::text_primary()
                } else {
                    Self::text_disabled()
                },
            );
        }
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response
    }

    /// 单行输入框 + 框内右侧清空小叉(有内容时出现;点击清空并保持焦点)。
    /// 返回底层 TextEdit 的 response,外层可继续判 lost_focus/Enter。
    pub(crate) fn clearable_input(
        ui: &mut egui::Ui,
        text: &mut String,
        width: f32,
        hint: &str,
    ) -> egui::Response {
        let response = ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(width)
                // 右侧固定留 20px 给小叉,滚动的文字不会伸到叉底下。
                .margin(egui::Margin {
                    left: 4,
                    right: 20,
                    top: 2,
                    bottom: 2,
                })
                .hint_text(hint),
        );
        if !text.is_empty() {
            let side = 14.0;
            let rect = egui::Rect::from_center_size(
                egui::pos2(
                    response.rect.right() - side / 2.0 - 4.0,
                    response.rect.center().y,
                ),
                egui::vec2(side, side),
            );
            let clear = ui.allocate_rect(rect, egui::Sense::click());
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(14.0),
                if clear.hovered() {
                    Self::text_primary()
                } else {
                    Self::text_muted()
                },
            );
            if clear.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if clear.clicked() {
                text.clear();
                response.request_focus();
            }
        }
        response
    }

    /// 顶栏/卡片角的小型状态徽标(软底色 + 同色文字 + 小图标)。
    pub(crate) fn status_badge(
        ui: &mut egui::Ui,
        icon: Option<Icon>,
        text: &str,
        fill: Color32,
        fg: Color32,
    ) {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(10, 5))
            .fill(fill)
            .corner_radius(CornerRadius::same(Self::BADGE_RADIUS))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(icon) = icon {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                        Self::paint_icon(ui.painter(), icon, rect, fg);
                    }
                    ui.label(egui::RichText::new(text).size(12.0).color(fg));
                });
            });
    }

    /// 进度条:浅灰轨道 + 蓝色填充(高度取任务书的 10~12px)。
    pub(crate) fn progress_bar(ui: &mut egui::Ui, fraction: f32, width: f32) {
        let height = 11.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width.max(60.0), height), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(6), Self::track());
            let fill_w = (rect.width() * fraction.clamp(0.0, 1.0)).max(0.0);
            if fill_w > 0.0 {
                let fill = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
                ui.painter()
                    .rect_filled(fill, CornerRadius::same(6), Self::accent());
            }
        }
    }

    /// 浅黄底警告条(大文档提示等)。
    pub(crate) fn warn_banner(ui: &mut egui::Ui, text: &str) {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(10, 8))
            .fill(Self::warning_soft())
            .corner_radius(CornerRadius::same(6))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
                    Self::paint_icon(ui.painter(), Icon::Warn, rect, Self::warning());
                    ui.label(egui::RichText::new(text).size(12.0).color(Self::warning()));
                });
            });
    }

    /// 文件类别 → 图标与颜色(沿用 token 色:word 蓝/pdf 红/excel 绿/ppt 橙)。
    pub(crate) fn file_type_look(file_type: &str) -> (Icon, Color32) {
        match file_type {
            "word" => (Icon::Doc, Self::accent()),
            "pdf" => (Icon::Doc, Self::danger()),
            "excel" => (Icon::Doc, Self::success()),
            "ppt" => (Icon::Doc, Self::warning()),
            "epub" => (Icon::Doc, Self::text_muted()),
            _ => (Icon::Doc, Self::text_muted()),
        }
    }

    /// 空状态提示(普通次要色)。
    pub(crate) fn empty_note(ui: &mut egui::Ui, text: &str) {
        ui.label(
            egui::RichText::new(text)
                .size(14.0)
                .color(Self::text_secondary()),
        );
    }

    // ---------- 图标绘制(painter 线条,不引图标库) ----------

    /// 在 rect 内画一个线条图标;rect 通常是 16~18px 见方。
    pub(crate) fn paint_icon(
        painter: &egui::Painter,
        icon: Icon,
        rect: egui::Rect,
        color: Color32,
    ) {
        let s = rect.width().min(rect.height());
        let c = rect.center();
        let l = c.x - s * 0.5;
        let r = c.x + s * 0.5;
        let t = c.y - s * 0.5;
        let b = c.y + s * 0.5;
        let w = (s * 0.085).clamp(1.1, 1.6);
        let st = Stroke::new(w, color);
        let line = |p: &egui::Painter, pts: Vec<egui::Pos2>| {
            p.add(egui::Shape::line(pts, st));
        };
        match icon {
            Icon::Folder => {
                let tab = s * 0.36;
                let y1 = t + s * 0.24;
                let y2 = t + s * 0.42;
                line(
                    painter,
                    vec![
                        egui::pos2(l + s * 0.10, b - s * 0.20),
                        egui::pos2(l + s * 0.10, y1),
                        egui::pos2(l + s * 0.10 + tab * 0.65, y1),
                        egui::pos2(l + s * 0.10 + tab, y2),
                        egui::pos2(r - s * 0.10, y2),
                        egui::pos2(r - s * 0.10, b - s * 0.20),
                        egui::pos2(l + s * 0.10, b - s * 0.20),
                    ],
                );
            }
            Icon::FolderPlus => {
                Self::paint_icon(painter, Icon::Folder, rect, color);
                let cx = r - s * 0.16;
                let cy = b - s * 0.18;
                painter.line_segment(
                    [egui::pos2(cx - s * 0.11, cy), egui::pos2(cx + s * 0.11, cy)],
                    Stroke::new(w, color),
                );
                painter.line_segment(
                    [egui::pos2(cx, cy - s * 0.11), egui::pos2(cx, cy + s * 0.11)],
                    Stroke::new(w, color),
                );
            }
            Icon::Doc => {
                let x0 = l + s * 0.20;
                let x1 = r - s * 0.20;
                let y0 = t + s * 0.12;
                let y1 = b - s * 0.12;
                let fx = x1 - s * 0.26;
                let fy = y0 + s * 0.26;
                line(
                    painter,
                    vec![
                        egui::pos2(x0, y0),
                        egui::pos2(fx, y0),
                        egui::pos2(x1, fy),
                        egui::pos2(x1, y1),
                        egui::pos2(x0, y1),
                        egui::pos2(x0, y0),
                    ],
                );
                line(
                    painter,
                    vec![egui::pos2(fx, y0), egui::pos2(fx, fy), egui::pos2(x1, fy)],
                );
            }
            Icon::DocPlus => {
                Self::paint_icon(painter, Icon::Doc, rect, color);
                let cx = r - s * 0.16;
                let cy = b - s * 0.18;
                painter.line_segment(
                    [egui::pos2(cx - s * 0.11, cy), egui::pos2(cx + s * 0.11, cy)],
                    Stroke::new(w, color),
                );
                painter.line_segment(
                    [egui::pos2(cx, cy - s * 0.11), egui::pos2(cx, cy + s * 0.11)],
                    Stroke::new(w, color),
                );
            }
            Icon::Search => {
                let cc = egui::pos2(c.x - s * 0.06, c.y - s * 0.06);
                let rr = s * 0.28;
                painter.circle_stroke(cc, rr, st);
                let d = (rr + s * 0.02) * 0.7071;
                painter.line_segment(
                    [
                        egui::pos2(cc.x + d, cc.y + d),
                        egui::pos2(c.x + s * 0.40, c.y + s * 0.40),
                    ],
                    st,
                );
            }
            Icon::Gear => {
                painter.circle_stroke(c, s * 0.26, st);
                for i in 0..8 {
                    let a = i as f32 * core::f32::consts::TAU / 8.0;
                    let (sin, cos) = a.sin_cos();
                    painter.line_segment(
                        [
                            c + egui::vec2(cos * s * 0.33, sin * s * 0.33),
                            c + egui::vec2(cos * s * 0.45, sin * s * 0.45),
                        ],
                        st,
                    );
                }
                painter.circle_stroke(c, s * 0.09, st);
            }
            Icon::External => {
                let x0 = l + s * 0.14;
                let y0 = t + s * 0.36;
                let x1 = r - s * 0.36;
                let y1 = b - s * 0.14;
                line(
                    painter,
                    vec![
                        egui::pos2(x0 + s * 0.34, y0),
                        egui::pos2(x0, y0),
                        egui::pos2(x0, y1),
                        egui::pos2(x1, y1),
                        egui::pos2(x1, y0 + s * 0.34),
                    ],
                );
                let tip = egui::pos2(r - s * 0.12, t + s * 0.12);
                painter.line_segment([egui::pos2(c.x, c.y), tip], st);
                painter.line_segment([egui::pos2(tip.x - s * 0.18, tip.y), tip], st);
                painter.line_segment([egui::pos2(tip.x, tip.y + s * 0.18), tip], st);
            }
            Icon::Refresh => {
                let rr = s * 0.30;
                let mut pts = Vec::with_capacity(15);
                for i in 0..=14 {
                    let a = -0.6 + i as f32 * 4.6 / 14.0;
                    let (sin, cos) = a.sin_cos();
                    pts.push(c + egui::vec2(cos * rr, sin * rr));
                }
                line(painter, pts.clone());
                if let (Some(&p_end), Some(&p_prev)) = (pts.last(), pts.get(pts.len() - 2)) {
                    let v = (p_end - p_prev).normalized();
                    let n = egui::vec2(-v.y, v.x);
                    painter.line_segment([p_end - v * s * 0.16 + n * s * 0.11, p_end], st);
                    painter.line_segment([p_end - v * s * 0.16 - n * s * 0.11, p_end], st);
                }
            }
            Icon::Trash => {
                let y = t + s * 0.26;
                painter.line_segment(
                    [egui::pos2(l + s * 0.16, y), egui::pos2(r - s * 0.16, y)],
                    st,
                );
                line(
                    painter,
                    vec![
                        egui::pos2(c.x - s * 0.10, y),
                        egui::pos2(c.x - s * 0.10, t + s * 0.16),
                        egui::pos2(c.x + s * 0.10, t + s * 0.16),
                        egui::pos2(c.x + s * 0.10, y),
                    ],
                );
                line(
                    painter,
                    vec![
                        egui::pos2(l + s * 0.22, y),
                        egui::pos2(l + s * 0.28, b - s * 0.14),
                        egui::pos2(r - s * 0.28, b - s * 0.14),
                        egui::pos2(r - s * 0.22, y),
                    ],
                );
                painter.line_segment(
                    [
                        egui::pos2(c.x - s * 0.08, t + s * 0.40),
                        egui::pos2(c.x - s * 0.06, b - s * 0.26),
                    ],
                    st,
                );
                painter.line_segment(
                    [
                        egui::pos2(c.x + s * 0.08, t + s * 0.40),
                        egui::pos2(c.x + s * 0.06, b - s * 0.26),
                    ],
                    st,
                );
            }
            Icon::Shield => {
                line(
                    painter,
                    vec![
                        egui::pos2(c.x, t + s * 0.10),
                        egui::pos2(r - s * 0.16, t + s * 0.24),
                        egui::pos2(r - s * 0.16, c.y + s * 0.02),
                        egui::pos2(c.x, b - s * 0.10),
                        egui::pos2(l + s * 0.16, c.y + s * 0.02),
                        egui::pos2(l + s * 0.16, t + s * 0.24),
                        egui::pos2(c.x, t + s * 0.10),
                    ],
                );
                line(
                    painter,
                    vec![
                        egui::pos2(c.x - s * 0.11, c.y + s * 0.01),
                        egui::pos2(c.x - s * 0.02, c.y + s * 0.12),
                        egui::pos2(c.x + s * 0.15, c.y - s * 0.10),
                    ],
                );
            }
            Icon::Database => {
                let rx = s * 0.30;
                let ry = s * 0.13;
                let top_cy = t + s * 0.24;
                let bot_cy = b - s * 0.24;
                let ellipse = |cy: f32, from: f32, to: f32, n: usize| {
                    (0..=n)
                        .map(|i| {
                            let a = from + (to - from) * i as f32 / n as f32;
                            let (sin, cos) = a.sin_cos();
                            egui::pos2(c.x + cos * rx, cy + sin * ry)
                        })
                        .collect::<Vec<_>>()
                };
                line(painter, ellipse(top_cy, 0.0, core::f32::consts::TAU, 16));
                painter.line_segment(
                    [egui::pos2(c.x - rx, top_cy), egui::pos2(c.x - rx, bot_cy)],
                    st,
                );
                painter.line_segment(
                    [egui::pos2(c.x + rx, top_cy), egui::pos2(c.x + rx, bot_cy)],
                    st,
                );
                line(painter, ellipse(bot_cy, 0.0, core::f32::consts::PI, 8));
                line(painter, ellipse(c.y, 0.0, core::f32::consts::PI, 8));
            }
            Icon::Copy => {
                let back = egui::Rect::from_min_max(
                    egui::pos2(c.x - s * 0.02, t + s * 0.12),
                    egui::pos2(r - s * 0.12, c.y + s * 0.10),
                );
                painter.rect_stroke(back, CornerRadius::same(2), st, egui::StrokeKind::Inside);
                let front = egui::Rect::from_min_max(
                    egui::pos2(l + s * 0.12, c.y - s * 0.02),
                    egui::pos2(c.x + s * 0.02, b - s * 0.12),
                );
                painter.rect(
                    front,
                    CornerRadius::same(2),
                    Self::surface(),
                    st,
                    egui::StrokeKind::Inside,
                );
            }
            Icon::Warn => {
                line(
                    painter,
                    vec![
                        egui::pos2(c.x, t + s * 0.10),
                        egui::pos2(r - s * 0.12, b - s * 0.14),
                        egui::pos2(l + s * 0.12, b - s * 0.14),
                        egui::pos2(c.x, t + s * 0.10),
                    ],
                );
                painter.line_segment(
                    [
                        egui::pos2(c.x, t + s * 0.38),
                        egui::pos2(c.x, c.y + s * 0.10),
                    ],
                    st,
                );
                painter.circle_filled(egui::pos2(c.x, b - s * 0.26), w * 0.55, color);
            }
            Icon::CheckCircle => {
                painter.circle_stroke(c, s * 0.36, st);
                line(
                    painter,
                    vec![
                        egui::pos2(c.x - s * 0.16, c.y + s * 0.01),
                        egui::pos2(c.x - s * 0.04, c.y + s * 0.14),
                        egui::pos2(c.x + s * 0.18, c.y - s * 0.14),
                    ],
                );
            }
            Icon::Loader => {
                let rr = s * 0.32;
                let pts: Vec<egui::Pos2> = (0..=10)
                    .map(|i| {
                        let a = -1.6 + i as f32 * 4.4 / 10.0;
                        let (sin, cos) = a.sin_cos();
                        c + egui::vec2(cos * rr, sin * rr)
                    })
                    .collect();
                line(painter, pts);
            }
        }
    }

    // ---------- 全局样式 ----------

    /// 配置 egui 视觉样式:浅灰工作区 + 白色卡片 + 蓝色主色。
    pub(crate) fn configure_ui_style(ctx: &egui::Context) {
        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = Self::canvas();
        visuals.window_fill = Self::surface();
        visuals.faint_bg_color = Self::surface_subtle();
        // extreme_bg_color 在亮色主题下是进度条轨道等「凹陷」区域的底色;
        // 输入框底色由 text_edit_bg_color 单独指定为白。
        visuals.extreme_bg_color = Self::track();
        visuals.text_edit_bg_color = Some(Self::surface());
        visuals.hyperlink_color = Self::accent();
        visuals.warn_fg_color = Self::warning();
        visuals.error_fg_color = Self::danger();
        visuals.window_corner_radius = CornerRadius::same(Self::MODAL_RADIUS);
        visuals.menu_corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.window_shadow = Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(18),
        };
        visuals.window_stroke = Stroke::new(1.0, Self::border());
        visuals.widgets.noninteractive.bg_fill = Self::surface();
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Self::border());
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Self::text_primary());
        visuals.widgets.inactive.bg_fill = Self::surface();
        visuals.widgets.inactive.weak_bg_fill = Self::surface();
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Self::border_strong());
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Self::text_primary());
        visuals.widgets.hovered.bg_fill = Self::accent_soft();
        visuals.widgets.hovered.weak_bg_fill = Self::accent_soft();
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Self::accent_border());
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Self::accent());
        visuals.widgets.active.bg_fill = Self::accent_soft();
        visuals.widgets.active.weak_bg_fill = Self::accent_soft();
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, Self::accent());
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, Self::accent());
        visuals.widgets.open.bg_fill = Self::surface();
        visuals.widgets.open.weak_bg_fill = Self::surface();
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, Self::accent_border());
        visuals.widgets.open.fg_stroke = Stroke::new(1.0, Self::text_primary());
        visuals.widgets.noninteractive.corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.widgets.inactive.corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.widgets.hovered.corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.widgets.active.corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.widgets.open.corner_radius = CornerRadius::same(Self::INPUT_RADIUS);
        visuals.selection.bg_fill = Self::accent();
        visuals.selection.stroke = Stroke::new(1.0, Self::accent());
        // Windows 的桌面文字通常更接近像素对齐效果，关闭 egui 的子像素分箱可减少
        // 小字号 Latin 字符的发虚；CJK 字符本身不会启用该模式。
        if cfg!(windows) {
            visuals.text_options.subpixel_binning = false;
        }
        ctx.set_visuals(visuals);

        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 8.0);
            style.spacing.interact_size = egui::vec2(30.0, 32.0);
            style.spacing.icon_width = 18.0;
            style.spacing.icon_width_inner = 12.0;
            style.spacing.icon_spacing = 6.0;
            style.spacing.combo_width = 150.0;
            style.spacing.window_margin = egui::Margin::same(10);

            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(28.0));
        });
    }
}
