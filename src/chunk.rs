//! 标题感知单层分块(目标 400–900 字符,见 docs/plan.md §6 分块决策)。
//!
//! 输入是 `text::PlainText`(块序列 + 标题层级),输出每个 chunk 在
//! `plain.text` 中的字节区间与标题路径。
//!
//! 不变式:chunk 区间落在字符边界上、互不重叠、单调递增,并集覆盖全部
//! 非空块(块间的 `\n\n` 分隔符不属于任何块,也不属于任何 chunk)。

use crate::text::PlainText;

/// chunk 的目标最小/最大字符数(按 `chars().count()` 计)。
pub const CHUNK_MIN_CHARS: usize = 400;
pub const CHUNK_MAX_CHARS: usize = 900;

/// 一个分块:`start`/`end` 是 plain.text 的字节偏移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub index: usize,
    /// 「文档标题 › H1 › H2」形式的标题路径
    pub context_header: String,
    pub start: usize,
    pub end: usize,
}

/// 标题栈里的一级标题。
struct Heading {
    level: u8,
    text: String,
}

/// 正在累计的 chunk。
struct Pending {
    /// 字符数(按 chars 计)
    chars: usize,
    start: usize,
    end: usize,
    /// chunk 开始时捕获的标题路径
    context: String,
}

/// 句末切点候选字符(分块切点优先取这些之后)。
const SENTENCE_ENDS: &[char] = &['。', '!', '?', ';', '；', '!', '?', '\n'];

pub fn chunk_document(title: &str, plain: &PlainText) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stack: Vec<Heading> = Vec::new();
    let mut pending: Option<Pending> = None;

    let context_of = |stack: &[Heading]| -> String {
        // 文档标题本身就是第一个 H1/H2 时(extract_title 的常见结果),
        // 标题栈里已经有一模一样的一级标题;直接拼会得到「标题 › 标题」,
        // 所以与首项相同时不再重复前置。
        let mut parts: Vec<&str> = Vec::with_capacity(stack.len() + 1);
        if !title.is_empty() {
            parts.push(title);
        }
        for heading in stack {
            let text = heading.text.as_str();
            if !parts.last().is_some_and(|last| *last == text) {
                parts.push(text);
            }
        }
        parts.join(" › ")
    };

    let seal = |chunks: &mut Vec<Chunk>, pending: &mut Option<Pending>| {
        if let Some(p) = pending.take() {
            chunks.push(Chunk {
                index: chunks.len(),
                context_header: p.context,
                start: p.start,
                end: p.end,
            });
        }
    };

    for block in &plain.blocks {
        let block_text = plain.block_text(block);
        let block_chars = block_text.chars().count();

        if let Some(level) = block.heading_level {
            // 标题块:当前 chunk 达到下限就先封口,标题并进新 chunk;
            // 未达下限则并入当前 chunk(标题栈照常更新,影响后续 chunk)。
            if pending.as_ref().is_some_and(|p| p.chars >= CHUNK_MIN_CHARS) {
                seal(&mut chunks, &mut pending);
            }
            while stack.last().is_some_and(|h| h.level >= level) {
                stack.pop();
            }
            stack.push(Heading {
                level,
                text: block_text.to_owned(),
            });
            if pending.is_none() {
                pending = Some(Pending {
                    chars: 0,
                    start: block.start,
                    end: block.start,
                    context: context_of(&stack),
                });
            }
            let p = pending.as_mut().expect("pending 已初始化");
            p.chars += block_chars;
            p.end = block.end;
            continue;
        }

        if block_chars > CHUNK_MAX_CHARS {
            // 超长单块:先封口,再在块内按句末标点切,每段一个 chunk。
            seal(&mut chunks, &mut pending);
            let context = context_of(&stack);
            for (start, end) in split_long_block(block_text, block.start) {
                chunks.push(Chunk {
                    index: chunks.len(),
                    context_header: context.clone(),
                    start,
                    end,
                });
            }
            continue;
        }

        if pending
            .as_ref()
            .is_some_and(|p| p.chars + block_chars > CHUNK_MAX_CHARS)
        {
            // 并入后会超过上限:先封口再开新 chunk。
            seal(&mut chunks, &mut pending);
        }
        if pending.is_none() {
            pending = Some(Pending {
                chars: 0,
                start: block.start,
                end: block.start,
                context: context_of(&stack),
            });
        }
        let p = pending.as_mut().expect("pending 已初始化");
        p.chars += block_chars;
        p.end = block.end;
    }
    seal(&mut chunks, &mut pending);
    chunks
}

/// 把超过 MAX 的单块切成若干 ≤MAX 字符的段,返回各段的字节区间。
///
/// 切点:在前 MAX 字符窗口内找最后一个句末标点,切在它之后;找不到就按
/// MAX 字符处的字符边界硬切。末段不足 MIN 也单独成段(不丢字)。
fn split_long_block(text: &str, base: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut rest = text;
    let mut rest_base = base;
    // nth(MAX) 为 Some ⇔ 剩余字符数 > MAX,其字节下标即前 MAX 字符的前缀长度;
    // 只扫前 MAX 个字符,不每轮数完剩余全文。
    while let Some((prefix_bytes, _)) = rest.char_indices().nth(CHUNK_MAX_CHARS) {
        let prefix = &rest[..prefix_bytes];
        // 窗口内最后一个句末标点,切在它之后。
        let cut = prefix
            .char_indices()
            .rfind(|(_, c)| SENTENCE_ENDS.contains(c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(prefix_bytes);
        pieces.push((rest_base, rest_base + cut));
        rest_base += cut;
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push((rest_base, rest_base + rest.len()));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{Block, markdown_to_plain};

    fn plain_of(text: &str) -> PlainText {
        markdown_to_plain(text)
    }

    /// 造一个不含标题、每段 n 字符的 plain_text。
    fn blocks_of(piece_chars: usize, pieces: usize) -> PlainText {
        let mut plain = PlainText::default();
        for i in 0..pieces {
            if !plain.text.is_empty() {
                plain.text.push_str("\n\n");
            }
            let start = plain.text.len();
            plain.text.push_str(&"文".repeat(piece_chars));
            plain.text.push_str(&format!("{i}"));
            plain.blocks.push(Block {
                start,
                end: plain.text.len(),
                heading_level: None,
            });
        }
        plain
    }

    #[test]
    fn empty_plain_yields_no_chunks() {
        assert!(chunk_document("标题", &PlainText::default()).is_empty());
    }

    #[test]
    fn blocks_merge_until_max_then_split() {
        // 每块 300 字符(299 个文 + 序号):300×3=900 满,第四块开新 chunk。
        let plain = blocks_of(299, 4);
        let chunks = chunk_document("标题", &plain);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].start, plain.blocks[0].start);
        assert_eq!(chunks[0].end, plain.blocks[2].end);
        assert_eq!(chunks[1].start, plain.blocks[3].start);
        // 区间单调、互不重叠、覆盖全部块。
        let mut prev_end = 0;
        for chunk in &chunks {
            assert!(chunk.start >= prev_end);
            prev_end = chunk.end;
        }
    }

    #[test]
    fn heading_below_min_merges_into_current() {
        // 汉字三字节:"短正文" 0..9,"二级标题" 11..23,"后续内容" 25..37。
        let plain = PlainText {
            text: "短正文\n\n二级标题\n\n后续内容".to_owned(),
            blocks: vec![
                Block {
                    start: 0,
                    end: 9,
                    heading_level: None,
                },
                Block {
                    start: 11,
                    end: 23,
                    heading_level: Some(2),
                },
                Block {
                    start: 25,
                    end: 37,
                    heading_level: None,
                },
            ],
        };
        let chunks = chunk_document("文档", &plain);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].end, 37);
    }

    #[test]
    fn heading_after_min_seals_and_updates_context() {
        let body = "文".repeat(450);
        // body 1350 字节;"新章节" 9 字节在 1352..1361,"后文" 6 字节在 1363..1369。
        let plain = PlainText {
            text: format!("{body}\n\n新章节\n\n后文"),
            blocks: vec![
                Block {
                    start: 0,
                    end: body.len(),
                    heading_level: None,
                },
                Block {
                    start: body.len() + 2,
                    end: body.len() + 11,
                    heading_level: Some(1),
                },
                Block {
                    start: body.len() + 13,
                    end: body.len() + 19,
                    heading_level: None,
                },
            ],
        };
        let chunks = chunk_document("文档", &plain);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].context_header, "文档");
        // 新 chunk 的标题路径含压入的标题块本身。
        assert_eq!(chunks[1].context_header, "文档 › 新章节");
    }

    #[test]
    fn document_title_is_not_repeated_in_context() {
        // extract_title 取的就是第一个 H1/H2,标题栈里会有同一串文字;
        // 上下文不应变成「标题 › 标题」。
        let md = "# 采购合同管理办法\n\n正文。\n\n## 付款条款\n\n后文。";
        let plain = plain_of(md);
        let chunks = chunk_document("采购合同管理办法", &plain);
        // 全是短块 → 一个 chunk,上下文就是文档标题本身(不重复)。
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].context_header, "采购合同管理办法");
    }

    #[test]
    fn meaningful_section_path_is_kept_after_title() {
        // 第一章够长先封口,第二章的 chunk 应带上小节名,且不重复文档标题。
        let mut md = String::from("# 采购合同管理办法\n\n");
        md.push_str(&"第一章正文。".repeat(120));
        md.push_str("\n\n## 付款条款\n\n这里出现合同。");
        let plain = plain_of(&md);
        let chunks = chunk_document("采购合同管理办法", &plain);
        assert!(chunks.len() >= 2, "第一章应已封口: {}", chunks.len());
        assert_eq!(chunks[0].context_header, "采购合同管理办法");
        assert_eq!(chunks[1].context_header, "采购合同管理办法 › 付款条款");
    }

    #[test]
    fn nested_heading_stack_pops_shallower() {
        let md = "# 一\n\n正文\n\n## 一点一\n\n正文\n\n## 一点二\n\n正文\n\n# 二\n\n正文";
        let plain = plain_of(md);
        let chunks = chunk_document("文档", &plain);
        // 全是短块,不会按长度封口;每个标题并入当前 chunk,整个文档一个 chunk。
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn overlong_block_splits_without_losing_chars() {
        // 单块 2000 字符,其中埋句末标点。
        let mut body = "文".repeat(600);
        body.push('。');
        body.push_str(&"字".repeat(600));
        body.push('。');
        body.push_str(&"词".repeat(800));
        let plain = PlainText {
            text: body.clone(),
            blocks: vec![Block {
                start: 0,
                end: body.len(),
                heading_level: None,
            }],
        };
        let chunks = chunk_document("标题", &plain);
        assert!(chunks.len() >= 3, "应切成多段: {}", chunks.len());
        // 拼回与原块逐字节相等。
        let mut reassembled = String::new();
        for chunk in &chunks {
            reassembled.push_str(&plain.text[chunk.start..chunk.end]);
        }
        assert_eq!(reassembled, body);
        // 除末段外每段 ≤ MAX 且切在句末标点之后。
        for chunk in &chunks[..chunks.len() - 1] {
            let piece = &plain.text[chunk.start..chunk.end];
            assert!(piece.chars().count() <= CHUNK_MAX_CHARS);
            assert!(SENTENCE_ENDS.contains(&piece.chars().last().unwrap()));
        }
    }

    #[test]
    fn huge_single_block_splits_in_linear_time() {
        // 200 万字符的单块(无标题、无句末标点):逐段硬切,不整体反复扫描。
        let body = "文".repeat(2_000_000);
        let plain = PlainText {
            text: body.clone(),
            blocks: vec![Block {
                start: 0,
                end: body.len(),
                heading_level: None,
            }],
        };
        let chunks = chunk_document("", &plain);
        let mut prev_end = 0;
        for chunk in &chunks {
            let piece = &plain.text[chunk.start..chunk.end];
            assert!(piece.chars().count() <= CHUNK_MAX_CHARS);
            assert_eq!(chunk.start, prev_end, "相邻块应首尾相接");
            prev_end = chunk.end;
        }
        assert_eq!(prev_end, plain.text.len(), "分块并集应覆盖整个块");
    }

    #[test]
    fn overlong_block_without_punctuation_hard_cuts() {
        let body = "文".repeat(2000);
        let plain = PlainText {
            text: body.clone(),
            blocks: vec![Block {
                start: 0,
                end: body.len(),
                heading_level: None,
            }],
        };
        let chunks = chunk_document("标题", &plain);
        assert_eq!(chunks.len(), 3); // 900 + 900 + 200
        let mut reassembled = String::new();
        for chunk in &chunks {
            assert!(plain.text.is_char_boundary(chunk.start));
            reassembled.push_str(&plain.text[chunk.start..chunk.end]);
        }
        assert_eq!(reassembled, body);
    }
}
