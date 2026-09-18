//! 检索:FTS5 查询、二次精确过滤、按文档聚合与高亮区间。
//!
//! 关键点(见 docs/plan.md §6.3/§6.4):
//! - 单条 SQL 取前 1000 行,`highlight(chunks_fts, N, char(1), char(2))` 的标记
//!   直接落在原文字节上(自建 tokenizer 上报原文区间,无需坐标映射);
//! - `highlight()` 会把同一短语的相邻词元合并成一个区间,区间文本可能含
//!   空白或标点(逐字索引下 `文、档` 也会被短语 `"文档"` 命中),所以必须有
//!   二次精确过滤:区间文本剔除空白后 **包含** 任一查询字面量才保留;
//! - 按 document_id 保序聚合(行序 = rank 序),保留每篇的全部命中片段。

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Context;
use rusqlite::{Connection, params_from_iter};

use crate::query::{self, CompiledQuery, Scope};
use crate::repo::{self, DocumentRow};

/// 检索过滤条件(都可选)。
#[derive(Debug, Default, Clone)]
pub struct Filters {
    /// 限定文档类型(file_type 取值:word/excel/ppt/pdf/text/epub);空 = 不限
    pub file_types: Vec<String>,
    pub mtime_from_ms: Option<i64>,
    pub mtime_to_ms: Option<i64>,
    /// 规范化路径前缀(目录过滤)
    pub path_prefix: Option<String>,
}

/// 一次检索的全部输入。
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub scope: Scope,
    /// true = 宽松模式(jieba 切词);无 jieba feature 时退化为精确
    pub loose: bool,
    pub filters: Filters,
    /// 返回文档数上限(总命中文档数仍记真实值)
    pub max_documents: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: Scope::All,
            loose: false,
            filters: Filters::default(),
            max_documents: 100,
        }
    }
}

/// 字节区间(content 内 / 列文本内)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// 单个分块命中(一条片段)。
#[derive(Debug, Clone)]
pub struct Hit {
    pub chunk_id: i64,
    /// 标题路径(已剥标记)
    pub context_header: String,
    /// chunk 在 plain_text 中的字节区间
    pub start_offset: usize,
    pub end_offset: usize,
    /// chunk 原文(已剥标记)
    pub content: String,
    /// content 内的高亮区间(过滤后)
    pub highlights: Vec<Span>,
    /// context_header 内的高亮区间(过滤后)
    pub header_highlights: Vec<Span>,
    /// bm25 分数(越小越相关)
    pub rank: f64,
}

/// 一篇文档的聚合结果。
#[derive(Debug)]
pub struct DocumentHit {
    pub document: DocumentRow,
    /// 文档标题内的高亮区间(取该文档首个命中行的 title 列)
    pub title_highlights: Vec<Span>,
    /// 全部命中片段,按 rank
    pub hits: Vec<Hit>,
    /// 该文档过滤后的命中分块总数(与 hits.len() 一致)
    pub total_hits: usize,
    pub best_rank: f64,
}

/// 检索结果。
#[derive(Debug)]
pub struct SearchResponse {
    pub documents: Vec<DocumentHit>,
    /// 过滤后的分块命中总数
    pub total_hits: usize,
    /// 过滤后的命中文档总数(未按 max_documents 截断)
    pub total_documents: usize,
    pub elapsed_ms: f64,
    /// 编译产物(literals 供预览定位)
    pub compiled: CompiledQuery,
}

/// highlight() 输出剥标记:返回原文与各命中区间(字节,相对于返回文本)。
pub fn highlight_spans(marked: &str, open: char, close: char) -> (String, Vec<Span>) {
    let mut text = String::with_capacity(marked.len());
    let mut spans = Vec::new();
    let mut open_at: Option<usize> = None;
    for ch in marked.chars() {
        if ch == open {
            open_at = Some(text.len());
        } else if ch == close {
            if let Some(start) = open_at.take() {
                spans.push(Span {
                    start,
                    end: text.len(),
                });
            }
        } else {
            text.push(ch);
        }
    }
    (text, spans)
}

/// 剔除 Unicode 空白并按 ASCII 小写归一(二次过滤的比对形态)。
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 一组字面量的比对形态:逐个 squash,丢弃空结果(调用方一次预计算、逐区间复用)。
pub fn squash_literals(literals: &[String]) -> Vec<String> {
    literals
        .iter()
        .map(|literal| squash(literal))
        .filter(|needle| !needle.is_empty())
        .collect()
}

/// 二次精确过滤:区间文本剔除空白后包含任一已归一化字面量即保留。
///
/// `needles` 必须已是 `squash_literals` 的产物(剔空白 + ASCII 小写)。
/// 用 contains 而不是相等:highlight() 会把相邻/重叠短语合并成一个区间,
/// 区间可能比单个字面量长。标点不剔除,所以 `文、档` 会被丢弃。
pub fn accept_span(text: &str, span: &Span, needles: &[String]) -> bool {
    let Some(slice) = text.get(span.start..span.end) else {
        return false;
    };
    let hay = squash(slice);
    needles.iter().any(|needle| hay.contains(needle))
}

/// 预览用:在全文里定位所有字面量(ASCII 大小写不敏感子串),合并重叠区间。
pub fn locate_literals(text: &str, literals: &[String]) -> Vec<Span> {
    let hay = text.to_ascii_lowercase();
    let mut spans: Vec<Span> = Vec::new();
    for literal in literals {
        let needle = literal.to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        for (start, part) in hay.match_indices(&needle) {
            spans.push(Span {
                start,
                end: start + part.len(),
            });
        }
    }
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => {
                last.end = last.end.max(span.end);
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// 文档的 plain_text(预览用);没有内容时返回 None。
pub fn plain_text_for_preview(
    conn: &Connection,
    document_id: i64,
) -> anyhow::Result<Option<String>> {
    repo::get_plain_text(conn, document_id)
}

const ROW_LIMIT: usize = 1000;

/// 执行检索:编译查询 → MATCH → 过滤/聚合 → 文档元数据回填。
///
/// 语法错误把 `QueryError` 原样上抛(GUI 直接显示其中文文案)。
pub fn search(conn: &Connection, request: &SearchRequest) -> anyhow::Result<SearchResponse> {
    let started = Instant::now();
    let compiled =
        query::compile(&request.query, request.scope, request.loose).map_err(anyhow::Error::new)?;
    // 字面量归一化只做一次,逐区间复用。
    let needles = squash_literals(&compiled.literals);

    let mut sql = String::from(
        "SELECT c.id, c.document_id, c.context_header, c.start_offset, c.end_offset, \
         highlight(chunks_fts, 0, char(1), char(2)), \
         highlight(chunks_fts, 1, char(1), char(2)), \
         highlight(chunks_fts, 2, char(1), char(2)), \
         bm25(chunks_fts, 5.0, 2.0, 1.0) AS rank \
         FROM chunks_fts \
         JOIN chunks c ON c.id = chunks_fts.rowid \
         JOIN documents d ON d.id = c.document_id \
         WHERE chunks_fts MATCH ?1",
    );
    let mut params: Vec<rusqlite::types::Value> = vec![compiled.match_expr.clone().into()];

    if !request.filters.file_types.is_empty() {
        let marks = std::iter::repeat_n("?", request.filters.file_types.len())
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" AND d.file_type IN ({marks})"));
        for file_type in &request.filters.file_types {
            params.push(file_type.clone().into());
        }
    }
    if let Some(from) = request.filters.mtime_from_ms {
        sql.push_str(" AND d.file_mtime_ms >= ?");
        params.push(from.into());
    }
    if let Some(to) = request.filters.mtime_to_ms {
        sql.push_str(" AND d.file_mtime_ms <= ?");
        params.push(to.into());
    }
    if let Some(prefix) = &request.filters.path_prefix {
        sql.push_str(" AND d.path LIKE ? ESCAPE '\\'");
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        params.push(format!("{escaped}%").into());
    }
    sql.push_str(&format!(
        " ORDER BY rank ASC, chunks_fts.rowid ASC LIMIT {ROW_LIMIT}"
    ));

    let mut stmt = conn.prepare(&sql).context("准备检索语句失败")?;
    let mut rows = stmt
        .query(params_from_iter(params))
        .context("执行检索失败")?;

    struct DocAcc {
        title_highlights: Vec<Span>,
        hits: Vec<Hit>,
        total_hits: usize,
        best_rank: f64,
    }

    let mut doc_order: Vec<i64> = Vec::new();
    let mut docs: HashMap<i64, DocAcc> = HashMap::new();
    let mut total_hits = 0usize;

    while let Some(row) = rows.next().context("读取检索结果失败")? {
        let chunk_id: i64 = row.get(0)?;
        let document_id: i64 = row.get(1)?;
        let context_header: String = row.get(2)?;
        let start_offset: i64 = row.get(3)?;
        let end_offset: i64 = row.get(4)?;
        // highlight 列序与 FTS 列序一致:0=title、1=context_header、2=content。
        let marked_title: String = row.get(5)?;
        let marked_header: String = row.get(6)?;
        let marked_content: String = row.get(7)?;
        let rank: f64 = row.get(8)?;

        let (content, content_spans) = highlight_spans(&marked_content, '\u{1}', '\u{2}');
        let (header, header_spans) = highlight_spans(&marked_header, '\u{1}', '\u{2}');
        let (title_text, title_spans) = highlight_spans(&marked_title, '\u{1}', '\u{2}');

        let highlights: Vec<Span> = content_spans
            .iter()
            .filter(|span| accept_span(&content, span, &needles))
            .copied()
            .collect();
        let header_highlights: Vec<Span> = header_spans
            .iter()
            .filter(|span| accept_span(&header, span, &needles))
            .copied()
            .collect();
        let title_highlights: Vec<Span> = title_spans
            .iter()
            .filter(|span| accept_span(&title_text, span, &needles))
            .copied()
            .collect();
        if highlights.is_empty() && header_highlights.is_empty() && title_highlights.is_empty() {
            // 三列过滤后全空:误配行(如 文、档),整行丢弃。
            continue;
        }

        total_hits += 1;
        let acc = docs.entry(document_id).or_insert_with(|| {
            doc_order.push(document_id);
            DocAcc {
                title_highlights: title_highlights.clone(),
                hits: Vec::new(),
                total_hits: 0,
                best_rank: rank,
            }
        });
        acc.total_hits += 1;
        let start_offset = start_offset.max(0) as usize;
        acc.hits.push(Hit {
            chunk_id,
            context_header,
            start_offset,
            end_offset: end_offset.max(0) as usize,
            content,
            highlights,
            header_highlights,
            rank,
        });
    }
    drop(rows);
    drop(stmt);

    let total_documents = docs.len();
    let kept_ids: Vec<i64> = doc_order.into_iter().take(request.max_documents).collect();
    let meta = repo::get_documents_by_ids(conn, &kept_ids).context("读取文档元数据失败")?;

    let mut documents = Vec::with_capacity(kept_ids.len());
    for id in kept_ids {
        let Some(acc) = docs.remove(&id) else {
            continue;
        };
        let Some(document) = meta.get(&id) else {
            // 行还在 chunks 里但 documents 已被并发删掉——跳过。
            continue;
        };
        documents.push(DocumentHit {
            document: document.clone(),
            title_highlights: acc.title_highlights,
            hits: acc.hits,
            total_hits: acc.total_hits,
            best_rank: acc.best_rank,
        });
    }

    Ok(SearchResponse {
        documents,
        total_hits,
        total_documents,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        compiled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{self, Chunk};
    use crate::parse::FileType;
    use crate::repo::{FileMeta, ParsedDocument};
    use crate::text;

    fn save_doc(conn: &mut Connection, path: &str, file_type: FileType, markdown: &str) -> i64 {
        let meta = FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: path.rsplit('.').next().unwrap_or("").to_owned(),
            file_type,
            file_size: markdown.len() as u64,
            file_mtime_ms: 1_000,
        };
        let plain = text::markdown_to_plain(markdown);
        let title = text::extract_title(&plain, &meta.stem());
        let chunks = chunk::chunk_document(&title, &plain);
        let parsed = ParsedDocument {
            title,
            markdown: markdown.to_owned(),
            plain,
            chunks,
            warnings: Vec::new(),
            parser_name: "text",
            parser_version: "text.v1",
        };
        repo::save_parsed(conn, &meta, "hash", &parsed, 1_000).unwrap()
    }

    /// 手工指定 plain/chunks 的文档(控制每篇的 chunk 数)。
    fn save_doc_chunks(
        conn: &mut Connection,
        path: &str,
        file_type: FileType,
        pieces: &[&str],
    ) -> i64 {
        let mut plain = crate::text::PlainText::default();
        let mut chunks = Vec::new();
        for (index, piece) in pieces.iter().enumerate() {
            if index > 0 {
                plain.text.push_str("\n\n");
            }
            let start = plain.text.len();
            plain.text.push_str(piece);
            chunks.push(Chunk {
                index,
                context_header: String::new(),
                start,
                end: plain.text.len(),
            });
        }
        let meta = FileMeta {
            path: path.into(),
            canonical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            ext: path.rsplit('.').next().unwrap_or("").to_owned(),
            file_type,
            file_size: 0,
            file_mtime_ms: 1_000,
        };
        let parsed = ParsedDocument {
            title: meta.stem(),
            markdown: String::new(),
            plain,
            chunks,
            warnings: Vec::new(),
            parser_name: "text",
            parser_version: "text.v1",
        };
        repo::save_parsed(conn, &meta, "hash", &parsed, 1_000).unwrap()
    }

    fn request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.to_owned(),
            ..SearchRequest::default()
        }
    }

    fn hit_paths(response: &SearchResponse) -> Vec<String> {
        response
            .documents
            .iter()
            .map(|d| d.document.path.clone())
            .collect()
    }

    #[test]
    fn whitespace_insensitive_but_punctuation_rejected() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/空格.txt", FileType::Text, "文 档");
        save_doc(&mut conn, "/d/换行.txt", FileType::Text, "文\n档");
        save_doc(&mut conn, "/d/顿号.txt", FileType::Text, "文、档");
        save_doc(&mut conn, "/d/连写.txt", FileType::Text, "文档");
        let response = search(&conn, &request("文档")).unwrap();
        let paths = hit_paths(&response);
        assert_eq!(response.total_documents, 3, "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("顿号")));
        assert_eq!(response.total_hits, 3);
    }

    #[test]
    fn ascii_case_insensitive_highlight_is_exact() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(
            &mut conn,
            "/d/a.txt",
            FileType::Text,
            "合同编号 A4 其它内容",
        );
        for query in ["a4", "A4"] {
            let response = search(&conn, &request(query)).unwrap();
            assert_eq!(response.total_documents, 1);
            let hit = &response.documents[0].hits[0];
            assert_eq!(hit.highlights.len(), 1);
            assert_eq!(
                &hit.content[hit.highlights[0].start..hit.highlights[0].end],
                "A4"
            );
        }
    }

    #[test]
    fn title_scope_only_matches_title() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(
            &mut conn,
            "/d/标题命中.txt",
            FileType::Text,
            "# 合同\n\n正文无关",
        );
        save_doc(
            &mut conn,
            "/d/正文命中.txt",
            FileType::Text,
            "正文里有合同二字",
        );
        let response = search(&conn, &request("title:合同")).unwrap();
        assert_eq!(hit_paths(&response), ["/d/标题命中.txt"]);
        assert!(!response.documents[0].title_highlights.is_empty());
        // 默认 scope 下两者都命中。
        let response = search(&conn, &request("合同")).unwrap();
        assert_eq!(response.total_documents, 2);
    }

    #[test]
    fn minus_excludes() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/d/草稿.txt", FileType::Text, "合同编号 草稿版");
        save_doc(&mut conn, "/d/正式.txt", FileType::Text, "合同编号 正式版");
        let response = search(&conn, &request("合同 -草稿")).unwrap();
        assert_eq!(hit_paths(&response), ["/d/正式.txt"]);
    }

    #[cfg(feature = "jieba")]
    #[test]
    fn loose_mode_matches_non_contiguous() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(
            &mut conn,
            "/d/散开.txt",
            FileType::Text,
            "文档,然后管理,再归档",
        );
        let loose = search(
            &conn,
            &SearchRequest {
                loose: true,
                ..request("文档管理")
            },
        )
        .unwrap();
        assert_eq!(loose.total_documents, 1);
        assert!(loose.compiled.literals.len() >= 2);
        let exact = search(&conn, &request("文档管理")).unwrap();
        assert_eq!(exact.total_documents, 0, "精确模式不应命中非连续文本");
    }

    #[test]
    fn filters_by_file_type_and_path_prefix() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(&mut conn, "/data/docs/a.pdf", FileType::Pdf, "检索词正文");
        save_doc(&mut conn, "/data/docs/b.docx", FileType::Word, "检索词正文");
        save_doc(&mut conn, "/other/c.pdf", FileType::Pdf, "检索词正文");
        let typed = search(
            &conn,
            &SearchRequest {
                filters: Filters {
                    file_types: vec!["pdf".to_owned()],
                    ..Filters::default()
                },
                ..request("检索词")
            },
        )
        .unwrap();
        assert_eq!(typed.total_documents, 2);
        let prefixed = search(
            &conn,
            &SearchRequest {
                filters: Filters {
                    path_prefix: Some("/data/docs/".to_owned()),
                    ..Filters::default()
                },
                ..request("检索词")
            },
        )
        .unwrap();
        assert_eq!(prefixed.total_documents, 2);
        assert_eq!(
            hit_paths(&prefixed),
            ["/data/docs/a.pdf", "/data/docs/b.docx"]
        );
    }

    #[test]
    fn all_hits_are_kept_and_totals_are_exact() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc_chunks(
            &mut conn,
            "/d/多段.txt",
            FileType::Text,
            &["合同甲", "合同乙", "合同丙", "合同丁", "合同戊"],
        );
        save_doc(&mut conn, "/d/单段.txt", FileType::Text, "合同只有一处");
        let response = search(&conn, &request("合同")).unwrap();
        assert_eq!(response.total_hits, 6);
        assert_eq!(response.total_documents, 2);
        let doc = response
            .documents
            .iter()
            .find(|d| d.document.path == "/d/多段.txt")
            .expect("多段文档应命中");
        assert_eq!(doc.hits.len(), 5);
        assert_eq!(doc.total_hits, 5);
        // max_documents 截断 documents,但 total_documents 记真实数。
        let truncated = search(
            &conn,
            &SearchRequest {
                max_documents: 1,
                ..request("合同")
            },
        )
        .unwrap();
        assert_eq!(truncated.documents.len(), 1);
        assert_eq!(truncated.total_documents, 2);
        assert_eq!(truncated.total_hits, 6);
    }

    #[test]
    fn highlight_spans_strips_markers() {
        let (text, spans) = highlight_spans("前文\u{1}合同编号\u{2}后文", '\u{1}', '\u{2}');
        assert_eq!(text, "前文合同编号后文");
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].start..spans[0].end], "合同编号");
        // 无标记 → 原文、空区间。
        let (text, spans) = highlight_spans("没有标记", '\u{1}', '\u{2}');
        assert_eq!(text, "没有标记");
        assert!(spans.is_empty());
    }

    #[test]
    fn squash_literals_drops_empty_and_normalizes() {
        let needles = squash_literals(&["  ".to_owned(), "A 4".to_owned(), "文 档".to_owned()]);
        assert_eq!(needles, ["a4".to_owned(), "文档".to_owned()]);
        // "合同编号 " 是 13 字节(4×3 + 空格),"A4" 占 13..15。
        let span = Span { start: 13, end: 15 };
        assert_eq!(&"合同编号 A4"[13..15], "A4");
        assert!(accept_span(
            "合同编号 A4",
            &span,
            &squash_literals(&["a4".to_owned()])
        ));
    }

    #[test]
    fn locate_literals_merges_overlaps_and_ignores_case() {
        let text = "文档管理文档,A4 与 a4 各一处";
        let spans = locate_literals(text, &["文档".to_owned(), "a4".to_owned()]);
        let located: Vec<&str> = spans.iter().map(|s| &text[s.start..s.end]).collect();
        assert_eq!(located, ["文档", "文档", "A4", "a4"]);
        // 重叠合并:「文档文档」对「文档」和「档文」重叠时合一。
        let spans = locate_literals("文档文档", &["文档".to_owned(), "档文".to_owned()]);
        assert_eq!(spans, vec![Span { start: 0, end: 12 }]);
        // 词元边界:多字节字符不会错位。
        let spans = locate_literals("前合同后", &["合同".to_owned()]);
        assert_eq!(&"前合同后"[spans[0].start..spans[0].end], "合同");
    }

    #[test]
    fn syntax_error_surfaces_chinese_message() {
        let conn = crate::store::open_in_memory().unwrap();
        let error = search(&conn, &request("文档*")).unwrap_err();
        assert!(format!("{error:#}").contains("搜索语法错误"));
    }
}
