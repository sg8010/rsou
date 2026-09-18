//! Markdown → 纯文本(plain_text):全文检索的偏移基准。
//!
//! 这是按行处理的简化 GFM 转换(规则固定),不是完整解析器:解析产物只有
//! 两个消费者——分块与预览,二者都只需要「块序列 + 标题层级 + 在 plain_text
//! 中的字节区间」。
//!
//! 不变式:`plain.text[b.start..b.end]` 就是该块文本,块互不重叠且按序排列。

/// plain_text 中的一个块(段落/标题/代码段)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 在 plain_text 中的起始字节偏移
    pub start: usize,
    /// 结束字节偏移(不含块间换行)
    pub end: usize,
    /// 标题层级 1..=6;普通块为 None
    pub heading_level: Option<u8>,
}

/// 转换产物:纯文本 + 块清单。
#[derive(Debug, Clone, Default)]
pub struct PlainText {
    pub text: String,
    pub blocks: Vec<Block>,
}

impl PlainText {
    /// 块文本(不变式的读取入口)。
    pub fn block_text(&self, block: &Block) -> &str {
        &self.text[block.start..block.end]
    }
}

/// markdown → plain_text。
///
/// 规则(顺序固定):
/// - 代码围栏 ``` 行本身丢弃,围栏内原样保留(不做行内转换,空行也不分块);
/// - `#{1,6} ` 前缀去掉并记 heading_level(标题总是独立成块);
/// - 表格分隔行(只含 `|`、`-`、`:` 与空白)丢弃;
/// - 表格行去掉首尾 `|`,单元格用 ` │ ` 连接;
/// - 列表前缀(`- `/`* `/`+ `/`数字`. `)去掉,缩进保留;
/// - `> ` 引用前缀去掉;
/// - 行内:`![alt](url)`→alt、`[text](url)`→text、成对的 `**`/`__`/`` ` `` 去掉
///   (不成对不动)、`<br>`→换行、其它 `<...>` 去掉、
///   `&amp;`/`&lt;`/`&gt;`/`&quot;`/`&nbsp;` 反转义、`\`+标点 去反斜杠;
/// - 连续空行折叠为一个块分隔;块内多行用 `\n` 连接,块间用 `\n\n`。
pub fn markdown_to_plain(markdown: &str) -> PlainText {
    let mut plain = PlainText::default();
    let mut block_lines: Vec<String> = Vec::new();
    let mut in_code = false;

    for raw_line in markdown.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        // 代码围栏:围栏行丢弃,围栏内原样(空行也保留在块内)。
        if line.trim_start().starts_with("```") {
            emit(&mut plain, &mut block_lines, None);
            in_code = !in_code;
            continue;
        }
        if in_code {
            block_lines.push(line.to_owned());
            continue;
        }

        // 标题:独立成块。
        if let Some(level) = heading_level_of(line) {
            emit(&mut plain, &mut block_lines, None);
            let text = inline(line[level as usize + 1..].trim());
            push_text_lines(&text, &mut block_lines);
            emit(&mut plain, &mut block_lines, Some(level));
            continue;
        }

        // 表格分隔行:整行丢弃(不打断表格块)。
        if is_table_separator(line) {
            continue;
        }

        // 引用前缀 → 列表前缀(缩进保留)→ 表格行连接。
        let stripped = strip_list_marker(strip_quote_marker(line));
        let stripped = if is_table_row(&stripped) {
            join_table_row(&stripped)
        } else {
            stripped
        };

        let processed = inline(&stripped);
        if processed.trim().is_empty() {
            // 空行:块分隔(连续空行自然折叠)。
            emit(&mut plain, &mut block_lines, None);
            continue;
        }
        push_text_lines(&processed, &mut block_lines);
    }
    emit(&mut plain, &mut block_lines, None);
    plain
}

/// 取文档标题:第一个 heading_level<=2 的块文本(截 120 字符),否则 fallback。
pub fn extract_title(plain: &PlainText, fallback: &str) -> String {
    for block in &plain.blocks {
        if matches!(block.heading_level, Some(level) if level <= 2) {
            let text: String = plain.block_text(block).chars().take(120).collect();
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    fallback.to_owned()
}

/// 当前累计的行落成一个块(空块不落);块间统一补 `\n\n`。
fn emit(plain: &mut PlainText, block_lines: &mut Vec<String>, heading_level: Option<u8>) {
    if block_lines.is_empty() {
        return;
    }
    if !plain.text.is_empty() {
        plain.text.push_str("\n\n");
    }
    let start = plain.text.len();
    plain.text.push_str(&block_lines.join("\n"));
    let end = plain.text.len();
    plain.blocks.push(Block {
        start,
        end,
        heading_level,
    });
    block_lines.clear();
}

fn heading_level_of(line: &str) -> Option<u8> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        Some(hashes as u8)
    } else {
        None
    }
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
        && trimmed.contains('-')
}

fn is_table_row(line: &str) -> bool {
    line.trim().starts_with('|') && line.trim().len() > 1
}

/// `| a | b |` → `a │ b`。
fn join_table_row(line: &str) -> String {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" │ ")
}

/// 去掉 `> ` 引用前缀(可连续多级)。
fn strip_quote_marker(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix('>') {
            rest = after.strip_prefix(' ').unwrap_or(after);
        } else {
            return rest;
        }
    }
}

/// 去掉 `- `/`* `/`+ `/`数字`. ` 列表标记,前导缩进保留在正文前。
fn strip_list_marker(line: &str) -> String {
    let indent_end = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_end);
    for marker in ["- ", "* ", "+ "] {
        if let Some(after) = rest.strip_prefix(marker) {
            return format!("{indent}{after}");
        }
    }
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with(". ") {
        return format!("{indent}{}", &rest[digits + 2..]);
    }
    line.to_owned()
}

/// 行内标记处理;可能产生内含 `\n` 的多行文本(`<br>`)。
fn inline(text: &str) -> String {
    let text = strip_links_and_images(text);
    let text = strip_paired_marks(&text);
    let text = strip_html_tags(&text);
    let text = unescape_entities(&text);
    unescape_punctuation(&text)
}

/// `![alt](url)` → alt;`[text](url)` → text。
fn strip_links_and_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];
        let image = rest.starts_with("![");
        let open_len = if image {
            2
        } else if rest.starts_with('[') {
            1
        } else {
            0
        };
        if open_len > 0
            && let Some(close) = rest[open_len..].find(']')
        {
            let label_end = open_len + close;
            let after = label_end + 1;
            if rest[after..].starts_with('(')
                && let Some(pclose) = rest[after + 1..].find(')')
            {
                out.push_str(&rest[open_len..label_end]);
                pos += after + 1 + pclose + 1;
                continue;
            }
        }
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        pos += ch_len;
    }
    out
}

/// 去掉成对的 `**`、`__`、`` ` `` 标记;不成对的原样保留。
fn strip_paired_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];
        if let Some(inner) = rest.strip_prefix('`') {
            if let Some(close) = inner.find('`') {
                out.push_str(&inner[..close]);
                pos += 1 + close + 1;
            } else {
                out.push('`');
                pos += 1;
            }
            continue;
        }
        let mut consumed = false;
        for marker in ["**", "__"] {
            if let Some(inner) = rest.strip_prefix(marker) {
                if let Some(close) = inner.find(marker) {
                    out.push_str(&inner[..close]);
                    pos += marker.len() + close + marker.len();
                } else {
                    out.push_str(marker);
                    pos += marker.len();
                }
                consumed = true;
                break;
            }
        }
        if consumed {
            continue;
        }
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        pos += ch_len;
    }
    out
}

/// `<br>`→`\n`;其它 `<...>` 标签去掉。
fn strip_html_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];
        if rest.starts_with('<')
            && let Some(end) = rest.find('>')
        {
            let tag = rest[1..end]
                .trim()
                .trim_start_matches('/')
                .split(' ')
                .next()
                .unwrap_or("");
            if tag.eq_ignore_ascii_case("br") {
                out.push('\n');
            }
            pos += end + 1;
            continue;
        }
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        pos += ch_len;
    }
    out
}

fn unescape_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
}

/// `\`+ASCII 标点 → 去反斜杠。
fn unescape_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];
        if let Some(after) = rest.strip_prefix('\\') {
            let next = after.chars().next();
            if let Some(c) = next
                && c.is_ascii_punctuation()
            {
                out.push(c);
                pos += 1 + c.len_utf8();
                continue;
            }
        }
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        pos += ch_len;
    }
    out
}

/// 行内转换可能产生多行文本(`<br>`),逐行入块。
fn push_text_lines(text: &str, block_lines: &mut Vec<String>) {
    for line in text.split('\n') {
        if !line.trim().is_empty() {
            block_lines.push(line.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(plain: &PlainText) -> Vec<&str> {
        plain.blocks.iter().map(|b| plain.block_text(b)).collect()
    }

    #[test]
    fn headings_lists_quotes_and_tables() {
        let md = "# 标题一\n\n正文第一段。\n\n## 标题二\n\n- 列表甲\n- 列表乙\n\n> 引用内容\n\n| 甲 | 乙 |\n|---|---|\n| 1 | 2 |\n";
        let plain = markdown_to_plain(md);
        assert_eq!(
            texts(&plain),
            [
                "标题一",
                "正文第一段。",
                "标题二",
                "列表甲\n列表乙",
                "引用内容",
                "甲 │ 乙\n1 │ 2"
            ]
        );
        assert_eq!(plain.blocks[0].heading_level, Some(1));
        assert_eq!(plain.blocks[2].heading_level, Some(2));
        assert_eq!(extract_title(&plain, "回退"), "标题一");
    }

    #[test]
    fn inline_marks_are_stripped() {
        let md = "这是 **加粗** 和 __下划__ 与 `代码` 及 [链接文字](https://x) 图 ![替代](u) 标签<br>折行 &amp; 实体 \\*星号";
        let plain = markdown_to_plain(md);
        assert_eq!(plain.blocks.len(), 1);
        assert_eq!(
            plain.block_text(&plain.blocks[0]),
            "这是 加粗 和 下划 与 代码 及 链接文字 图 替代 标签\n折行 & 实体 *星号"
        );
    }

    #[test]
    fn code_fence_keeps_raw_lines() {
        let md = "前文\n\n```rust\n# 不是标题\n**保留**\n\n空行也在\n```\n\n后文";
        let plain = markdown_to_plain(md);
        let code = &plain.blocks[1];
        assert_eq!(plain.block_text(code), "# 不是标题\n**保留**\n\n空行也在");
    }

    #[test]
    fn block_offsets_are_exact() {
        let md = "# 标题\n\n第一段。\n\n第二段\n\n第三段";
        let plain = markdown_to_plain(md);
        let mut covered = 0;
        for block in &plain.blocks {
            assert!(block.start >= covered, "块重叠");
            covered = block.end;
            assert!(!plain.block_text(block).is_empty());
        }
    }

    #[test]
    fn unpaired_marks_are_kept() {
        let plain = markdown_to_plain("只有一个 *星号 不成对");
        assert_eq!(plain.block_text(&plain.blocks[0]), "只有一个 *星号 不成对");
    }

    #[test]
    fn title_falls_back() {
        let plain = markdown_to_plain("没有标题的正文");
        assert_eq!(extract_title(&plain, "文件名"), "文件名");
    }
}
