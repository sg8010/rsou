//! 视觉主题：色板、通用控件与全局样式。

use super::*;

// 部分控件(letter_badge/green_button/secondary_button/action_row/sub_panel 等)
// 在阶段 2/3 的页面中才会用到,先整体保留同一套设计语言。
#[allow(dead_code)]
impl RsouApp {
    // ---------- 颜色与基础组件 ----------

    pub(crate) fn navy() -> Color32 {
        Color32::from_rgb(23, 39, 59)
    }

    pub(crate) fn navy_2() -> Color32 {
        Color32::from_rgb(32, 52, 77)
    }

    pub(crate) fn sidebar_text() -> Color32 {
        Color32::from_rgb(231, 238, 247)
    }

    pub(crate) fn sidebar_muted() -> Color32 {
        Color32::from_rgb(144, 165, 187)
    }

    pub(crate) fn sidebar_faint() -> Color32 {
        Color32::from_rgb(129, 148, 170)
    }

    pub(crate) fn ink() -> Color32 {
        Color32::from_rgb(31, 48, 66)
    }

    pub(crate) fn muted() -> Color32 {
        Color32::from_rgb(113, 129, 150)
    }

    pub(crate) fn soft() -> Color32 {
        Color32::from_rgb(149, 165, 181)
    }

    pub(crate) fn canvas() -> Color32 {
        Color32::from_rgb(237, 242, 247)
    }

    pub(crate) fn white() -> Color32 {
        Color32::WHITE
    }

    pub(crate) fn surface() -> Color32 {
        Color32::from_rgb(251, 252, 254)
    }

    pub(crate) fn line() -> Color32 {
        Color32::from_rgb(220, 229, 238)
    }

    pub(crate) fn line_strong() -> Color32 {
        Color32::from_rgb(201, 214, 227)
    }

    pub(crate) fn blue() -> Color32 {
        Color32::from_rgb(43, 104, 197)
    }

    pub(crate) fn blue_soft() -> Color32 {
        Color32::from_rgb(234, 242, 255)
    }

    pub(crate) fn teal() -> Color32 {
        Color32::from_rgb(35, 139, 120)
    }

    pub(crate) fn teal_soft() -> Color32 {
        Color32::from_rgb(228, 246, 241)
    }

    pub(crate) fn amber() -> Color32 {
        Color32::from_rgb(189, 116, 47)
    }

    pub(crate) fn card_frame(fill: Color32, stroke: Color32, margin: i8) -> egui::Frame {
        egui::Frame::new()
            .inner_margin(egui::Margin::same(margin))
            .fill(fill)
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(CornerRadius::ZERO)
    }

    pub(crate) fn panel_frame() -> egui::Frame {
        egui::Frame::new()
            .inner_margin(egui::Margin::ZERO)
            .fill(Self::white())
            .stroke(Stroke::new(1.0, Self::line()))
            .corner_radius(CornerRadius::ZERO)
            .shadow(Shadow {
                offset: [0, 3],
                blur: 12,
                spread: 0,
                color: Color32::from_black_alpha(16),
            })
    }

    pub(crate) fn status_badge(ui: &mut egui::Ui, text: &str) {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 4))
            .fill(Self::teal_soft())
            .corner_radius(CornerRadius::ZERO)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.0, Self::teal());
                    ui.label(egui::RichText::new(text).size(13.0).color(Self::teal()));
                });
            });
    }

    pub(crate) fn letter_badge(ui: &mut egui::Ui, letter: &str, color: Color32) {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(9, 6))
            .fill(color)
            .corner_radius(CornerRadius::ZERO)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(letter)
                        .strong()
                        .size(15.0)
                        .color(Color32::WHITE),
                );
            });
    }

    pub(crate) fn primary_button(
        ui: &mut egui::Ui,
        text: &str,
        width: f32,
        enabled: bool,
    ) -> egui::Response {
        ui.add_enabled(
            enabled,
            egui::Button::new(egui::RichText::new(text).strong().color(Color32::WHITE))
                .min_size(egui::vec2(width, 36.0))
                .fill(Self::blue())
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::ZERO),
        )
    }

    pub(crate) fn green_button(ui: &mut egui::Ui, text: &str, width: f32) -> egui::Response {
        ui.add(
            egui::Button::new(egui::RichText::new(text).strong().color(Color32::WHITE))
                .min_size(egui::vec2(width, 42.0))
                .fill(Self::teal())
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::ZERO),
        )
    }

    pub(crate) fn secondary_button(ui: &mut egui::Ui, text: &str, width: f32) -> egui::Response {
        ui.add(
            egui::Button::new(egui::RichText::new(text).color(Self::muted()))
                .min_size(egui::vec2(width, 36.0))
                .fill(Self::white())
                .stroke(Stroke::new(1.0, Self::line_strong()))
                .corner_radius(CornerRadius::ZERO),
        )
    }

    pub(crate) fn action_row(
        ui: &mut egui::Ui,
        note: &str,
        add_buttons: impl FnOnce(&mut egui::Ui),
    ) {
        ui.add_space(17.0);
        ui.separator();
        ui.add_space(15.0);
        // 窗口较窄时让按钮逐个参与换行;宽窗口仍保持说明在左、操作在右的布局。
        let compact = ui.available_width() < 620.0;
        if compact {
            ui.horizontal_wrapped(|ui| {
                Self::action_note(ui, note);
                ui.add_space(17.0);
                add_buttons(ui);
            });
        } else {
            ui.horizontal(|ui| {
                Self::action_note(ui, note);
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    add_buttons,
                );
            });
        }
    }

    pub(crate) fn action_note(ui: &mut egui::Ui, note: &str) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            ui.painter()
                .circle_stroke(rect.center(), 5.0, Stroke::new(1.4, Self::teal()));
            ui.painter().line_segment(
                [
                    egui::pos2(rect.center().x, rect.center().y),
                    egui::pos2(rect.center().x + 2.5, rect.center().y + 2.0),
                ],
                Stroke::new(1.2, Self::teal()),
            );
            ui.label(egui::RichText::new(note).size(13.0).color(Self::muted()));
        });
    }

    pub(crate) fn work_panel(
        ui: &mut egui::Ui,
        index: &str,
        title: &str,
        hint: &str,
        status: Option<&str>,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) {
        Self::panel_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(index)
                        .size(13.0)
                        .strong()
                        .color(Self::blue()),
                );
                ui.label(
                    egui::RichText::new(title)
                        .size(17.0)
                        .strong()
                        .color(Self::ink()),
                );
                ui.label(egui::RichText::new(hint).size(13.0).color(Self::muted()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(status) = status {
                        Self::status_badge(ui, status);
                    }
                });
            });
            ui.separator();
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(18, 18))
                .show(ui, add_contents);
        });
    }

    pub(crate) fn sub_panel(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
        Self::card_frame(Self::surface(), Self::line(), 16).show(ui, add_contents);
    }

    /// 配置 egui 视觉样式:浅色工作区 + 深色流程侧栏。
    pub(crate) fn configure_ui_style(ctx: &egui::Context) {
        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = Self::canvas();
        visuals.window_fill = Self::white();
        visuals.faint_bg_color = Self::surface();
        visuals.extreme_bg_color = Self::white();
        visuals.text_edit_bg_color = Some(Self::white());
        visuals.hyperlink_color = Self::blue();
        visuals.warn_fg_color = Self::amber();
        visuals.error_fg_color = Color32::from_rgb(177, 74, 61);
        visuals.window_corner_radius = CornerRadius::ZERO;
        visuals.menu_corner_radius = CornerRadius::ZERO;
        visuals.window_shadow = Shadow {
            offset: [0, 3],
            blur: 12,
            spread: 0,
            color: Color32::from_black_alpha(18),
        };
        visuals.window_stroke = Stroke::new(1.0, Self::line());
        visuals.widgets.noninteractive.bg_fill = Self::white();
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Self::line());
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Self::ink());
        visuals.widgets.inactive.bg_fill = Self::white();
        visuals.widgets.inactive.weak_bg_fill = Self::white();
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Self::line_strong());
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Self::ink());
        visuals.widgets.hovered.bg_fill = Self::blue_soft();
        visuals.widgets.hovered.weak_bg_fill = Self::blue_soft();
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Self::blue());
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Self::blue());
        visuals.widgets.active.bg_fill = Self::blue_soft();
        visuals.widgets.active.weak_bg_fill = Self::blue_soft();
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, Self::blue());
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, Self::blue());
        visuals.widgets.noninteractive.corner_radius = CornerRadius::ZERO;
        visuals.widgets.inactive.corner_radius = CornerRadius::ZERO;
        visuals.widgets.hovered.corner_radius = CornerRadius::ZERO;
        visuals.widgets.active.corner_radius = CornerRadius::ZERO;
        visuals.widgets.open.corner_radius = CornerRadius::ZERO;
        visuals.selection.bg_fill = Self::blue_soft();
        visuals.selection.stroke = Stroke::new(1.0, Self::blue());
        // Windows 的桌面文字通常更接近像素对齐效果，关闭 egui 的子像素分箱可减少
        // 小字号 Latin 字符的发虚；CJK 字符本身不会启用该模式。
        if cfg!(windows) {
            visuals.text_options.subpixel_binning = false;
        }
        ctx.set_visuals(visuals);

        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 5.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
            style.spacing.interact_size = egui::vec2(32.0, 32.0);
            style.spacing.icon_width = 18.0;
            style.spacing.icon_width_inner = 13.0;
            style.spacing.icon_spacing = 5.0;
            style.spacing.combo_width = 160.0;
            style.spacing.window_margin = egui::Margin::same(10);

            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(17.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
            style
                .text_styles
                .insert(egui::TextStyle::Monospace, egui::FontId::monospace(16.0));
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(29.0));
        });
    }
}
