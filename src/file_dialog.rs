//! 内置文件对话框(Linux)
//!
//! 为什么自己画:rfd 在 Linux 上走 XDG Desktop Portal(DBus),Portal 不可用时回退成
//! 调用 `zenity` 外部进程。目标机(麒麟 / UOS / 精简桌面 / 无桌面的环境)往往两者都
//! 没有,点了「选择工作簿」就是毫无反应。这里用 egui 自己画一个:只依赖 `std::fs`
//! 列目录,不依赖 DBus / GTK / 任何外部可执行文件。
//!
//! 只负责「让用户选中一个文件并交回调用方」,不碰业务数据;目录列举、排序、过滤等
//! 纯逻辑在 rsou_lib::filebrowser(有单测)。

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, CornerRadius, Stroke};
use egui_extras::{Column, TableBuilder};

use rsou_lib::filebrowser::{self, Entry, FilterOpts};
/// 色值取自 app.rs 的主题(同一套视觉:白底、方角、蓝色主按钮)。
/// 改动主题时这里要跟着改。
mod palette {
    use eframe::egui::Color32;

    pub fn ink() -> Color32 {
        Color32::from_rgb(31, 48, 66)
    }
    pub fn muted() -> Color32 {
        Color32::from_rgb(113, 129, 150)
    }
    pub fn soft() -> Color32 {
        Color32::from_rgb(149, 165, 181)
    }
    pub fn white() -> Color32 {
        Color32::WHITE
    }
    pub fn surface() -> Color32 {
        Color32::from_rgb(251, 252, 254)
    }
    pub fn line() -> Color32 {
        Color32::from_rgb(220, 229, 238)
    }
    pub fn line_strong() -> Color32 {
        Color32::from_rgb(201, 214, 227)
    }
    pub fn blue() -> Color32 {
        Color32::from_rgb(43, 104, 197)
    }
    pub fn blue_soft() -> Color32 {
        Color32::from_rgb(234, 242, 255)
    }
    pub fn amber() -> Color32 {
        Color32::from_rgb(189, 116, 47)
    }
    pub fn danger() -> Color32 {
        Color32::from_rgb(177, 74, 61)
    }
}

/// 对话框用途:选择文件(打开)/ 指定保存路径(保存)/ 选择文件夹
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Open,
    Save,
    PickFolder,
}

/// 文件类型过滤器(后缀为空 = 不过滤,即「所有文件」)
#[derive(Clone)]
pub struct Filter {
    pub label: String,
    /// 小写、不含点
    pub exts: Vec<String>,
}

impl Filter {
    pub fn new(label: &str, exts: &[&str]) -> Self {
        Self {
            label: label.to_owned(),
            exts: exts.iter().map(|e| e.to_ascii_lowercase()).collect(),
        }
    }
}

/// 读取用过滤器:全部支持的文档格式 + 所有文件。
/// 扩展名清单取自 parse::supported_extensions(与导入扫描共用一份)。
pub fn document_filters() -> Vec<Filter> {
    vec![
        Filter::new(
            "文档(全部支持格式)",
            rsou_lib::parse::supported_extensions(),
        ),
        Filter::new("所有文件", &[]),
    ]
}

/// 用户在对话框里的最终操作
pub enum DialogAction {
    /// 本帧无操作,对话框继续显示
    None,
    /// 确认(打开选中文件 / 保存到指定路径)
    Picked(PathBuf),
    /// 取消(取消按钮 / Esc)
    Cancelled,
}

/// 列表里的点击意图(先记下来,等表格闭包结束、借用释放后再改自身状态)
enum RowIntent {
    /// 单击选中
    Select(PathBuf),
    /// 双击:(路径, 是否文件夹) — 文件夹进入,文件则确认/填入文件名
    Activate(PathBuf, bool),
}

/// visible_indices 里代表「上一级」的哨兵下标
const PARENT_ROW: usize = usize::MAX;

pub struct FileDialog {
    purpose: Purpose,
    title: String,
    /// 右上角说明当前在选择谁的文件
    hint: String,
    filters: Vec<Filter>,
    filter_idx: usize,
    /// 当前目录(绝对路径)
    dir: PathBuf,
    /// 当前目录的目录项(进入目录/刷新时重列一次,不是每帧)
    entries: Vec<Entry>,
    /// 「上一级」入口(始终显示,不受筛选影响)
    parent: Option<Entry>,
    /// 快捷位置(构造时算一次,节省每帧的 stat)
    places: Vec<(&'static str, PathBuf)>,
    /// 路径输入框(内容与 dir 同步;回车 / 点「转到」按它跳转)
    path_input: String,
    query: String,
    show_hidden: bool,
    /// 选中项(按路径记,筛选变化后依然有效)
    selected: Option<PathBuf>,
    /// 保存模式的文件名(允许带相对路径)
    file_name: String,
    error: Option<String>,
    /// 保存时目标已存在,等用户确认覆盖
    overwrite: Option<PathBuf>,
    /// 保存模式:第一帧把焦点给文件名输入框
    focus_name: bool,
}

impl FileDialog {
    /// 选择文件。dir 为上次停留的目录(无效时退回主目录)
    pub fn open(title: &str, hint: &str, dir: Option<PathBuf>, filters: Vec<Filter>) -> Self {
        Self::new(Purpose::Open, title, hint, dir, String::new(), filters)
    }

    /// 选择文件夹(导入文件夹用):列表只显示目录,确认键选中当前/选中的目录。
    /// 已知的简化:与文件选择一样只支持单选,多选文件夹暂不实现。
    pub fn pick_folder(title: &str, hint: &str, dir: Option<PathBuf>) -> Self {
        Self::new(
            Purpose::PickFolder,
            title,
            hint,
            dir,
            String::new(),
            vec![Filter::new("文件夹", &[])],
        )
    }

    /// 保存文件。default_name 为文件名输入框的初值
    ///
    /// 本阶段没有保存入口(检索结果导出在后续阶段接入),先保留对话框能力。
    #[allow(dead_code)]
    pub fn save(
        title: &str,
        hint: &str,
        dir: Option<PathBuf>,
        default_name: &str,
        filters: Vec<Filter>,
    ) -> Self {
        Self::new(
            Purpose::Save,
            title,
            hint,
            dir,
            default_name.to_owned(),
            filters,
        )
    }

    fn new(
        purpose: Purpose,
        title: &str,
        hint: &str,
        dir: Option<PathBuf>,
        file_name: String,
        filters: Vec<Filter>,
    ) -> Self {
        let start = dir
            .filter(|p| p.is_dir())
            .unwrap_or_else(filebrowser::home_dir);
        // 目录读不出来(权限/已被删)时退到主目录,保证对话框总能打开
        let (dir, entries) = match filebrowser::list_dir(&start) {
            Ok(entries) => (start, entries),
            Err(_) => {
                let home = filebrowser::home_dir();
                let entries = filebrowser::list_dir(&home).unwrap_or_default();
                (home, entries)
            }
        };
        let parent = parent_entry(&dir);
        let path_input = dir.display().to_string();
        Self {
            purpose,
            title: title.to_owned(),
            hint: hint.to_owned(),
            filters,
            filter_idx: 0,
            dir,
            entries,
            parent,
            places: filebrowser::places(),
            path_input,
            query: String::new(),
            show_hidden: false,
            selected: None,
            file_name,
            error: None,
            overwrite: None,
            focus_name: purpose == Purpose::Save,
        }
    }

    /// 当前所在目录(调用方据此记住下次打开位置)
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 画一帧。返回 Picked / Cancelled 时调用方应关闭对话框
    pub fn ui(&mut self, ctx: &egui::Context) -> DialogAction {
        let mut action = DialogAction::None;
        let modal = egui::Modal::new(egui::Id::new("rsou_file_dialog"))
            .backdrop_color(Color32::from_black_alpha(110))
            .frame(
                egui::Frame::new()
                    .fill(palette::white())
                    .stroke(Stroke::new(1.0, palette::line_strong()))
                    .corner_radius(CornerRadius::ZERO)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ctx, |ui| action = self.body(ui));

        // Esc 取消(组合框等弹层打开时让弹层先处理)
        if matches!(action, DialogAction::None)
            && modal.is_top_modal
            && !modal.any_popup_open
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            action = DialogAction::Cancelled;
        }
        action
    }

    // ---------- 状态变更 ----------

    /// 切到目标目录:先列目录,成功才真正切换(失败停在原地并提示)
    fn navigate_to(&mut self, dir: PathBuf) {
        match filebrowser::list_dir(&dir) {
            Ok(entries) => {
                self.dir = dir;
                self.entries = entries;
                self.parent = parent_entry(&self.dir);
                self.path_input = self.dir.display().to_string();
                self.selected = None;
                self.query.clear();
                self.overwrite = None;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// 重新列当前目录(刷新按钮;选中项还在就保留)
    fn refresh(&mut self) {
        match filebrowser::list_dir(&self.dir) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
                let keep = self
                    .selected
                    .as_ref()
                    .is_some_and(|p| self.entries.iter().any(|e| &e.path == p));
                if !keep {
                    self.selected = None;
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// 路径输入框回车 / 点「转到」:目录就进去,文件就选中(打开模式直接确认)
    fn goto_input(&mut self) -> DialogAction {
        let path = filebrowser::expand_input_path(&self.path_input, &self.dir);
        if path.is_dir() {
            self.navigate_to(path);
            return DialogAction::None;
        }
        if path.is_file() {
            return self.take_file(path);
        }
        // 保存模式:输入的是还不存在的新文件(另存为),落到对应目录 + 文件名
        if self.purpose == Purpose::Save
            && let Some(parent) = path.parent().filter(|p| p.is_dir())
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            self.navigate_to(parent.to_path_buf());
            self.file_name = name.to_owned();
            return DialogAction::None;
        }
        if self.purpose == Purpose::PickFolder {
            self.error = Some("请选择一个文件夹".to_owned());
            return DialogAction::None;
        }
        self.error = Some(format!("路径不存在: {}", path.display()));
        DialogAction::None
    }

    /// 用户明确指定了一个文件(双击 / 输入路径)
    fn take_file(&mut self, path: PathBuf) -> DialogAction {
        self.selected = Some(path.clone());
        self.overwrite = None;
        self.error = None;
        match self.purpose {
            Purpose::Open => DialogAction::Picked(path),
            // 文件夹选择模式不选文件(列表也不显示文件);走到这里说明用户
            // 在路径框里输入了文件路径,只记住它,不确认。
            Purpose::PickFolder => DialogAction::None,
            Purpose::Save => {
                if let Some(parent) = path.parent()
                    && parent != self.dir
                {
                    self.navigate_to(parent.to_path_buf());
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    self.file_name = name.to_owned();
                }
                DialogAction::None
            }
        }
    }

    /// 「打开 / 保存 / 选择此文件夹」按钮
    fn confirm(&mut self) -> DialogAction {
        if self.purpose == Purpose::Save {
            return self.confirm_save();
        }
        // 文件夹模式:选中了目录就选它,否则选当前所在目录。
        if self.purpose == Purpose::PickFolder {
            let target = match self.selected_entry() {
                Some((path, true)) => path,
                _ => self.dir.clone(),
            };
            return DialogAction::Picked(target);
        }
        match self.selected_entry() {
            Some((path, true)) => {
                self.navigate_to(path);
                DialogAction::None
            }
            Some((path, false)) => DialogAction::Picked(path),
            None => {
                self.error = Some("请先选择一个文件".to_owned());
                DialogAction::None
            }
        }
    }

    /// 保存:补后缀 → 校验目录 → 已存在则先要一次覆盖确认
    fn confirm_save(&mut self) -> DialogAction {
        let raw = self.file_name.trim().to_owned();
        if raw.is_empty() {
            self.error = Some("请输入文件名".to_owned());
            return DialogAction::None;
        }
        let mut target = filebrowser::expand_input_path(&raw, &self.dir);
        let ext = self.filters[self.filter_idx].exts.first().cloned();
        if let (Some(name), Some(ext)) = (target.file_name().and_then(|n| n.to_str()), ext) {
            let fixed = filebrowser::ensure_extension(name, &ext);
            if fixed != name {
                target = target.with_file_name(fixed);
            }
        }
        if target.is_dir() {
            self.error = Some("该路径是一个文件夹,请输入文件名".to_owned());
            return DialogAction::None;
        }
        if !target.parent().is_some_and(Path::is_dir) {
            self.error = Some(format!(
                "文件夹不存在: {}",
                target.parent().unwrap_or(Path::new("")).display()
            ));
            return DialogAction::None;
        }
        // 覆盖是有破坏性的:第一次点「保存」只提示,再点一次「覆盖保存」才真的写
        if target.exists() && self.overwrite.as_ref() != Some(&target) {
            self.error = None;
            self.overwrite = Some(target);
            return DialogAction::None;
        }
        DialogAction::Picked(target)
    }

    // ---------- 读取状态 ----------

    fn selected_entry(&self) -> Option<(PathBuf, bool)> {
        let path = self.selected.as_ref()?;
        let entry = self
            .entries
            .iter()
            .find(|e| &e.path == path)
            .or_else(|| self.parent.as_ref().filter(|p| &p.path == path))?;
        Some((entry.path.clone(), entry.is_dir))
    }

    /// 当前可见行:第 0 行是「上一级」(有的话),其后是过滤后的目录项(存的是 entries 下标)
    fn visible_rows(&self) -> Vec<usize> {
        let exts = &self.filters[self.filter_idx].exts;
        let opts = FilterOpts {
            show_hidden: self.show_hidden,
            query: &self.query,
            exts,
        };
        let mut rows = Vec::new();
        if self.parent.is_some() {
            rows.push(PARENT_ROW);
        }
        rows.extend(filebrowser::filter_indices(&self.entries, &opts));
        // 文件夹选择模式只显示目录(文件对选择没有意义)。
        if self.purpose == Purpose::PickFolder {
            rows.retain(|&i| i == PARENT_ROW || self.entries[i].is_dir);
        }
        rows
    }

    // ---------- 绘制 ----------

    fn body(&mut self, ui: &mut egui::Ui) -> DialogAction {
        let mut nav: Option<PathBuf> = None;
        let mut refresh = false;
        let mut goto_input = false;
        let mut row_intent: Option<RowIntent> = None;
        let mut confirm = false;
        let mut cancel = false;
        let mut filter_changed: Option<usize> = None;
        let mut hidden_changed = false;

        ui.set_min_width(900.0);
        ui.set_max_width(900.0);

        // 标题 + 用途提示
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&self.title)
                    .size(19.0)
                    .strong()
                    .color(palette::ink()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(&self.hint)
                        .size(12.0)
                        .color(palette::soft()),
                );
            });
        });
        ui.add_space(10.0);

        // 快捷位置 + 刷新
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("位置")
                    .size(12.0)
                    .color(palette::soft()),
            );
            for (label, path) in self.places.iter() {
                let active = self.dir == *path;
                if chip(ui, label, active).clicked() && !active {
                    nav = Some(path.clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if small_button(ui, "刷新  ↻").clicked() {
                    refresh = true;
                }
                if self.parent.is_some() && small_button(ui, "上一级  ↑").clicked() {
                    nav = self.parent.as_ref().map(|p| p.path.clone());
                }
            });
        });
        ui.add_space(8.0);

        // 路径输入:回车或「转到」跳转
        ui.horizontal(|ui| {
            let button_width = 62.0;
            let width = (ui.available_width() - button_width - 8.0).max(160.0);
            let response = ui.add_sized(
                [width, 30.0],
                egui::TextEdit::singleline(&mut self.path_input)
                    .margin(egui::Margin::symmetric(8, 5))
                    .hint_text("直接输入路径,回车跳转"),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                goto_input = true;
            }
            if ui
                .add_sized([button_width, 30.0], egui::Button::new("转到"))
                .clicked()
            {
                goto_input = true;
            }
        });
        ui.add_space(8.0);

        // 筛选行:名称关键字 + 文件类型 + 隐藏文件
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("筛选")
                    .size(12.0)
                    .color(palette::soft()),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(200.0)
                    .hint_text("按名称筛选"),
            );
            if self.filters.len() > 1 {
                ui.add_space(6.0);
                let labels: Vec<String> = self.filters.iter().map(|f| f.label.clone()).collect();
                egui::ComboBox::from_id_salt("rsou_file_dialog_filter")
                    .width(300.0)
                    .selected_text(&labels[self.filter_idx])
                    .show_ui(ui, |ui| {
                        for (index, label) in labels.iter().enumerate() {
                            if ui
                                .selectable_label(index == self.filter_idx, label)
                                .clicked()
                            {
                                filter_changed = Some(index);
                            }
                        }
                    });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.checkbox(&mut self.show_hidden, "显示隐藏文件").changed() {
                    hidden_changed = true;
                }
            });
        });
        ui.add_space(8.0);

        // 目录列表(虚拟滚动:只画可见行)
        let visible = self.visible_rows();
        let selected = self.selected.clone();
        let parent = self.parent.as_ref();
        let entries = &self.entries;
        if visible.is_empty() {
            egui::Frame::new()
                .fill(palette::surface())
                .stroke(Stroke::new(1.0, palette::line()))
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(ui.available_width(), 320.0));
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "没有可显示的文件(试试放宽筛选条件或勾选显示隐藏文件)",
                            )
                            .size(13.0)
                            .color(palette::soft()),
                        );
                    });
                });
        } else {
            let row_h = 24.0;
            TableBuilder::new(ui)
                .id_salt("rsou_file_dialog_list")
                .striped(true)
                .vscroll(true)
                .min_scrolled_height(378.0)
                .max_scroll_height(378.0)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder().at_least(320.0).clip(true))
                .column(Column::exact(110.0).clip(true))
                .column(Column::exact(90.0).clip(true))
                .header(26.0, |mut header| {
                    for title in ["名称", "大小", "类型"] {
                        header.col(|ui| {
                            ui.label(
                                egui::RichText::new(title)
                                    .size(12.0)
                                    .strong()
                                    .color(palette::muted()),
                            );
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, visible.len(), |mut row| {
                        let index = visible[row.index()];
                        let entry: Option<&Entry> = if index == PARENT_ROW {
                            parent
                        } else {
                            entries.get(index)
                        };
                        let Some(entry) = entry else {
                            // 理论上到不了(下标来自内部过滤);真出现数据不一致也保持表格几何完整
                            for _ in 0..3 {
                                row.col(|_| {});
                            }
                            return;
                        };
                        row.set_selected(selected.as_deref() == Some(entry.path.as_path()));
                        let is_dir = entry.is_dir;
                        row.col(|ui| {
                            let (color, mark) = if is_dir {
                                (palette::ink(), "▣ ")
                            } else {
                                (palette::ink(), "▪ ")
                            };
                            let response = ui
                                .label(
                                    egui::RichText::new(format!("{mark}{}", entry.name))
                                        .size(14.0)
                                        .color(color),
                                )
                                // 标签默认只响应 hover，会挡住表格行的点击；把点击能力
                                // 加回标签本身，文件名文字和行内空白区域行为保持一致。
                                .interact(egui::Sense::click())
                                .on_hover_cursor(egui::CursorIcon::Default);
                            if response.double_clicked() {
                                row_intent = Some(RowIntent::Activate(entry.path.clone(), is_dir));
                            } else if response.clicked() {
                                row_intent = Some(RowIntent::Select(entry.path.clone()));
                            }
                        });
                        row.col(|ui| {
                            ui.label(
                                egui::RichText::new(entry.size_label())
                                    .size(12.0)
                                    .color(palette::muted()),
                            );
                        });
                        row.col(|ui| {
                            ui.label(
                                egui::RichText::new(entry.kind_label())
                                    .size(12.0)
                                    .color(palette::soft()),
                            );
                        });
                        let response = row.response();
                        if row_intent.is_some() {
                            return;
                        }
                        if response.double_clicked() {
                            row_intent = Some(RowIntent::Activate(entry.path.clone(), is_dir));
                        } else if response.clicked() {
                            row_intent = Some(RowIntent::Select(entry.path.clone()));
                        }
                    });
                });
        }

        // 列表已画完(借用结束),这里落地本帧的点击/切换
        if let Some(index) = filter_changed
            && index != self.filter_idx
        {
            self.filter_idx = index;
            self.selected = None;
            self.overwrite = None;
            self.error = None;
        }
        if hidden_changed {
            self.overwrite = None;
            self.error = None;
        }
        if let Some(dir) = nav {
            self.navigate_to(dir);
        }
        if refresh {
            self.refresh();
        }
        if goto_input {
            return self.goto_input();
        }
        match row_intent {
            Some(RowIntent::Select(path)) => {
                self.selected = Some(path);
                self.overwrite = None;
                self.error = None;
            }
            Some(RowIntent::Activate(path, true)) => self.navigate_to(path),
            Some(RowIntent::Activate(path, false)) => return self.take_file(path),
            None => {}
        }

        // 底部:保存模式的文件名 + 提示 + 按钮
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(10.0);

        if self.purpose == Purpose::Save {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("文件名")
                        .size(13.0)
                        .color(palette::muted()),
                );
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.file_name)
                        .desired_width(360.0)
                        .hint_text("要保存的文件名"),
                );
                if self.focus_name {
                    response.request_focus();
                    self.focus_name = false;
                }
                if response.changed() {
                    self.overwrite = None;
                    self.error = None;
                }
                let hint = match self.filters[self.filter_idx].exts.first() {
                    Some(ext) => format!("保存到 {} · 自动补 .{ext}", self.dir.display()),
                    None => format!("保存到 {}", self.dir.display()),
                };
                ui.label(egui::RichText::new(hint).size(12.0).color(palette::soft()));
            });
            ui.add_space(6.0);
        }

        if let Some(error) = &self.error {
            ui.label(
                egui::RichText::new(error)
                    .size(12.0)
                    .color(palette::danger()),
            );
        } else if let Some(target) = &self.overwrite {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.label(
                egui::RichText::new(format!("文件已存在:{name} — 再点一次「覆盖保存」将覆盖它"))
                    .size(12.0)
                    .color(palette::amber()),
            );
        } else {
            ui.label(egui::RichText::new(" ").size(12.0));
        }
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let tip = if self.purpose == Purpose::PickFolder {
                match self.selected_entry() {
                    Some((path, true)) => format!(
                        "已选择:{}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    _ => format!("将导入当前文件夹:{}", self.dir.display()),
                }
            } else {
                match self.selected_entry() {
                    Some((path, false)) => format!(
                        "已选择:{}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    Some((_, true)) => "已选择文件夹,点「打开」进入".to_owned(),
                    None => {
                        // 只统计目录项(「上一级」不算一项);有筛选时显示的是可见数量
                        let shown = visible.iter().filter(|&&i| i != PARENT_ROW).count();
                        format!("共 {shown} 个项目")
                    }
                }
            };
            ui.label(egui::RichText::new(tip).size(12.0).color(palette::soft()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match (self.purpose, self.overwrite.is_some()) {
                    (Purpose::Save, true) => "覆盖保存",
                    (Purpose::Save, false) => "保存",
                    (Purpose::PickFolder, _) => "选择此文件夹",
                    (Purpose::Open, _) => "打开",
                };
                let enabled = self.purpose != Purpose::Open || self.selected.is_some();
                let width = 96.0;
                if primary_button(ui, label, width, enabled).clicked() {
                    confirm = true;
                }
                if secondary_button(ui, "取消", 88.0).clicked() {
                    cancel = true;
                }
            });
        });

        if cancel {
            return DialogAction::Cancelled;
        }
        if confirm {
            return self.confirm();
        }
        DialogAction::None
    }
}

/// 上一级入口(根目录时为 None)
fn parent_entry(dir: &Path) -> Option<Entry> {
    filebrowser::parent_dir(dir).map(|path| Entry {
        name: "..".to_owned(),
        path,
        is_dir: true,
        size: 0,
    })
}

/// 快捷位置小按钮:选中态用浅蓝底
fn chip(ui: &mut egui::Ui, text: &str, active: bool) -> egui::Response {
    let (fill, color) = if active {
        (palette::blue_soft(), palette::blue())
    } else {
        (palette::white(), palette::muted())
    };
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.0).color(color))
            .min_size(egui::vec2(0.0, 26.0))
            .fill(fill)
            .stroke(Stroke::new(
                1.0,
                if active {
                    palette::blue()
                } else {
                    palette::line()
                },
            ))
            .corner_radius(CornerRadius::ZERO),
    )
}

fn small_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.0).color(palette::muted()))
            .min_size(egui::vec2(0.0, 26.0))
            .fill(palette::white())
            .stroke(Stroke::new(1.0, palette::line_strong()))
            .corner_radius(CornerRadius::ZERO),
    )
}

fn primary_button(ui: &mut egui::Ui, text: &str, width: f32, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).strong().color(Color32::WHITE))
            .min_size(egui::vec2(width, 34.0))
            .fill(palette::blue())
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::ZERO),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str, width: f32) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(palette::muted()))
            .min_size(egui::vec2(width, 34.0))
            .fill(palette::white())
            .stroke(Stroke::new(1.0, palette::line_strong()))
            .corner_radius(CornerRadius::ZERO),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 渲染冒烟:对话框是新写的手绘界面,布局代码里的 panic 会让整个应用卡住
    /// (用户连「取消」都点不到),所以这里无头空跑几帧兜底。
    /// 视觉效果仍需人工确认,自动化测试不覆盖外观。
    fn render(dialog: &mut FileDialog, frames: usize) {
        let ctx = egui::Context::default();
        for _ in 0..frames {
            let mut output = ctx.run_ui(Default::default(), |ctx| {
                let _ = dialog.ui(ctx);
            });
            // 无头运行不落地纹理,丢掉前必须先清空(否则 epaint 会 panic)
            output.textures_delta.clear();
        }
    }

    #[test]
    fn renders_every_mode_without_panic() {
        let mut open = FileDialog::open("选择文档", "添加文件", None, document_filters());
        render(&mut open, 3);
        assert!(open.dir().is_absolute());

        // 保存模式:带快捷键位置/文件名输入/覆盖确认的完整底部区域
        let mut save = FileDialog::save(
            "导出结果",
            "保存为文档",
            None,
            "检索结果.md",
            document_filters(),
        );
        save.overwrite = Some(save.dir().join("检索结果.md"));
        render(&mut save, 3);

        // 文件夹选择模式:只列目录,确认键可用
        let mut folder = FileDialog::pick_folder("选择文件夹", "添加文件夹", None);
        render(&mut folder, 3);

        // 空列表(筛选词匹配不到任何名字)
        let mut filtered = FileDialog::open("选择文档", "添加文件", None, document_filters());
        filtered.query = "不可能匹配到的名字".to_owned();
        render(&mut filtered, 2);

        // 起始目录不可用/不存在:建对话框时应退回主目录而不是直接失败
        let mut fallback = FileDialog::open(
            "选择文档",
            "添加文件",
            Some(PathBuf::from("/rsou-不存在的目录")),
            document_filters(),
        );
        assert!(fallback.dir().is_dir());
        render(&mut fallback, 2);
    }
}
