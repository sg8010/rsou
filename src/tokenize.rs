//! `rsou` FTS5 自定义 tokenizer(逐字索引,与 wsou 的 `simple 0` 同语义)。
//!
//! 规则(见 docs/plan.md §6.1):
//! - 非 ASCII 的字母数字字符(含全部 CJK 与中文标点之外的 Unicode 字符)
//!   逐码点成词元;
//! - ASCII 字母连续段小写后成一个词元,ASCII 数字连续段成一个词元,
//!   字母与数字之间切开("A4" → "a","4");
//! - 空白、标点与其余符号是分隔符,不产生词元。
//!
//! 上报给 FTS5 的每个词元都带着它在原文中的字节区间,因此 `highlight()`
//! 的标记天然落在原文上,不需要任何坐标映射。
//!
//! 本模块只包含纯逻辑;tokenize 注册统一由 `store::open`/`open_in_memory`
//! 调用 `register` 完成(FTS5 tokenizer 注册是 per-connection 的)。

use rusqlite::Connection;
use rusqlite_ext::{TokenizeReason, Tokenizer, register_tokenizer};
use std::ffi::CStr;
use std::ops::Range;

/// 一个词元及其在原始 UTF-8 文本中的字节区间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub text: String,
    pub range: Range<usize>,
}

/// 不走 SQLite,直接返回 `rsou 0` 的词元序列(便于单测与调试)。
///
/// 词元区间始终指向原始字符串,而不是归一化后的副本。
pub fn token_spans(text: &str) -> Vec<TokenSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut iter = text.char_indices().peekable();

    while let Some((start, ch)) = iter.next() {
        if ch.is_ascii_alphabetic() {
            let mut end = start + ch.len_utf8();
            while let Some(&(next_start, next)) = iter.peek() {
                if next.is_ascii_alphabetic() {
                    iter.next();
                    end = next_start + next.len_utf8();
                } else {
                    break;
                }
            }
            spans.push(TokenSpan {
                text: bytes[start..end]
                    .iter()
                    .map(|byte| byte.to_ascii_lowercase() as char)
                    .collect(),
                range: start..end,
            });
        } else if ch.is_ascii_digit() {
            let mut end = start + ch.len_utf8();
            while let Some(&(next_start, next)) = iter.peek() {
                if next.is_ascii_digit() {
                    iter.next();
                    end = next_start + next.len_utf8();
                } else {
                    break;
                }
            }
            spans.push(TokenSpan {
                text: text[start..end].to_owned(),
                range: start..end,
            });
        } else if !ch.is_ascii() && ch.is_alphanumeric() {
            spans.push(TokenSpan {
                text: text[start..start + ch.len_utf8()].to_owned(),
                range: start..start + ch.len_utf8(),
            });
        }
    }

    spans
}

/// 注册名 `rsou` 的 tokenizer。
pub struct RsouTokenizer;

impl Tokenizer for RsouTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"rsou"
    }

    fn new(_global: &Self::Global, args: Vec<String>) -> Result<Self, rusqlite::Error> {
        // `0` 是当前模式(逐字索引,关闭拼音,对齐 wsou 的 `simple 0`)。
        // `1` 是预留参数位:将来做拼音搜索时表示「汉字词元 + COLOCATED 拼音词元」。
        if args.iter().any(|arg| arg != "0" && arg != "1") {
            return Err(rusqlite::Error::InvalidParameterName(
                "rsou tokenizer 只接受模式 0 或 1".to_owned(),
            ));
        }
        Ok(Self)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        text: &[u8],
        mut push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        let text = std::str::from_utf8(text)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        for span in token_spans(text) {
            push_token(span.text.as_bytes(), span.range, false)?;
        }
        Ok(())
    }
}

/// 在已打开的连接上注册 `rsou` tokenizer。
///
/// FTS5 tokenizer 注册是 per-connection 的:应用内只允许
/// `store::open`/`store::open_in_memory` 调用本函数,把注册收在一处才能保证
/// 「任何连接都已注册」这条不变式(导出给外部调用方做一次性注册时用)。
pub fn register(connection: &Connection) -> rusqlite::Result<()> {
    register_tokenizer::<RsouTokenizer>(connection, ())
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    #[test]
    fn token_spans_preserve_source_ranges() {
        let text = "A4 foo中文，文、档 😀";
        let tokens = token_spans(text);
        let values: Vec<_> = tokens.iter().map(|token| token.text.as_str()).collect();
        assert_eq!(values, ["a", "4", "foo", "中", "文", "文", "档"]);
        for token in tokens {
            assert_eq!(text[token.range.clone()].to_ascii_lowercase(), token.text);
            assert!(token.range.end <= text.len());
        }
    }

    #[test]
    fn registered_fts5_tokenizer_matches_and_highlights_original_text() {
        let connection = store::open_in_memory().unwrap();
        connection
            .execute(
                "INSERT INTO chunks_fts(rowid, title, context_header, content) VALUES (1, '合同', '资料', '合同编号 A4'), (2, '噪声', '资料', '文、档')",
                [],
            )
            .unwrap();

        let highlighted: String = connection
            .query_row(
                "SELECT highlight(chunks_fts, 2, char(1), char(2)) FROM chunks_fts WHERE chunks_fts MATCH ?1 AND rowid = 1",
                ["\"合同\""],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(highlighted, "\u{1}合同\u{2}编号 A4");

        // 逐字索引下标点不产生词元,「文、档」会被短语 "文档" 误配;
        // 这是已知行为,由检索层的二次精确过滤收口(见 plan §6.4)。
        let punctuated_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                ["\"文档\""],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(punctuated_count, 1);
    }
}
