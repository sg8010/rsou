//! 检索页:查询输入、过滤行、按文档分组的结果列表与命中预览。

use egui::text::LayoutJob;
use rsou_lib::parse::FileType;
use rsou_lib::search::{self, DocumentHit, Hit, Span};

use super::theme::Icon;
use super::*;

/// 预览全文上限:超过后只显示命中附近窗口并提示。
const PREVIEW_MAX_BYTES: usize = 2 * 1024 * 1024;
/// 大文档预览窗口半径(命中位置前后各 100 KB)。
const PREVIEW_WINDOW_BYTES: usize = 100 * 1024;
/// 结果列与预览列的高度(外层整页已有纵向滚动,这里给内层滚动一个固定高度)。
const RESULT_PANE_HEIGHT: f32 = 620.0;
/// 左右分栏阈值:低于该宽度时结果/预览上下排。
const TWO_PANE_MIN_WIDTH: f32 = 900.0;
/// 搜索埋点暂时显示在状态行下,避免占满宽窗口。
const SEARCH_DIAGNOSTICS_MAX_WIDTH: f32 = 800.0;

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

/// 把文本按高亮区间拆成 LayoutJob 段(spans 为 text 内字节区间,已排序不重叠)。
fn span_job(text: &str, base: usize, spans: &[Span], color: Color32, size: f32) -> LayoutJob {
    let normal = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color,
        ..Default::default()
    };
    let marked = egui::TextFormat {
        font_id: egui::FontId::proportional(size),
        color: RsouApp::highlight_text(),
        background: RsouApp::highlight_bg(),
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
        Self::page_header(ui, self.page.title(), self.page.description());
        ui.add_space(Self::HEADER_TO_CONTENT);

        // ---------- 搜索卡:输入行 + 两行过滤 ----------
        let mut want_search = false;
        let db_ready = self.db.is_some();
        let can_search = db_ready && !self.search_query.trim().is_empty();
        Self::card(ui, |ui| {
            // 第一行:搜索输入 + 搜索按钮 + 精确/宽松开关 + 范围 + 时间。
            ui.horizontal_wrapped(|ui| {
                // 输入框不再抢满整行:给同行其余控件预留估算宽度后取剩余,
                // 并封顶避免过宽;窄窗口下收缩,放不下时行内控件自动换行。
                let reserved = 500.0;
                let input_width = (ui.available_width() - reserved).clamp(180.0, 340.0);
                let response = Self::clearable_input(
                    ui,
                    &mut self.search_query,
                    input_width,
                    "输入关键词,回车搜索",
                );
                // 输入框持有焦点时回车触发;按按钮同样触发。
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    want_search = true;
                }
                if Self::primary_button(ui, Some(Icon::Search), "搜索", 96.0, can_search).clicked()
                {
                    want_search = true;
                }
                ui.add_space(8.0);
                // 无 jieba feature 时宽松模式退化为精确,开关不显示。
                #[cfg(feature = "jieba")]
                {
                    Self::mode_switch(ui, &mut self.search_exact, "精确", "宽松");
                    ui.add_space(8.0);
                }
                ui.label(
                    egui::RichText::new("范围:")
                        .size(13.0)
                        .color(Self::text_secondary()),
                );
                egui::ComboBox::from_id_salt("search_scope")
                    .selected_text(scope_label(self.search_scope))
                    .show_ui(ui, |ui| {
                        for scope in [Scope::All, Scope::Title, Scope::Content] {
                            ui.selectable_value(&mut self.search_scope, scope, scope_label(scope));
                        }
                    });
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("时间:")
                        .size(13.0)
                        .color(Self::text_secondary()),
                );
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
                if self.search_active {
                    ui.add_space(8.0);
                    Self::status_badge(
                        ui,
                        Some(Icon::Loader),
                        "检索中",
                        Self::accent_soft(),
                        Self::accent(),
                    );
                }
            });
            ui.add_space(10.0);
            // 第二行:目录 + 搜索文件类型。
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("目录:")
                        .size(13.0)
                        .color(Self::text_secondary()),
                );
                Self::clearable_input(ui, &mut self.search_path_prefix, 220.0, "路径前缀");
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("搜索文件类型:")
                        .size(13.0)
                        .color(Self::text_secondary()),
                );
                for file_type in FILE_TYPES {
                    let mut checked = self.search_types.contains(file_type.as_str());
                    if Self::accent_checkbox(ui, &mut checked, file_type.label()).changed() {
                        if checked {
                            self.search_types.insert(file_type.as_str().to_owned());
                        } else {
                            self.search_types.remove(file_type.as_str());
                        }
                    }
                }
            });
        });
        if want_search {
            self.start_search(ui.ctx());
        }
        ui.add_space(Self::SECTION_GAP);

        // ---------- 状态行 ----------
        if let Some(error) = &self.search_error {
            ui.label(egui::RichText::new(error).size(13.0).color(Self::danger()));
        } else if self.search_active {
            ui.label(
                egui::RichText::new("正在检索…")
                    .size(13.0)
                    .color(Self::text_secondary()),
            );
        } else if let Some(response) = &self.search_result {
            let summary = format!(
                "展示 {} 组结果，共 {} 个文件位置 · 耗时 {:.0} ms{}",
                response.representatives().count(),
                response.documents.len(),
                response.elapsed_ms,
                if response.total_documents > response.documents.len() {
                    format!(
                        "（FTS 候选 {} 组、{} 个位置，部分未展示或未通过精确匹配）",
                        response.total_groups, response.total_documents
                    )
                } else {
                    String::new()
                }
            );
            ui.label(
                egui::RichText::new(summary)
                    .size(13.0)
                    .color(Self::text_secondary()),
            );
            let diagnostics = &response.diagnostics;
            ui.scope(|ui| {
                ui.set_max_width(SEARCH_DIAGNOSTICS_MAX_WIDTH);
                let details = format!(
                    "阶段：编译 {:.1} ms · FTS 计数 {:.1} ms · FTS 排名/候选 {:.1} ms · 元数据 {:.1} ms · 正文读取 {:.1} ms · 命中定位 {:.1} ms · 分块读取 {:.1} ms · 片段生成 {:.1} ms · 收尾 {:.1} ms\n规模：FTS {} 篇 / {} 组 · 候选 {} 组 / {} 个位置 · 正文 {} 字节 · 分块 {} · 命中区间 {}",
                    diagnostics.compile_ms,
                    diagnostics.fts_count_ms,
                    diagnostics.fts_candidates_ms,
                    diagnostics.metadata_ms,
                    diagnostics.load_plain_text_ms,
                    diagnostics.locate_literals_ms,
                    diagnostics.load_chunks_ms,
                    diagnostics.group_fragments_ms,
                    diagnostics.finalize_ms,
                    diagnostics.fts_documents,
                    diagnostics.fts_groups,
                    diagnostics.candidate_groups,
                    diagnostics.candidate_locations,
                    diagnostics.plain_text_bytes,
                    diagnostics.chunk_count,
                    diagnostics.literal_spans,
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(details)
                            .size(11.0)
                            .color(Self::text_secondary()),
                    )
                    .wrap(),
                );
            });
        }
        ui.add_space(8.0);

        // ---------- 左右分栏:结果列表 | 预览 ----------
        let total_width = ui.available_width();
        if total_width >= TWO_PANE_MIN_WIDTH {
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
            ui.add_space(Self::SECTION_GAP);
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
        let mut open_locations = None;
        Self::card_frame().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("search_result_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            let Some(response) = &self.search_result else {
                                Self::empty_note(ui, "输入关键词开始检索。");
                                return;
                            };
                            if response.documents.is_empty() {
                                Self::empty_note(
                                    ui,
                                    if self.search_active {
                                        "正在检索…"
                                    } else {
                                        "没有命中的文档。"
                                    },
                                );
                                return;
                            }
                            for representative in response.representatives() {
                                let locations: Vec<_> =
                                    response.locations(representative.group_id).collect();
                                let doc_hit = self
                                    .search_locations
                                    .get(&representative.group_id)
                                    .and_then(|id| {
                                        locations.iter().find(|hit| hit.document.id == *id).copied()
                                    })
                                    .unwrap_or(representative);
                                let selected = self.preview_doc_id == Some(doc_hit.document.id);
                                ui_doc_card(
                                    ui,
                                    doc_hit,
                                    locations.len(),
                                    selected,
                                    &mut focus,
                                    &mut open_locations,
                                );
                                ui.add_space(8.0);
                            }
                        });
                });
        });
        if let Some((doc_id, hit_index, offset)) = focus {
            self.focus_preview(doc_id, hit_index, offset);
        }
        if let Some(group_id) = open_locations {
            self.location_popup_group = Some(group_id);
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
        let mut switch_location = None;
        Self::card_frame().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    // 标题、路径和操作各占一行,长路径换行后也不会侵占按钮空间。
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
                                    .color(Self::text_primary()),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&doc.path)
                                    .size(11.0)
                                    .color(Self::text_muted()),
                            )
                            .wrap(),
                        );
                        ui.horizontal(|ui| {
                            if let Some(response) = &self.search_result
                                && let Some(current) = response
                                    .documents
                                    .iter()
                                    .find(|hit| hit.document.id == doc.id)
                            {
                                let locations: Vec<_> =
                                    response.locations(current.group_id).collect();
                                if locations.len() > 1 {
                                    let mut open =
                                        self.location_popup_group == Some(current.group_id);
                                    let trigger =
                                        ui.button(format!("文件位置 · {} ▾", locations.len()));
                                    if trigger.clicked() {
                                        open = !open;
                                    }
                                    let width = (ui.ctx().content_rect().width() - 48.0)
                                        .clamp(160.0, 520.0);
                                    egui::Popup::from_response(&trigger)
                                        .id(egui::Id::new(("search_locations", current.group_id)))
                                        .open_bool(&mut open)
                                        .close_behavior(
                                            egui::PopupCloseBehavior::CloseOnClickOutside,
                                        )
                                        .width(width)
                                        .show(|ui| {
                                            ui.label("选择文件位置");
                                            ui.separator();
                                            egui::ScrollArea::vertical()
                                                .id_salt(("location_list", current.group_id))
                                                .max_height(240.0)
                                                .show(ui, |ui| {
                                                    for location in locations {
                                                        let selected =
                                                            location.document.id == doc.id;
                                                        let label = if selected {
                                                            format!(
                                                                "✓ {}\n当前位置",
                                                                location.document.path
                                                            )
                                                        } else {
                                                            location.document.path.clone()
                                                        };
                                                        if ui
                                                            .add_sized(
                                                                [ui.available_width(), 0.0],
                                                                egui::Button::new(label)
                                                                    .selected(selected)
                                                                    .wrap(),
                                                            )
                                                            .clicked()
                                                        {
                                                            let offset = location
                                                                .hits
                                                                .first()
                                                                .map(hit_offset)
                                                                .unwrap_or(0);
                                                            switch_location = Some((
                                                                location.document.id,
                                                                offset,
                                                            ));
                                                            ui.close();
                                                        }
                                                    }
                                                });
                                            ui.separator();
                                            ui.weak("点击路径切换 · Esc 关闭");
                                        });
                                    self.location_popup_group = open.then_some(current.group_id);
                                }
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if Self::link_button(ui, "所在目录").clicked() {
                                        action = Some(RowAction::Reveal(PathBuf::from(&doc.path)));
                                    }
                                    if Self::link_button(ui, "打开文件").clicked() {
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
                                    .color(Self::text_secondary()),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if Self::small_secondary_button(
                                            ui,
                                            None,
                                            "下一批",
                                            64.0,
                                            current_hit_index + 1 < hit_offsets.len(),
                                        )
                                        .clicked()
                                        {
                                            navigation = Some(current_hit_index + 1);
                                        }
                                        if Self::small_secondary_button(
                                            ui,
                                            None,
                                            "上一批",
                                            64.0,
                                            current_hit_index > 0,
                                        )
                                        .clicked()
                                        {
                                            navigation = Some(current_hit_index - 1);
                                        }
                                    },
                                );
                            });
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("预览")
                                .size(15.0)
                                .strong()
                                .color(Self::text_primary()),
                        );
                    }
                    Self::thin_divider(ui);

                    let Some(text) = self.preview_text.as_deref() else {
                        Self::empty_note(
                            ui,
                            if self.preview_loading {
                                "正在加载原文…"
                            } else {
                                "点击左侧文档卡片查看原文。"
                            },
                        );
                        return;
                    };

                    // 大文档只渲染命中附近的窗口(锚点是稳定的命中偏移,
                    // 滚动请求清除后窗口不跳回文首)。
                    let (window, base) = preview_window(text, self.preview_anchor);
                    if base > 0 || window.len() < text.len() {
                        Self::warn_banner(ui, "文档过大,只显示命中附近的内容。");
                        ui.add_space(6.0);
                    }
                    // 高亮定位缓存:同一预览文本同一窗口、同一组字面量不重算。
                    let literals: &[String] = self
                        .search_result
                        .as_ref()
                        .map(|r| r.compiled.literals.as_slice())
                        .unwrap_or(&[]);
                    let cache_hit = self.preview_spans_cache.as_ref().is_some_and(|cache| {
                        cache.preview_gen == self.preview_gen
                            && cache.base == base
                            && cache.window_len == window.len()
                            && cache.literals == literals
                    });
                    if !cache_hit {
                        self.preview_spans_cache = Some(PreviewSpanCache {
                            preview_gen: self.preview_gen,
                            base,
                            window_len: window.len(),
                            literals: literals.to_vec(),
                            spans: search::locate_literals(window, literals),
                        });
                    }
                    let spans: &[Span] = self
                        .preview_spans_cache
                        .as_ref()
                        .map(|cache| cache.spans.as_slice())
                        .unwrap_or(&[]);
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
                                    Self::text_primary(),
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
        if let Some((id, offset)) = switch_location {
            self.location_popup_group = None;
            self.focus_preview(id, 0, offset);
        }
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

/// 计算预览窗口:超过 PREVIEW_MAX_BYTES 时以锚点(命中偏移)为中心取 ±PREVIEW_WINDOW_BYTES。
/// 返回 (窗口文本, 窗口起点在原文字节偏移)。边界取字符边界。
fn preview_window(text: &str, anchor: usize) -> (&str, usize) {
    if text.len() <= PREVIEW_MAX_BYTES {
        return (text, 0);
    }
    let center = anchor.min(text.len());
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

/// 一篇文档的命中卡片:类型图标 + 标题(命中上底色)+ 路径 + 命中数。
/// 整张卡片可点击,命中内容统一在右侧预览区显示;当前预览的文档带选中态。
fn ui_doc_card(
    ui: &mut egui::Ui,
    doc_hit: &DocumentHit,
    location_count: usize,
    selected: bool,
    focus: &mut Option<(i64, usize, usize)>,
    open_locations: &mut Option<i64>,
) {
    let doc = &doc_hit.document;
    let first_hit_offset = doc_hit.hits.iter().map(hit_offset).min().unwrap_or(0);
    let (icon, icon_color) = RsouApp::file_type_look(&doc.file_type);
    let (fill, stroke) = if selected {
        (
            RsouApp::selected_soft(),
            Stroke::new(1.0, RsouApp::accent()),
        )
    } else {
        (RsouApp::surface(), Stroke::new(1.0, RsouApp::border()))
    };
    let card = egui::Frame::new()
        .inner_margin(egui::Margin::same(12))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .show(ui, |ui| {
            // 让可点击区域铺满结果列,点击卡片的空白处也能切换预览。
            ui.set_min_width(ui.available_width());
            ui.horizontal_top(|ui| {
                // 文件类型图标(按类型着色)。
                let (icon_rect, _) =
                    ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                RsouApp::paint_icon(ui.painter(), icon, icon_rect, icon_color);
                ui.vertical(|ui| {
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
                            RsouApp::text_primary(),
                            14.0,
                        )));
                        ui.label(egui::RichText::new(&doc.ext).size(11.0).color(icon_color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!("命中 {} 处", doc_hit.total_hits))
                                    .size(12.0)
                                    .color(RsouApp::text_muted()),
                            );
                        });
                    });
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&doc.path)
                                .size(11.0)
                                .color(RsouApp::text_muted()),
                        )
                        .truncate(),
                    );
                    if location_count > 1
                        && RsouApp::link_button(ui, &format!("相同内容 · {location_count} 个位置"))
                            .clicked()
                    {
                        *focus = Some((doc.id, 0, first_hit_offset));
                        *open_locations = Some(doc_hit.group_id);
                    }
                });
            });
        });
    let response = ui.interact(
        card.response.rect,
        ui.id().with(("search-document-card", doc.id)),
        egui::Sense::click(),
    );
    // hover 态:非选中卡片悬停时描一道浅蓝边(选中态已有蓝底蓝边)。
    if response.hovered() && !selected {
        ui.painter().rect_stroke(
            card.response.rect,
            CornerRadius::same(8),
            Stroke::new(1.0, RsouApp::accent_border()),
            egui::StrokeKind::Inside,
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        *focus = Some((doc.id, 0, first_hit_offset));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_window_short_text_returns_whole() {
        let text = "短文本,不超过上限。";
        assert_eq!(preview_window(text, 0), (text, 0));
        // 锚点越界也不影响短文本
        assert_eq!(preview_window(text, usize::MAX), (text, 0));
    }

    #[test]
    fn preview_window_centers_on_anchor() {
        let text = "a".repeat(PREVIEW_MAX_BYTES + 4 * PREVIEW_WINDOW_BYTES);
        let anchor = text.len() / 2 + 123;
        let (window, base) = preview_window(&text, anchor);
        assert!(base > 0);
        assert!(base <= anchor);
        assert!(anchor <= base + window.len());
        assert!(window.len() <= 2 * PREVIEW_WINDOW_BYTES);
    }

    #[test]
    fn preview_window_same_anchor_is_stable() {
        // 滚动请求清除前后(同一锚点)窗口必须一致,不能跳回文首。
        let text = "a".repeat(PREVIEW_MAX_BYTES + 4 * PREVIEW_WINDOW_BYTES);
        let anchor = text.len() / 2;
        assert_eq!(preview_window(&text, anchor), preview_window(&text, anchor));
    }

    #[test]
    fn preview_window_stays_on_char_boundaries() {
        // 多字节字符:窗口两端都必须落在字符边界上(切片不能 panic)。
        let text = "文".repeat(PREVIEW_MAX_BYTES + 4 * PREVIEW_WINDOW_BYTES);
        let anchor = text.len() / 2 + 1;
        let (window, base) = preview_window(&text, anchor);
        assert!(text.is_char_boundary(base));
        assert!(text.is_char_boundary(base + window.len()));
        assert!(base <= anchor);
        assert!(anchor <= base + window.len());
    }
}
