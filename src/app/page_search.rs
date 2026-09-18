//! 检索页:查询输入、过滤行、按文档分组的结果列表与命中预览。

use egui::text::LayoutJob;
use rsou_lib::parse::FileType;
use rsou_lib::search::{self, DocumentHit, Hit, Span};

use super::*;

/// 预览全文上限:超过后只显示命中附近窗口并提示。
const PREVIEW_MAX_BYTES: usize = 2 * 1024 * 1024;
/// 大文档预览窗口半径(命中位置前后各 100 KB)。
const PREVIEW_WINDOW_BYTES: usize = 100 * 1024;
/// 结果列与预览列的高度(外层整页已有纵向滚动,这里给内层滚动一个固定高度)。
const RESULT_PANE_HEIGHT: f32 = 620.0;

/// 全部文档类型(类型过滤的复选框顺序)。
const FILE_TYPES: [FileType; 6] = [
    FileType::Word,
    FileType::Excel,
    FileType::Ppt,
    FileType::Pdf,
    FileType::Text,
    FileType::Epub,
];

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::All => "全部",
        Scope::Title => "标题",
        Scope::Content => "正文",
    }
}

fn mtime_label(days: Option<u64>) -> &'static str {
    match days {
        None => "全部时间",
        Some(7) => "最近 7 天",
        Some(30) => "最近 30 天",
        Some(365) => "最近一年",
        Some(_) => "自定义",
    }
}

/// 高亮底色(命中段上底色)。
fn highlight_bg() -> Color32 {
    RsouApp::amber().gamma_multiply(0.3)
}

/// 把文本按高亮区间拆成 LayoutJob 段(spans 为 text 内字节区间,已排序不重叠)。
fn span_job(text: &str, base: usize, spans: &[Span], color: Color32, size: f32) -> LayoutJob {
    let normal = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color,
        ..Default::default()
    };
    let marked = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color: RsouApp::ink(),
        background: highlight_bg(),
        ..Default::default()
    };
    let end_of_text = base + text.len();
    let mut job = LayoutJob::default();
    let mut pos = 0usize;
    for span in spans {
        if span.end <= base {
            continue;
        }
        if span.start >= end_of_text {
            break;
        }
        let start = span.start.max(base) - base;
        let end = span.end.min(end_of_text) - base;
        if start > pos {
            job.append(&text[pos..start], 0.0, normal.clone());
        }
        if end > start {
            job.append(&text[start..end], 0.0, marked.clone());
        }
        pos = end.max(pos);
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, normal.clone());
    }
    if text.is_empty() {
        job.append(" ", 0.0, normal);
    }
    job
}

/// 命中批次在全文中的定位点:优先定位到该批次的首个高亮。
fn hit_offset(hit: &Hit) -> usize {
    hit.start_offset + hit.highlights.first().map(|span| span.start).unwrap_or(0)
}

/// 预览导航按文档中的阅读顺序排列,而不是按检索相关度排列。
fn preview_hit_offsets(doc_hit: &DocumentHit) -> Vec<usize> {
    let mut offsets: Vec<usize> = doc_hit.hits.iter().map(hit_offset).collect();
    offsets.sort_unstable();
    offsets
}

impl RsouApp {
    pub(crate) fn ui_page_search(&mut self, ui: &mut egui::Ui) {
        let mut want_search = false;
        let db_ready = self.db.is_some();
        let can_search = db_ready && !self.search_query.trim().is_empty();
        Self::work_panel(
            ui,
            "检索",
            "全文检索",
            "引号短语 · -词 排除 · title:/content: 限字段",
            if self.search_active {
                Some("检索中")
            } else {
                None
            },
            |ui| {
                ui.horizontal(|ui| {
                    // 不使用 `desired_width(f32::INFINITY)`:它会把输入框抢满整行,
                    // 让按钮只剩下贴在窗口边缘的一小条。按钮先占位,输入框使用剩余宽度。
                    let button_width = 72.0;
                    let input_width =
                        (ui.available_width() - button_width - ui.spacing().item_spacing.x)
                            .max(160.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .desired_width(input_width)
                            .hint_text("输入关键词,回车搜索"),
                    );
                    // 输入框持有焦点时回车触发;按按钮同样触发。
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        want_search = true;
                    }
                    if Self::primary_button(ui, "搜索", button_width, can_search).clicked() {
                        want_search = true;
                    }
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    // 无 jieba feature 时宽松模式退化为精确,开关不显示。
                    #[cfg(feature = "jieba")]
                    ui.checkbox(&mut self.search_exact, "精确");
                    egui::ComboBox::from_id_salt("search_scope")
                        .selected_text(scope_label(self.search_scope))
                        .show_ui(ui, |ui| {
                            for scope in [Scope::All, Scope::Title, Scope::Content] {
                                ui.selectable_value(
                                    &mut self.search_scope,
                                    scope,
                                    scope_label(scope),
                                );
                            }
                        });
                    ui.label(egui::RichText::new("类型:").size(13.0).color(Self::muted()));
                    for file_type in FILE_TYPES {
                        let mut checked = self.search_types.contains(file_type.as_str());
                        if ui.checkbox(&mut checked, file_type.label()).changed() {
                            if checked {
                                self.search_types.insert(file_type.as_str().to_owned());
                            } else {
                                self.search_types.remove(file_type.as_str());
                            }
                        }
                    }
                    ui.label(egui::RichText::new("目录:").size(13.0).color(Self::muted()));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search_path_prefix)
                            .desired_width(180.0)
                            .hint_text("路径前缀"),
                    );
                    ui.label(egui::RichText::new("时间:").size(13.0).color(Self::muted()));
                    egui::ComboBox::from_id_salt("search_mtime")
                        .selected_text(mtime_label(self.search_mtime_days))
                        .show_ui(ui, |ui| {
                            for days in [None, Some(7), Some(30), Some(365)] {
                                ui.selectable_value(
                                    &mut self.search_mtime_days,
                                    days,
                                    mtime_label(days),
                                );
                            }
                        });
                });
            },
        );
        if want_search {
            self.start_search(ui.ctx());
        }
        ui.add_space(13.0);

        // ---------- 状态行 ----------
        if let Some(error) = &self.search_error {
            ui.label(egui::RichText::new(error).size(13.0).color(Self::amber()));
        } else if self.search_active {
            ui.label(
                egui::RichText::new("正在检索…")
                    .size(13.0)
                    .color(Self::muted()),
            );
        } else if let Some(response) = &self.search_result {
            ui.label(
                egui::RichText::new(format!(
                    "命中 {} 篇 · {} 处 · 耗时 {:.0} ms",
                    response.total_documents, response.total_hits, response.elapsed_ms
                ))
                .size(13.0)
                .color(Self::muted()),
            );
        }
        ui.add_space(6.0);

        // ---------- 左右分栏:结果列表 | 预览 ----------
        let total_width = ui.available_width();
        if total_width >= 900.0 {
            let gap = ui.spacing().item_spacing.x;
            let left_width = (total_width * 0.52).clamp(360.0, total_width - gap - 400.0);
            let right_width = (total_width - gap - left_width).max(400.0);
            ui.horizontal_top(|ui| {
                // horizontal_top 会继承横向布局;若直接 allocate_ui,面板内部的
                // 结果片段和预览行也会被当成同一行排列。这里显式切回纵向布局。
                ui.allocate_ui_with_layout(
                    egui::vec2(left_width, RESULT_PANE_HEIGHT),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_size(egui::vec2(left_width, RESULT_PANE_HEIGHT));
                        self.ui_search_results(ui);
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(right_width, RESULT_PANE_HEIGHT),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_size(egui::vec2(right_width, RESULT_PANE_HEIGHT));
                        self.ui_search_preview(ui);
                    },
                );
            });
        } else {
            ui.allocate_ui_with_layout(
                egui::vec2(total_width, RESULT_PANE_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_size(egui::vec2(total_width, RESULT_PANE_HEIGHT));
                    self.ui_search_results(ui);
                },
            );
            ui.add_space(10.0);
            ui.allocate_ui_with_layout(
                egui::vec2(total_width, RESULT_PANE_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_size(egui::vec2(total_width, RESULT_PANE_HEIGHT));
                    self.ui_search_preview(ui);
                },
            );
        }
    }

    /// 左侧:按文档分组的命中卡片。
    fn ui_search_results(&mut self, ui: &mut egui::Ui) {
        let mut focus: Option<(i64, usize, usize)> = None;
        Self::panel_frame().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("search_result_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            let Some(response) = &self.search_result else {
                                ui.label(
                                    egui::RichText::new("输入关键词开始检索。")
                                        .size(14.0)
                                        .color(Self::muted()),
                                );
                                return;
                            };
                            if response.documents.is_empty() {
                                ui.label(
                                    egui::RichText::new(if self.search_active {
                                        "正在检索…"
                                    } else {
                                        "没有命中的文档。"
                                    })
                                    .size(14.0)
                                    .color(Self::muted()),
                                );
                                return;
                            }
                            for doc_hit in &response.documents {
                                ui_doc_card(ui, doc_hit, &mut focus);
                                ui.add_space(8.0);
                            }
                        });
                });
        });
        if let Some((doc_id, hit_index, offset)) = focus {
            self.focus_preview(doc_id, hit_index, offset);
        }
    }

    /// 右侧:命中文档的 plain_text 预览。
    fn ui_search_preview(&mut self, ui: &mut egui::Ui) {
        // 工具行需要文档元数据(路径);从最近一次结果里找。
        let document = self.preview_doc_id.and_then(|id| {
            self.search_result
                .as_ref()?
                .documents
                .iter()
                .find(|d| d.document.id == id)
                .map(|d| d.document.clone())
        });
        let literals = self
            .search_result
            .as_ref()
            .map(|r| r.compiled.literals.clone())
            .unwrap_or_default();
        let hit_offsets = self
            .preview_doc_id
            .and_then(|id| {
                self.search_result
                    .as_ref()?
                    .documents
                    .iter()
                    .find(|d| d.document.id == id)
            })
            .map(preview_hit_offsets)
            .unwrap_or_default();
        let current_hit_index = self
            .preview_hit_index
            .min(hit_offsets.len().saturating_sub(1));

        let mut action: Option<RowAction> = None;
        let mut navigation: Option<usize> = None;
        Self::panel_frame().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    // 工具行拆成标题和操作两行,避免长标题把右侧按钮挤出面板。
                    if let Some(doc) = &document {
                        let title = if doc.title.is_empty() {
                            &doc.file_name
                        } else {
                            &doc.title
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(title)
                                    .size(15.0)
                                    .strong()
                                    .color(Self::ink()),
                            )
                            .truncate(),
                        );
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if link_button(ui, "所在目录") {
                                        action = Some(RowAction::Reveal(PathBuf::from(&doc.path)));
                                    }
                                    if link_button(ui, "打开文件") {
                                        action = Some(RowAction::Open(PathBuf::from(&doc.path)));
                                    }
                                },
                            );
                        });
                        if hit_offsets.len() > 1 {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "命中批次 {} / {}",
                                        current_hit_index + 1,
                                        hit_offsets.len()
                                    ))
                                    .size(12.0)
                                    .color(Self::muted()),
                                );
                                if preview_nav_button(ui, "上一批", current_hit_index > 0) {
                                    navigation = Some(current_hit_index - 1);
                                }
                                if preview_nav_button(
                                    ui,
                                    "下一批",
                                    current_hit_index + 1 < hit_offsets.len(),
                                ) {
                                    navigation = Some(current_hit_index + 1);
                                }
                            });
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("预览")
                                .size(15.0)
                                .strong()
                                .color(Self::ink()),
                        );
                    }
                    ui.separator();

                    let Some(text) = self.preview_text.as_deref() else {
                        ui.label(
                            egui::RichText::new(if self.preview_loading {
                                "正在加载原文…"
                            } else {
                                "点击左侧文档卡片查看原文。"
                            })
                            .size(14.0)
                            .color(Self::muted()),
                        );
                        return;
                    };

                    // 大文档只渲染命中附近的窗口。
                    let (window, base) = preview_window(text, self.pending_scroll);
                    if base > 0 || window.len() < text.len() {
                        ui.label(
                            egui::RichText::new("文档过大,只显示命中附近的内容。")
                                .size(12.0)
                                .color(Self::amber()),
                        );
                        ui.add_space(4.0);
                    }
                    let spans = search::locate_literals(window, &literals);
                    let target = self
                        .pending_scroll
                        .map(|offset| offset.saturating_sub(base));

                    let mut scrolled = false;
                    egui::ScrollArea::vertical()
                        .id_salt("preview_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let mut offset = 0usize;
                            let mut span_idx = 0usize;
                            for line in window.split('\n') {
                                let line_start = offset;
                                let line_end = offset + line.len();
                                while span_idx < spans.len() && spans[span_idx].end <= line_start {
                                    span_idx += 1;
                                }
                                let job = span_job(
                                    line,
                                    line_start,
                                    &spans[span_idx..],
                                    Self::ink(),
                                    13.0,
                                );
                                let response = ui.add(egui::Label::new(job).wrap());
                                if !scrolled
                                    && let Some(target) = target
                                    && line_end >= target
                                {
                                    // 不指定对齐方式:命中已在当前可视区域时不移动,
                                    // 只有目标不在预览窗格内才滚动到目标位置。
                                    response.scroll_to_me(None);
                                    scrolled = true;
                                }
                                offset = line_end + 1;
                            }
                        });
                    if scrolled {
                        self.pending_scroll = None;
                    }
                });
        });
        if let Some(hit_index) = navigation
            && let Some(document_id) = document.as_ref().map(|doc| doc.id)
            && let Some(&offset) = hit_offsets.get(hit_index)
        {
            self.focus_preview(document_id, hit_index, offset);
        }
        let action_result = match action {
            Some(RowAction::Open(path)) => Some(platform::open_path(&path)),
            Some(RowAction::Reveal(path)) => Some(platform::reveal_in_folder(&path)),
            None => None,
        };
        if let Some(Err(error)) = action_result {
            self.search_error = Some(error);
        }
    }
}

/// 行内操作意图(复用资料库页的枚举思路)。
enum RowAction {
    Open(PathBuf),
    Reveal(PathBuf),
}

/// 计算预览窗口:超过 PREVIEW_MAX_BYTES 时以滚动目标为中心取 ±PREVIEW_WINDOW_BYTES。
/// 返回 (窗口文本, 窗口起点在原文字节偏移)。边界取字符边界。
fn preview_window(text: &str, target: Option<usize>) -> (&str, usize) {
    if text.len() <= PREVIEW_MAX_BYTES {
        return (text, 0);
    }
    let center = target.unwrap_or(0).min(text.len());
    let mut start = center.saturating_sub(PREVIEW_WINDOW_BYTES);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (center + PREVIEW_WINDOW_BYTES).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    (&text[start..end], start)
}

/// 一篇文档的命中卡片:标题(标题命中上底色)+ 路径 + 命中数。
/// 整张卡片可点击,命中内容统一在右侧预览区显示。
fn ui_doc_card(ui: &mut egui::Ui, doc_hit: &DocumentHit, focus: &mut Option<(i64, usize, usize)>) {
    let doc = &doc_hit.document;
    let first_hit_offset = preview_hit_offsets(doc_hit).first().copied().unwrap_or(0);
    let card = RsouApp::card_frame(RsouApp::surface(), RsouApp::line(), 12).show(ui, |ui| {
        // 让可点击区域铺满结果列,点击卡片的空白处也能切换预览。
        ui.set_min_width(ui.available_width());
        // 标题行:标题命中上底色 + 类型徽标 + 命中数
        ui.horizontal_wrapped(|ui| {
            let title = if doc.title.is_empty() {
                doc.file_name.as_str()
            } else {
                doc.title.as_str()
            };
            ui.add(egui::Label::new(span_job(
                title,
                0,
                &doc_hit.title_highlights,
                RsouApp::ink(),
                15.0,
            )));
            ui.label(
                egui::RichText::new(&doc.ext)
                    .size(11.0)
                    .color(RsouApp::teal()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("命中 {} 处", doc_hit.total_hits))
                        .size(12.0)
                        .color(RsouApp::muted()),
                );
            });
        });
        ui.add(
            egui::Label::new(
                egui::RichText::new(&doc.path)
                    .size(11.0)
                    .color(RsouApp::soft()),
            )
            .truncate(),
        );
    });
    let response = ui.interact(
        card.response.rect,
        ui.id().with(("search-document-card", doc.id)),
        egui::Sense::click(),
    );
    if response.clicked() {
        *focus = Some((doc.id, 0, first_hit_offset));
    }
}

/// 预览命中导航按钮:没有上一批/下一批时禁用,避免越界后循环跳转。
fn preview_nav_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).size(12.0)).min_size(egui::vec2(72.0, 30.0)),
    )
    .clicked()
}

/// 行内文字按钮(与资料库页同款,避免跨模块私有依赖重复定义)。
fn link_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.0).color(RsouApp::blue())).frame(false),
    )
    .clicked()
}
