//! Stage 0 tokenizer spike.
//!
//! This crate deliberately contains only the tokenizer and the one connection
//! opening function used by the smoke tests.  It is not production code yet;
//! the point of the spike is to prove that a Rust tokenizer can be registered
//! with the bundled SQLite FTS5 implementation and that `highlight()` keeps
//! the byte ranges from the source text.

use rusqlite::Connection;
use rusqlite_ext::{TokenizeReason, Tokenizer, register_tokenizer};
use std::ffi::CStr;
use std::ops::Range;

/// A token and its byte range in the original UTF-8 string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub text: String,
    pub range: Range<usize>,
}

/// Return the `rsou 0` token sequence without going through SQLite.
///
/// The rules are intentionally the small set specified in `docs/plan.md`:
/// ASCII letters and digits form separate runs, ASCII runs are normalized to
/// lower case, Unicode alphanumeric characters are indexed one code point at
/// a time, and punctuation/whitespace are separators.  In particular, the
/// source range always refers to the original string rather than a normalized
/// copy.
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

/// The tokenizer registered under the `rsou` name.
pub struct RsouTokenizer;

impl Tokenizer for RsouTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"rsou"
    }

    fn new(_global: &Self::Global, args: Vec<String>) -> Result<Self, rusqlite::Error> {
        // `0` is the current mode.  `1` is accepted as a forward-compatible
        // placeholder for the future colocated-pinyin mode; accepting it here
        // makes the schema migration path explicit without changing semantics
        // in this spike.
        if args.iter().any(|arg| arg != "0" && arg != "1") {
            return Err(rusqlite::Error::InvalidParameterName(
                "rsou tokenizer accepts only mode 0 or 1".to_owned(),
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

/// Open a connection and register the tokenizer on that connection.
///
/// FTS5 tokenizer registration is per SQLite connection.  Keeping this as the
/// only constructor in the spike makes the read/write connection requirement
/// executable rather than a comment in the design document.
pub fn open_connection() -> Result<Connection, Box<dyn std::error::Error>> {
    let connection = Connection::open_in_memory()?;
    register(&connection)?;
    Ok(connection)
}

/// Register `rsou` on an already opened connection.
pub fn register(connection: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    register_tokenizer::<RsouTokenizer>(connection, ())?;
    Ok(())
}

/// The second-stage guard required by the plan for a character tokenizer:
/// FTS5 positions ignore punctuation, so `文、档` can satisfy the phrase
/// query `"文档"`.  The UI must only retain a highlight when the original
/// source contains the requested literal substring.
pub fn accepts_exact_literal(source: &str, literal: &str) -> bool {
    source.contains(literal)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let connection = open_connection().unwrap();
        connection
            .execute(
                "CREATE VIRTUAL TABLE chunks_fts USING fts5(title, context_header, content, tokenize = 'rsou 0')",
                [],
            )
            .unwrap();
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

        let punctuated_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                ["\"文档\""],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(punctuated_count, 1);
        assert!(!accepts_exact_literal("文、档", "文档"));
        assert!(accepts_exact_literal("合同编号 A4", "合同"));
    }

    #[test]
    fn every_connection_uses_the_same_registration_path() {
        let path = std::env::temp_dir().join(format!(
            "rsou-tokenizer-spike-{}-{}.sqlite3",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);

        {
            let connection = Connection::open(&path).unwrap();
            register_tokenizer::<RsouTokenizer>(&connection, ()).unwrap();
            connection
                .execute(
                    "CREATE VIRTUAL TABLE chunks_fts USING fts5(content, tokenize = 'rsou 0')",
                    [],
                )
                .unwrap();
            connection
                .execute("INSERT INTO chunks_fts(content) VALUES ('读连接')", [])
                .unwrap();
        }

        {
            let connection = Connection::open(&path).unwrap();
            register_tokenizer::<RsouTokenizer>(&connection, ()).unwrap();
            let count: i64 = connection
                .query_row(
                    "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                    ["\"连接\""],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
        let _ = std::fs::remove_file(path);
    }
}
