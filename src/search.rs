//! 检索:FTS5 查询、字面量定位、按文档聚合与展示分块切分。
//!
//! 关键点:
//! - FTS 表 `documents_fts` **一行一篇文档**,所以 MATCH 里的隐式 AND / OR / NOT
//!   都是**文档级**语义(多词只要同篇命中即可),不会因为分块边界丢召回;
//! - 高亮不走 FTS5 的 `highlight()`:逐字索引下标点不产生词元,`文、档` 会被
//!   短语 `"文档"` 命中,而 `highlight()` 只返回区间、要判对错得再把区间文本取
//!   出来比对——既然要取文本,直接在 `plain_text` 上查字面量更直接。`locate_literals`
//!   一次完成「定位 + 精确过滤」:标点不剔除,因此 `文、档` 天然不命中,
//!   而空白/换行的 `文 档` 命中(与 plan §6.4 的收口规则一致);
//! - 展示分块由命中偏移 + `chunks` 边界切出,分块只影响「怎么展示」,
//!   不影响「能不能搜到」。

use std::time::Instant;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params_from_iter};

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
    /// 返回文档数上限;最多从前 200 个 FTS 候选中精确复核,默认展示 100 篇。
    pub max_documents: usize,
    /// 每篇最多返回几个展示片段(`chunks` 中命中块 + 相邻块,按命中位置取前 N)
    pub max_fragments_per_document: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: Scope::All,
            loose: false,
            filters: Filters::default(),
            max_documents: 100,
            max_fragments_per_document: DEFAULT_MAX_FRAGMENTS,
        }
    }
}

/// 每篇文档默认返回的展示片段数上限。
///
/// 「命中 N 处」现在指「N 个展示片段」,而不是 FTS 行数。不设上限的话,
/// 一篇长文档可能命中几百个块,右侧预览的「上一批/下一批」就失去意义了。
pub const DEFAULT_MAX_FRAGMENTS: usize = 20;

/// 每次最多读取正文并精确复核的 FTS 候选数,不继续补页。
const MAX_CANDIDATES: usize = 200;

/// 字节区间(content 内 / 列文本内)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// 一个展示片段。
///
/// `content` 取的是一个 `chunks` 块(命中位置所在块,必要时并入命中所在的相邻
/// 块),`highlights` 是 `content` 内的字节区间。`start_offset` 是 `content`
/// 在 `plain_text` 里的起点,所以「片段内偏移 + start_offset」能直接定位到
/// 预览全文。
#[derive(Debug, Clone)]
pub struct Hit {
    /// 片段起点在 plain_text 中的字节偏移
    pub start_offset: usize,
    pub end_offset: usize,
    /// 片段原文
    pub content: String,
    /// 标题路径(块所属章节;块无标题信息时为空)
    pub context_header: String,
    /// content 内的高亮区间
    pub highlights: Vec<Span>,
}

/// 一篇文档的聚合结果。
#[derive(Debug)]
pub struct DocumentHit {
    pub document: DocumentRow,
    /// 文档标题内的高亮区间
    pub title_highlights: Vec<Span>,
    /// 展示片段,按在文档中的位置升序
    pub hits: Vec<Hit>,
    /// 该文档的全部命中批次(可能多于 `hits`,超出上限的部分不再展示)
    pub total_hits: usize,
    /// bm25 分数(越小越相关)
    pub best_rank: f64,
}

/// 检索结果。
#[derive(Debug)]
pub struct SearchResponse {
    pub documents: Vec<DocumentHit>,
    /// 已展示文档的命中片段总数;文档被 `max_documents` 截断时是下界
    /// (完整总数需要为每篇都切片段,不值得)
    pub total_hits: usize,
    /// FTS 命中文档总数(含结构化过滤,不受候选/展示上限影响,可能含少量假阳性)。
    pub total_documents: usize,
    pub elapsed_ms: f64,
    /// 编译产物(literals 供预览定位)
    pub compiled: CompiledQuery,
}

/// 预览用:在全文里定位所有字面量(ASCII 大小写不敏感子串),合并重叠区间。
///
/// 这是唯一的定位与精确过滤入口:标点不剔除,所以 `文档` 不会命中 `文、档`;
/// 空白/换行不参与比对,所以 `文档` 会命中 `文 档` 与 `文\n档`。
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
        // 逐字索引把空白也当分隔符,所以字面量可能跨空白:`document 管理`
        // 对 `document\n管理` 也应命中。逐字定位一次去空白后的形态。
        if let Some(skipped) = find_ignoring_whitespace(text, literal) {
            spans.push(skipped);
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

/// 在 `text` 里找 `literal`,允字面量的字符之间隔着空白(与 FTS 逐字索引一致)。
/// 返回原文字节区间;找不到返回 None。
fn find_ignoring_whitespace(text: &str, literal: &str) -> Option<Span> {
    let needle: Vec<char> = literal.chars().filter(|c| !c.is_whitespace()).collect();
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<(usize, char)> = text.char_indices().collect();
    for start_index in 0..hay.len() {
        let mut cursor = start_index;
        let mut matched = 0usize;
        // 区间从**第一个非空白字符**起:不能把匹配前跳过的空白算进去,
        // 否则「合同编号 A4」会把前面的空格高亮进去。
        let mut start_byte: Option<usize> = None;
        let mut last_end = hay[start_index].0;
        while cursor < hay.len() && matched < needle.len() {
            let (byte, ch) = hay[cursor];
            if ch.is_whitespace() {
                cursor += 1;
                continue;
            }
            // ASCII 大小写不敏感(与 locate_literals 的比对口径一致)。
            if !ch.eq_ignore_ascii_case(&needle[matched]) {
                break;
            }
            if start_byte.is_none() {
                start_byte = Some(byte);
            }
            matched += 1;
            last_end = byte + ch.len_utf8();
            cursor += 1;
        }
        if matched == needle.len()
            && let Some(start) = start_byte
        {
            return Some(Span {
                start,
                end: last_end,
            });
        }
    }
    None
}

/// 转义 SQL LIKE 模式里的特殊字符(`\` 是转义符,另有 `%` 与 `_` 通配)。
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 文档的 plain_text(预览用);没有内容时返回 None。
pub fn plain_text_for_preview(
    conn: &Connection,
    document_id: i64,
) -> anyhow::Result<Option<String>> {
    repo::get_plain_text(conn, document_id)
}

/// 展示分块:某篇文档在 plain_text 中的块边界(用于把命中切成片段)。
pub fn chunk_ranges(
    conn: &Connection,
    document_id: i64,
) -> anyhow::Result<Vec<(usize, usize, String)>> {
    let mut stmt = conn.prepare(
        "SELECT start_offset, end_offset, context_header FROM chunks \
         WHERE document_id = ?1 ORDER BY chunk_index",
    )?;
    let rows = stmt
        .query_map([document_id], |row| {
            Ok((
                row.get::<_, i64>(0)? as usize,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 把命中区间切成展示片段。
///
/// 输入是**已定位好在 `content` 内的**区间(升序、互不重叠),输出片段序列:
/// 同一块内的命中合并成一个片段,不同块各出一个片段。
///
/// `bounds` 是展示块边界;为空(没有块信息)时整篇一段。
/// 命中跨块边界时把它两侧的块并进同一片段,保证短语两半都在片段里。
fn group_into_fragments(
    content: &str,
    spans: &[Span],
    bounds: &[(usize, usize, String)],
    document_title: &str,
    limit: usize,
) -> Vec<Hit> {
    if spans.is_empty() {
        return Vec::new();
    }
    // 无块信息:整篇一段。
    let fallback;
    let bounds: &[(usize, usize, String)] = if bounds.is_empty() {
        fallback = vec![(0, content.len(), String::new())];
        &fallback
    } else {
        bounds
    };
    // 有的文档根本没有标题(或标题没被识别出来),此时所有块共用同一个
    // 路径。那种路径只是重复文档标题,没有导航价值,一律不展示。
    let header_count = distinct_header_count(bounds);
    let header_count = if header_count <= 1 { 0 } else { header_count };

    // 每个命中归到它起点所在的块。
    let block_of = |byte: usize| -> usize {
        bounds
            .iter()
            .position(|(start, end, _)| byte >= *start && byte < *end)
            .unwrap_or_else(|| bounds.len().saturating_sub(1))
    };

    // 把命中归组:同一个块内的命中合为一个片段;**相邻但不共享块的命中各自
    // 成段**(它们本来就是不同的「命中处」)。跨块短语(一个 span 横跨两个块)
    // 把两个块并进同一段,因为它是一处命中,不该被切断。
    //
    // spans 已按 start 升序,所以只需与最后一组比较。
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        let first = block_of(span.start);
        // 末字节所在的块(span.end 是开区间,减 1 才落在片段内)。
        let last = block_of(span.end.saturating_sub(1)).max(first);
        match groups.last_mut() {
            // first 落在上一段内 ⇔ 两处命中共享同一块 → 并段。
            Some(group) if first <= group.1 => group.1 = group.1.max(last),
            _ => groups.push((first, last)),
        }
    }

    let mut hits = Vec::with_capacity(groups.len().min(limit));
    for (first, last) in groups {
        if hits.len() >= limit {
            break;
        }
        let start = bounds[first].0;
        let end = bounds[last].1;
        let Some(piece) = content.get(start..end) else {
            continue;
        };
        let highlights: Vec<Span> = spans
            .iter()
            .filter(|span| span.start >= start && span.end <= end)
            .map(|span| Span {
                start: span.start - start,
                end: span.end - start,
            })
            .collect();
        hits.push(Hit {
            start_offset: start,
            end_offset: end,
            content: piece.to_owned(),
            context_header: fragment_header(&bounds[first].2, header_count, document_title),
            highlights,
        });
    }
    hits
}

/// 不同的标题路径个数(用于判断标题是否具有区分度)。
fn distinct_header_count(bounds: &[(usize, usize, String)]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for (_, _, header) in bounds {
        if !header.is_empty() && !seen.contains(&header.as_str()) {
            seen.push(header);
        }
    }
    seen.len()
}

/// 片段的标题路径:去掉只重复文档标题的头一段。
///
/// `chunks.context_header` 是「文档标题 › H1 › H2」形式;文档标题在预览区顶部
/// 已经单独显示,片段再重复一次没有信息量。去掉后剩下的 H1 › H2 才是定位用的。
fn fragment_header(header: &str, header_count: usize, document_title: &str) -> String {
    if header_count == 0 || header.is_empty() {
        return String::new();
    }
    let without_title = match header.split_once(" › ") {
        Some((first, rest)) if first == document_title => rest,
        _ => header,
    };
    // 只剩文档标题本身(单级标题文档)→ 没有额外信息,不展示。
    if without_title.is_empty() || without_title == document_title {
        return String::new();
    }
    without_title.to_owned()
}

/// 执行检索:编译查询 → FTS 计数与相关度前 200 个候选 → 按 ID 读取正文
/// → 定位并精确复核 → 按展示分块切片段。
///
/// 语法错误把 `QueryError` 原样上抛(GUI 直接显示其中文文案)。
pub fn search(conn: &Connection, request: &SearchRequest) -> anyhow::Result<SearchResponse> {
    let started = Instant::now();
    let compiled =
        query::compile(&request.query, request.scope, request.loose).map_err(anyhow::Error::new)?;

    // WHERE 子句与参数只拼一次,给「取结果」与「数总数」两条 SQL 共用。
    let mut where_sql = String::from("WHERE documents_fts MATCH ?");
    let mut params: Vec<rusqlite::types::Value> = vec![compiled.match_expr.clone().into()];
    let mut filter_sql = String::new();
    if !request.filters.file_types.is_empty() {
        let marks = std::iter::repeat_n("?", request.filters.file_types.len())
            .collect::<Vec<_>>()
            .join(",");
        filter_sql.push_str(&format!(" AND d.file_type IN ({marks})"));
        for file_type in &request.filters.file_types {
            params.push(file_type.clone().into());
        }
    }
    if let Some(from) = request.filters.mtime_from_ms {
        filter_sql.push_str(" AND d.file_mtime_ms >= ?");
        params.push(from.into());
    }
    if let Some(to) = request.filters.mtime_to_ms {
        filter_sql.push_str(" AND d.file_mtime_ms <= ?");
        params.push(to.into());
    }
    if let Some(prefix) = &request.filters.path_prefix {
        // 同时匹配带与不带 `\\?\` 前缀的存储值。
        //
        // 库里的路径已经由 `normalize` 归一化(见 crate 注释),所以正常情况下
        // 第一种就够;但用户还可能手动填 `\\?\C:\...`,或者从旧版本一路升上来
        // 而某行因 UNIQUE 冲突/非 Unicode 没被剥成功。多一个 OR 分支的代价可忽略,
        // 却能避开“明明有这个目录却搜不到”的困惑。
        let escaped = escape_like(prefix);
        let mut alternatives = vec![format!("{escaped}%")];
        if !prefix.starts_with(r"\\?\") {
            let with_prefix = escape_like(&format!(r"\\?\{prefix}"));
            alternatives.push(format!("{with_prefix}%"));
        }
        let clause = std::iter::repeat_n("d.path LIKE ? ESCAPE '\\'", alternatives.len())
            .collect::<Vec<_>>()
            .join(" OR ");
        filter_sql.push_str(&format!(" AND ({clause})"));
        for value in alternatives {
            params.push(value.into());
        }
    }
    where_sql.push_str(&filter_sql);

    // 总数只统计 FTS 候选,接受逐字 tokenizer 的少量标点假阳性。
    // 与候选查询共用 JOIN、WHERE 和参数,保证结构化过滤口径一致。
    let from_sql = "FROM documents_fts JOIN documents d ON d.id = documents_fts.rowid";
    let total_documents: usize = conn
        .query_row(
            &format!("SELECT COUNT(*) {from_sql} {where_sql}"),
            params_from_iter(params.iter()),
            |row| row.get::<_, i64>(0),
        )
        .context("统计检索命中文档失败")? as usize;

    // 隐藏 rank 让 FTS5 按相关度输出有限候选;此阶段不返回标题/正文。
    // 不增加 rowid 次排序,避免 SQLite 为多列排序建立临时排序表。
    let candidate_sql = format!(
        "SELECT documents_fts.rowid, documents_fts.rank \
         {from_sql} {where_sql} AND rank MATCH 'bm25(5.0, 1.0)' \
         ORDER BY rank LIMIT {MAX_CANDIDATES}"
    );
    let candidates = conn
        .prepare(&candidate_sql)
        .context("准备检索语句失败")?
        .query_map(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })
        .context("执行检索失败")?
        .collect::<Result<Vec<_>, _>>()
        .context("读取检索候选失败")?;
    let ids: Vec<i64> = candidates.iter().map(|(id, _)| *id).collect();
    let mut meta = repo::get_documents_by_ids(conn, &ids).context("读取文档元数据失败")?;
    let mut content_stmt =
        conn.prepare("SELECT plain_text FROM document_contents WHERE document_id = ?1")?;

    let literals = &compiled.literals;
    let mut documents = Vec::with_capacity(candidates.len().min(request.max_documents));
    for (id, rank) in candidates {
        if documents.len() >= request.max_documents {
            break;
        }
        let Some(document) = meta.remove(&id) else {
            // 候选取回后文档已被删除——跳过。
            continue;
        };
        let Some(content) = content_stmt
            .query_row([id], |row| row.get::<_, String>(0))
            .optional()
            .context("读取候选文档正文失败")?
        else {
            continue;
        };
        // 定位结果直接复用于精确复核与片段生成,不重复调用定位函数。
        let content_highlights = locate_literals(&content, literals);
        let title_highlights = locate_literals(&document.title, literals);
        if content_highlights.is_empty() && title_highlights.is_empty() {
            continue;
        }
        let hits = group_into_fragments(
            &content,
            &content_highlights,
            &chunk_ranges(conn, id).unwrap_or_default(),
            &document.title,
            request.max_fragments_per_document,
        );
        let total_for_doc = hits.len();
        documents.push(DocumentHit {
            document,
            title_highlights,
            hits,
            total_hits: total_for_doc,
            best_rank: rank,
        });
    }

    // 「命中处数」= 所有**已展示**文档的片段数之和。文档被 max_documents 截断时
    // 它是个下界,调用方可以用 `total_documents > documents.len()` 判断并说明。
    let total_hits = documents.iter().map(|doc| doc.hits.len()).sum();

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
            source_root: None,
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
            source_root: None,
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
        assert_eq!(response.total_documents, 4, "FTS 总数包含标点假阳性");
        assert_eq!(paths.len(), 3, "{paths:?}");
        // 二次过滤现在由 locate_literals 承担:「文、档」不产生任何片段,
        // 于是整篇被丢掉(与分块实现的结论一致)。
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
    fn fragment_header_drops_redundant_document_title() {
        // 文档标题就是唯一的一级标题时,片段路径会退化成「标题 › 标题」。
        // 预览区顶部已经显示文档标题,片段不再重复。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(
            &mut conn,
            "/d/重复标题.txt",
            FileType::Text,
            "# 采购合同管理办法\n\n正文里出现合同二字。",
        );
        let response = search(&conn, &request("合同")).unwrap();
        let hit = &response.documents[0].hits[0];
        assert_eq!(hit.context_header, "", "重复的文档标题不应展示");
    }

    #[test]
    fn fragment_header_keeps_meaningful_section_path() {
        // 有多级标题时,片段应带上小节名(去掉重复的文档标题)。
        let mut conn = crate::store::open_in_memory().unwrap();
        let mut md = String::from("# 总则\n\n");
        md.push_str(&"第一章正文。".repeat(120));
        md.push_str("\n\n## 付款条款\n\n这里出现合同二字。");
        save_doc(&mut conn, "/d/多节.txt", FileType::Text, &md);
        let response = search(&conn, &request("合同")).unwrap();
        let doc = &response.documents[0];
        let hit = doc
            .hits
            .iter()
            .find(|h| h.content.contains("合同"))
            .expect("应有命中片段");
        assert_eq!(hit.context_header, "付款条款");
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
    fn title_only_hit_keeps_the_document() {
        // 标题命中但正文没有该字面量(宽松模式只切中标题时会出现):
        // 文档必须保留,不能因为没有片段而被丢掉。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc(
            &mut conn,
            "/d/仅标题.txt",
            FileType::Text,
            "# 合同管理\n\n正文完全无关",
        );
        let response = search(&conn, &request("title:合同")).unwrap();
        assert_eq!(response.total_documents, 1);
        assert!(!response.documents[0].title_highlights.is_empty());
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
    fn path_prefix_filter_tolerates_verbatim_prefix() {
        // Windows 上早期版本的入库路径带 `\\?\` 前缀。归一化之后不应再有,
        // 但用户手填或极端情况下仍可能遇到;过滤要能两边都对上,
        // 否则「明明有这个目录却搜不到」。
        let mut conn = crate::store::open_in_memory().unwrap();
        // 直接造两行:一行干净、一行带前缀(模拟未归一化的旧库)。
        for (path, tag) in [
            (r"C:\docs\干净.docx", "干净"),
            (r"\\?\C:\docs\带前缀.docx", "带前缀"),
        ] {
            let meta = FileMeta {
                path: path.into(),
                canonical_path: path.into(),
                file_name: path.rsplit('\\').next().unwrap_or(path).to_owned(),
                ext: "docx".to_owned(),
                file_type: FileType::Word,
                file_size: 0,
                file_mtime_ms: 1_000,
                source_root: None,
            };
            let plain = crate::text::markdown_to_plain(&format!("检索词正文 {tag}"));
            let chunks = chunk::chunk_document(&meta.stem(), &plain);
            let parsed = ParsedDocument {
                title: meta.stem(),
                markdown: String::new(),
                plain,
                chunks,
                warnings: Vec::new(),
                parser_name: "text",
                parser_version: "text.v1",
            };
            repo::save_parsed(&mut conn, &meta, "hash", &parsed, 1_000).unwrap();
        }

        // 用户输入常见的盘符前缀 → 两种存储形态都应命中。
        let by_plain = search(
            &conn,
            &SearchRequest {
                filters: Filters {
                    path_prefix: Some(r"C:\docs\".to_owned()),
                    ..Filters::default()
                },
                ..request("检索词")
            },
        )
        .unwrap();
        assert_eq!(by_plain.total_documents, 2, "普通前缀应同时命中两种形态");

        // 用户自己敲了 `\\?\` → 也应命中。
        let by_verbatim = search(
            &conn,
            &SearchRequest {
                filters: Filters {
                    path_prefix: Some(r"\\?\C:\docs\".to_owned()),
                    ..Filters::default()
                },
                ..request("检索词")
            },
        )
        .unwrap();
        assert_eq!(
            by_verbatim.total_documents, 1,
            "带前缀的输入应命中带前缀的行"
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
        // 「命中处数」是已展示文档的片段数之和:5 + 1 = 6。
        assert_eq!(response.total_hits, 6);
        assert_eq!(response.total_documents, 2);
        let doc = response
            .documents
            .iter()
            .find(|d| d.document.path == "/d/多段.txt")
            .expect("多段文档应命中");
        // 5 个块各含一次命中 → 5 个展示片段。
        assert_eq!(doc.hits.len(), 5);
        assert_eq!(doc.total_hits, 5);
        // 片段按文档内位置升序,且每条片段都真的含高亮。
        let mut prev = 0usize;
        for hit in &doc.hits {
            assert!(hit.start_offset >= prev, "片段应按位置升序");
            prev = hit.start_offset;
            assert!(!hit.highlights.is_empty());
        }
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
        // 截断后 total_hits 只是下界(只算了已展示的那篇的片段)。
        assert_eq!(truncated.total_hits, 5);
    }

    #[test]
    fn fragments_per_document_are_capped() {
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc_chunks(
            &mut conn,
            "/d/多段.txt",
            FileType::Text,
            &["合同甲", "合同乙", "合同丙", "合同丁", "合同戊"],
        );
        let response = search(
            &conn,
            &SearchRequest {
                max_fragments_per_document: 2,
                ..request("合同")
            },
        )
        .unwrap();
        let doc = &response.documents[0];
        assert_eq!(
            doc.hits.len(),
            2,
            "片段数应受 max_fragments_per_document 限制"
        );
        // 文档本身仍算命中,总数不变。
        assert_eq!(response.total_documents, 1);
    }

    #[test]
    fn words_far_apart_in_one_document_still_intersect() {
        // 这是改成文档级 FTS 的核心原因:两个词分居不同的展示块,
        // 隐式 AND 仍应命中(分块级 FTS 下会漏)。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc_chunks(
            &mut conn,
            "/d/远距.txt",
            FileType::Text,
            &["前段提到合同", "无关内容", "后段提到发票"],
        );
        let response = search(&conn, &request("合同 发票")).unwrap();
        assert_eq!(response.total_documents, 1, "同篇不同块的两词应命中");
        let doc = &response.documents[0];
        assert_eq!(doc.hits.len(), 2, "两个块各出一个片段");
        // 片段内偏移 + start_offset 能定位回全文。
        for hit in &doc.hits {
            assert_eq!(hit.content.len(), hit.end_offset - hit.start_offset);
        }
    }

    #[test]
    fn phrase_split_across_fragment_boundary_is_found() {
        // 字面量定位在原文上做,不经过块边界,所以跨块短语也能命中;
        // build_fragments 会把触及的两个块并进同一个片段。
        let mut conn = crate::store::open_in_memory().unwrap();
        save_doc_chunks(
            &mut conn,
            "/d/跨界.txt",
            FileType::Text,
            &["前文到公文", "编号后续内容"],
        );
        let response = search(&conn, &request("公文编号")).unwrap();
        assert_eq!(response.total_documents, 1);
        let doc = &response.documents[0];
        assert_eq!(doc.hits.len(), 1, "跨块短语应并入一个片段");
        let hit = &doc.hits[0];
        assert!(
            hit.content.contains("公文") && hit.content.contains("编号"),
            "片段应同时含短语两半:{}",
            hit.content
        );
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
    fn locate_literals_tolerates_whitespace_inside_literal() {
        // 逐字索引把空白当分隔符,所以「文 档」应命中「文档」与「文\n档」。
        for text in ["文 档", "文\n档"] {
            let spans = locate_literals(text, &["文档".to_owned()]);
            assert_eq!(spans.len(), 1, "应命中 {text:?}");
            assert_eq!(&text[spans[0].start..spans[0].end], text);
        }
        // 标点不剔除:「文、档」不命中。
        assert!(locate_literals("文、档", &["文档".to_owned()]).is_empty());
    }

    #[test]
    fn syntax_error_surfaces_chinese_message() {
        let conn = crate::store::open_in_memory().unwrap();
        let error = search(&conn, &request("文档*")).unwrap_err();
        assert!(format!("{error:#}").contains("搜索语法错误"));
    }
}
