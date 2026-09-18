//! 检索查询语法:词法 + 递归下降 → FTS5 MATCH 表达式。
//!
//! 语法逐条照搬 wsou `src/core/search/query-parser.ts`,去掉 NEAR 与同义词
//! 展开(本应用没有同义词表;宽松模式的分词替代 `cut` 回调):
//! - 空白分隔;`( ) : , -` 单字符 token;`'`/`"` 引号短语(未闭合/空短语报错);
//! - 单词在 `\s():,'"-` 处结束;纯数字是 number;AND/OR/NOT 大小写不敏感;
//! - `title:`/`content:` 字段前缀(未知字段报错);相邻条件隐式 AND;
//! - `-x`/`NOT x` 排除;OR 两侧都必须有正向条件;整体必须有正向条件;
//! - 词/短语里出现 `*` 或 `"` 报错;超过 MAX_QUERY_CHARS 报错。
//!
//! 编译产物与 ts 有一处刻意差异:单词项不再无条件套括号(`("x")`→`"x"`),
//! 只有宽松模式切出多段时才写成 `("s1" AND "s2")`,产物更可读且语义等价。

use std::collections::HashSet;

/// 默认检索字段(查询里显式 `title:`/`content:` 总是优先)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Title,
    Content,
}

/// 查询长度上限(字符数)。
pub const MAX_QUERY_CHARS: usize = 500;

/// 编译产物:MATCH 表达式 + 正向字面量(二次过滤与预览定位用)。
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    /// FTS5 MATCH 表达式
    pub match_expr: String,
    /// 全部正向条件的字面量:短语取原文,词在宽松模式下是 jieba 切出的各段;
    /// 去重并保持出现顺序
    pub literals: Vec<String>,
}

/// 查询语法错误(中文;Display 输出「搜索语法错误:…」)。
#[derive(Debug)]
pub struct QueryError(pub String);

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "搜索语法错误:{}", self.0)
    }
}

impl std::error::Error for QueryError {}

fn syntax<T>(message: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError(message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Content,
}

impl Field {
    fn prefix(self) -> &'static str {
        match self {
            Field::Title => "title:",
            Field::Content => "content:",
        }
    }
}

#[derive(Debug)]
enum Node {
    Term { text: String, field: Option<Field> },
    Phrase { text: String, field: Option<Field> },
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
}

impl Node {
    fn has_positive(&self) -> bool {
        match self {
            Node::Not(_) => false,
            Node::Term { .. } | Node::Phrase { .. } => true,
            Node::And(l, r) | Node::Or(l, r) => l.has_positive() || r.has_positive(),
        }
    }
}

#[derive(Debug)]
enum Token {
    Word(String),
    Phrase(String),
    And,
    Or,
    Not,
    LParen,
    RParen,
    Colon,
    Comma,
    Minus,
    Number(String),
}

fn is_word_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | ':' | ',' | '\'' | '"' | '-')
}

/// 词法分析(与 ts `tokenize` 同构)。
fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
                continue;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
                continue;
            }
            ':' => {
                tokens.push(Token::Colon);
                i += 1;
                continue;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
                continue;
            }
            '-' => {
                tokens.push(Token::Minus);
                i += 1;
                continue;
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let mut value = String::new();
                while i < chars.len() && chars[i] != quote {
                    value.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return syntax("引号没有闭合");
                }
                i += 1;
                if value.is_empty() {
                    return syntax("短语不能为空");
                }
                tokens.push(Token::Phrase(value));
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < chars.len() && !is_word_end(chars[i]) {
            i += 1;
        }
        if start == i {
            return syntax(format!("无法识别字符“{}”", chars[i]));
        }
        let value: String = chars[start..i].iter().collect();
        if value.bytes().all(|b| b.is_ascii_digit()) {
            tokens.push(Token::Number(value));
        } else {
            match value.to_uppercase().as_str() {
                "AND" => tokens.push(Token::And),
                "OR" => tokens.push(Token::Or),
                "NOT" => tokens.push(Token::Not),
                _ => tokens.push(Token::Word(value)),
            }
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    index: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.index)
    }

    fn peek_at(&self, offset: usize) -> Option<&'a Token> {
        self.tokens.get(self.index + offset)
    }

    fn take(&mut self) -> Result<&'a Token, QueryError> {
        let Some(token) = self.tokens.get(self.index) else {
            return syntax("条件不完整");
        };
        self.index += 1;
        Ok(token)
    }

    fn parse(&mut self) -> Result<Node, QueryError> {
        if self.tokens.is_empty() {
            return syntax("查询不能为空");
        }
        let result = self.parse_or()?;
        if self.peek().is_some() {
            return syntax("存在无法识别的尾部条件");
        }
        if !result.has_positive() {
            return syntax("查询必须包含至少一个正向条件");
        }
        Ok(result)
    }

    fn parse_or(&mut self) -> Result<Node, QueryError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.index += 1;
            left = Node::Or(Box::new(left), Box::new(self.parse_and()?));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node, QueryError> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Token::And) => {
                    self.index += 1;
                    left = Node::And(Box::new(left), Box::new(self.parse_unary()?));
                }
                Some(Token::Not) => {
                    self.index += 1;
                    left = Node::And(
                        Box::new(left),
                        Box::new(Node::Not(Box::new(self.parse_unary()?))),
                    );
                }
                Some(token) if Self::starts_primary(token) => {
                    left = Node::And(Box::new(left), Box::new(self.parse_unary()?));
                }
                _ => return Ok(left),
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Node, QueryError> {
        match self.peek() {
            Some(Token::Minus) | Some(Token::Not) => {
                self.index += 1;
                Ok(Node::Not(Box::new(self.parse_unary()?)))
            }
            _ => self.parse_primary(None),
        }
    }

    fn parse_primary(&mut self, field: Option<Field>) -> Result<Node, QueryError> {
        // `title:`/`content:` 字段前缀;其它 word 接冒号是未知字段。
        if let Some(Token::Word(value)) = self.peek()
            && matches!(self.peek_at(1), Some(Token::Colon))
        {
            match value.as_str() {
                "title" | "content" => {
                    self.index += 2;
                    let field = if value == "title" {
                        Field::Title
                    } else {
                        Field::Content
                    };
                    return self.parse_primary(Some(field));
                }
                _ => return syntax(format!("未知字段“{value}”")),
            }
        }
        match self.peek() {
            Some(Token::LParen) => {
                self.index += 1;
                let nested = self.parse_or()?;
                if !matches!(self.take()?, Token::RParen) {
                    return syntax("括号不匹配");
                }
                Ok(match field {
                    None => nested,
                    Some(field) => Self::apply_field(nested, field),
                })
            }
            Some(Token::Word(_)) | Some(Token::Number(_)) => {
                let text = match self.take()? {
                    Token::Word(v) | Token::Number(v) => v.clone(),
                    _ => unreachable!(),
                };
                Ok(Node::Term { text, field })
            }
            Some(Token::Phrase(_)) => {
                let text = match self.take()? {
                    Token::Phrase(v) => v.clone(),
                    _ => unreachable!(),
                };
                Ok(Node::Phrase { text, field })
            }
            _ => syntax("缺少词语、短语或括号条件"),
        }
    }

    fn starts_primary(token: &Token) -> bool {
        matches!(
            token,
            Token::Word(_) | Token::Number(_) | Token::Phrase(_) | Token::LParen | Token::Minus
        )
    }

    /// `field:(...)` 把字段下推到括号内所有叶子。
    fn apply_field(node: Node, field: Field) -> Node {
        match node {
            Node::Term { text, .. } => Node::Term {
                text,
                field: Some(field),
            },
            Node::Phrase { text, .. } => Node::Phrase {
                text,
                field: Some(field),
            },
            Node::Not(child) => Node::Not(Box::new(Self::apply_field(*child, field))),
            Node::And(l, r) => Node::And(
                Box::new(Self::apply_field(*l, field)),
                Box::new(Self::apply_field(*r, field)),
            ),
            Node::Or(l, r) => Node::Or(
                Box::new(Self::apply_field(*l, field)),
                Box::new(Self::apply_field(*r, field)),
            ),
        }
    }
}

/// FTS5 短语字面量:含 `*` 或 `"` 拒绝(`"` 双写转义后才有意义,但词内一律报错)。
fn quote(value: &str) -> Result<String, QueryError> {
    if value.is_empty() || value.contains(['"', '*']) {
        return syntax("词语包含不支持的查询字符");
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

/// 宽松模式的中文分词:jieba(编译期词典);无 jieba feature 时退化为整词。
#[cfg(feature = "jieba")]
pub fn cut_loose(term: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();
    JIEBA
        .get_or_init(jieba_rs::Jieba::new)
        .cut(term, false)
        .into_iter()
        .map(|token| token.word)
        .filter(|part| !part.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// 无 jieba feature:宽松模式退化为精确模式(不报错)。
#[cfg(not(feature = "jieba"))]
pub fn cut_loose(term: &str) -> Vec<String> {
    vec![term.to_owned()]
}

/// 把查询编译成 FTS5 MATCH 表达式。
///
/// `loose = true` 时单词项经 `cut_loose` 切段,各段 AND 连接;`loose = false`
/// 时整词作为一个短语。`literals` 收集全部正向条件的字面量(去重、保序)。
pub fn compile(input: &str, scope: Scope, loose: bool) -> Result<CompiledQuery, QueryError> {
    if input.chars().count() > MAX_QUERY_CHARS {
        return syntax("查询过长");
    }
    let tokens = tokenize(input)?;
    let mut parser = Parser {
        tokens: &tokens,
        index: 0,
    };
    let ast = parser.parse()?;

    let default_field = match scope {
        Scope::All => None,
        Scope::Title => Some(Field::Title),
        Scope::Content => Some(Field::Content),
    };
    let cut: fn(&str) -> Vec<String> = if loose {
        cut_loose
    } else {
        |term| vec![term.to_owned()]
    };
    let mut literals: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut positive = false;

    let match_expr = compile_node(
        &ast,
        None,
        default_field,
        cut,
        true,
        &mut literals,
        &mut seen,
        &mut positive,
    )?;
    if !positive {
        return syntax("查询必须包含至少一个正向条件");
    }
    Ok(CompiledQuery {
        match_expr,
        literals,
    })
}

fn push_literal(literals: &mut Vec<String>, seen: &mut HashSet<String>, value: &str) {
    if seen.insert(value.to_owned()) {
        literals.push(value.to_owned());
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_node(
    node: &Node,
    inherited: Option<Field>,
    default_field: Option<Field>,
    cut: fn(&str) -> Vec<String>,
    positive_ctx: bool,
    literals: &mut Vec<String>,
    seen: &mut HashSet<String>,
    positive: &mut bool,
) -> Result<String, QueryError> {
    match node {
        Node::Not(child) => Ok(format!(
            "NOT {}",
            compile_node(
                child,
                inherited,
                default_field,
                cut,
                false,
                literals,
                seen,
                positive
            )?
        )),
        Node::And(left, right) | Node::Or(left, right) => {
            if matches!(node, Node::Or(..)) && (!left.has_positive() || !right.has_positive()) {
                return syntax("OR 的每个分支都必须包含正向条件");
            }
            // 左侧没有正向条件时,AND 编译为「右 NOT 左」(与 ts 一致)。
            if matches!(node, Node::And(..)) && !left.has_positive() && right.has_positive() {
                let right_expr = compile_node(
                    right,
                    inherited,
                    default_field,
                    cut,
                    positive_ctx,
                    literals,
                    seen,
                    positive,
                )?;
                let excluded = compile_exclusion(left, inherited, default_field, cut)?;
                return Ok(format!("({right_expr} NOT {excluded})"));
            }
            let left_expr = compile_node(
                left,
                inherited,
                default_field,
                cut,
                positive_ctx,
                literals,
                seen,
                positive,
            )?;
            // `x AND NOT y` → `(x NOT y)`。
            if let Node::Not(child) = right.as_ref()
                && matches!(node, Node::And(..))
            {
                let right_expr = compile_node(
                    child,
                    inherited,
                    default_field,
                    cut,
                    false,
                    literals,
                    seen,
                    positive,
                )?;
                return Ok(format!("({left_expr} NOT {right_expr})"));
            }
            let right_expr = compile_node(
                right,
                inherited,
                default_field,
                cut,
                positive_ctx,
                literals,
                seen,
                positive,
            )?;
            let op = if matches!(node, Node::Or(..)) {
                "OR"
            } else {
                "AND"
            };
            Ok(format!("({left_expr} {op} {right_expr})"))
        }
        Node::Phrase { text, field } => {
            *positive = true;
            let field = field.or(inherited).or(default_field);
            if positive_ctx {
                push_literal(literals, seen, text);
            }
            Ok(format!(
                "{}{}",
                field.map(Field::prefix).unwrap_or(""),
                quote(text)?
            ))
        }
        Node::Term { text, field } => {
            *positive = true;
            let field = field.or(inherited).or(default_field);
            let mut pieces = cut(text);
            pieces.retain(|part| !part.is_empty());
            if pieces.is_empty() {
                return syntax("普通词无法分词");
            }
            if positive_ctx {
                for piece in &pieces {
                    push_literal(literals, seen, piece);
                }
            }
            let mut quoted = Vec::with_capacity(pieces.len());
            for piece in &pieces {
                quoted.push(quote(piece)?);
            }
            let body = quoted.join(" AND ");
            let body = if pieces.len() == 1 {
                body
            } else {
                format!("({body})")
            };
            Ok(format!(
                "{}{}",
                field.map(Field::prefix).unwrap_or(""),
                body
            ))
        }
    }
}

/// 排除侧(AND NOT 的右侧)编译:内部不再区分正负,按原样结构展开。
fn compile_exclusion(
    node: &Node,
    inherited: Option<Field>,
    default_field: Option<Field>,
    cut: fn(&str) -> Vec<String>,
) -> Result<String, QueryError> {
    match node {
        Node::Not(child) => compile_exclusion(child, inherited, default_field, cut),
        Node::And(left, right) | Node::Or(left, right) => {
            let op = if matches!(node, Node::Or(..)) {
                "OR"
            } else {
                "AND"
            };
            Ok(format!(
                "({} {op} {})",
                compile_exclusion(left, inherited, default_field, cut)?,
                compile_exclusion(right, inherited, default_field, cut)?
            ))
        }
        leaf => {
            let mut literals = Vec::new();
            let mut seen = HashSet::new();
            let mut positive = false;
            compile_node(
                leaf,
                inherited,
                default_field,
                cut,
                false,
                &mut literals,
                &mut seen,
                &mut positive,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (输入, scope, loose, 期望 MATCH 或 None=应报错)。
    #[test]
    fn compile_table() {
        let cases: &[(&str, Scope, bool, Option<&str>)] = &[
            ("文档", Scope::All, false, Some("\"文档\"")),
            (
                "文档 管理",
                Scope::All,
                false,
                Some("(\"文档\" AND \"管理\")"),
            ),
            ("\"公文 号\"", Scope::All, false, Some("\"公文 号\"")),
            (
                "title:合同 -草稿",
                Scope::All,
                false,
                Some("(title:\"合同\" NOT \"草稿\")"),
            ),
            ("a OR b", Scope::All, false, Some("(\"a\" OR \"b\")")),
            (
                "content:(x y)",
                Scope::All,
                false,
                Some("(content:\"x\" AND content:\"y\")"),
            ),
            // 显式字段覆盖默认 scope
            ("title:x", Scope::Content, false, Some("title:\"x\"")),
            // scope 决定默认字段
            ("x", Scope::Title, false, Some("title:\"x\"")),
            ("x", Scope::Content, false, Some("content:\"x\"")),
            ("a AND NOT b", Scope::All, false, Some("(\"a\" NOT \"b\")")),
            ("NOT a b", Scope::All, false, Some("(\"b\" NOT \"a\")")),
            (
                "a NOT (b OR c)",
                Scope::All,
                false,
                Some("(\"a\" NOT (\"b\" OR \"c\"))"),
            ),
            ("  文档  ", Scope::All, false, Some("\"文档\"")),
            // 数字也是词;查询字面量保持原样(索引端小写,FTS5 用同一 tokenizer 处理查询)
            ("A4", Scope::All, false, Some("\"A4\"")),
            // 错误用例
            ("", Scope::All, false, None),
            ("   ", Scope::All, false, None),
            ("文档*", Scope::All, false, None),
            ("-草稿", Scope::All, false, None),
            ("NOT a", Scope::All, false, None),
            ("\"未闭合", Scope::All, false, None),
            ("\"\"", Scope::All, false, None),
            ("foo:x", Scope::All, false, None),
            ("a OR", Scope::All, false, None),
            ("a b)", Scope::All, false, None),
            ("a OR -b", Scope::All, false, None),
        ];
        for (input, scope, loose, expected) in cases {
            let result = compile(input, *scope, *loose);
            match expected {
                Some(expr) => assert_eq!(
                    result.map(|q| q.match_expr).as_deref().ok(),
                    Some(*expr),
                    "输入 {input:?}"
                ),
                None => assert!(
                    result.is_err(),
                    "输入 {input:?} 应报语法错误,实际得到 {:?}",
                    result.map(|q| q.match_expr)
                ),
            }
        }
    }

    #[test]
    fn literals_follow_positive_order_and_dedup() {
        let query = compile("文档 管理 文档 -草稿", Scope::All, false).unwrap();
        assert_eq!(query.literals, ["文档", "管理"]);
    }

    #[test]
    fn error_messages_are_chinese() {
        for (input, hint) in [
            ("\"未闭合", "引号没有闭合"),
            ("文档*", "词语包含不支持的查询字符"),
            ("foo:x", "未知字段"),
            ("", "查询不能为空"),
            ("-草稿", "查询必须包含至少一个正向条件"),
            ("a OR -b", "OR 的每个分支都必须包含正向条件"),
        ] {
            let error = compile(input, Scope::All, false).unwrap_err();
            assert!(error.0.contains(hint), "{input:?} → {error}");
            assert!(error.to_string().starts_with("搜索语法错误:"));
        }
    }

    #[test]
    fn overlong_query_rejected() {
        let input = "文".repeat(MAX_QUERY_CHARS + 1);
        let error = compile(&input, Scope::All, false).unwrap_err();
        assert!(error.0.contains("查询过长"));
    }

    #[cfg(feature = "jieba")]
    #[test]
    fn loose_mode_cuts_terms_and_collects_segments() {
        let query = compile("文档管理系统", Scope::All, true).unwrap();
        // jieba 至少切出两段;各段 AND 连接,字面量与段一致。
        assert!(query.literals.len() >= 2, "literals: {:?}", query.literals);
        for piece in &query.literals {
            assert!(query.match_expr.contains(&format!("\"{piece}\"")));
        }
        assert!(query.match_expr.contains(" AND "));
        // 精确模式不切段。
        let exact = compile("文档管理系统", Scope::All, false).unwrap();
        assert_eq!(exact.match_expr, "\"文档管理系统\"");
    }
}
