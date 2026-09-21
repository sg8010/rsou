//! 检索查询语法:词法 + 递归下降 → FTS5 MATCH 表达式。
//!
//! 语法逐条照搬 wsou `src/core/search/query-parser.ts`,去掉 NEAR(宽松模式的
//! 分词与用户同义词组替代了它的 `cut` 回调与同义词展开):
//! - 空白分隔;`( ) : , -` 单字符 token;`'`/`"` 引号短语(未闭合/空短语报错);
//! - 单词在 `\s():,'"-` 处结束;纯数字是 number;AND/OR/NOT 大小写不敏感;
//! - `title:`/`content:` 字段前缀(未知字段报错);相邻条件隐式 AND;
//! - 字段分组内禁止再次指定字段(同字段也拒绝);组外可自由组合字段条件;
//! - 禁止双否定与嵌套排除;字段后的否定请写为 `-title:x`;
//! - `-x`/`NOT x` 排除;OR 两侧都必须有正向条件;整体必须有正向条件;
//! - 词/短语里出现 `*` 或 `"` 报错;超过 MAX_QUERY_CHARS 报错。
//!
//! 编译产物与 ts 有一处刻意差异:单词项不再无条件套括号(`("x")`→`"x"`),
//! 只有切出多段或命中同义词组时才写成 `(…)`,产物更可读且语义等价。

use std::collections::HashSet;

use crate::dict;

/// 默认检索字段(查询里显式 `title:`/`content:` 总是优先)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Title,
    Content,
}

/// 查询长度上限(字符数)。
pub const MAX_QUERY_CHARS: usize = 500;

/// 编译产物:MATCH 表达式 + 正向字面量(供展示定位与预览高亮用,
/// 不参与搜索真假判定)。
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
    fn validate_negation(&self, excluded: bool) -> Result<(), QueryError> {
        match self {
            Node::Not(_) if excluded => syntax("不支持双否定或嵌套排除"),
            Node::Not(child) => child.validate_negation(true),
            Node::And(left, right) | Node::Or(left, right) => {
                left.validate_negation(excluded)?;
                right.validate_negation(excluded)?;
                if matches!(self, Node::Or(..)) && (!left.has_positive() || !right.has_positive()) {
                    return syntax("OR 的每个分支都必须包含正向条件");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// 只展开 AND,保留 OR 和被排除的括号作为完整条件。
    fn conjunction_parts<'a>(&'a self, positive: &mut Vec<&'a Node>, excluded: &mut Vec<&'a Node>) {
        match self {
            Node::And(left, right) => {
                left.conjunction_parts(positive, excluded);
                right.conjunction_parts(positive, excluded);
            }
            Node::Not(child) => excluded.push(child),
            _ => positive.push(self),
        }
    }

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
        result.validate_negation(false)?;
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
                    if field.is_some() {
                        return syntax("字段条件内不能再次指定字段，请将字段条件移到分组外");
                    }
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
                match field {
                    None => Ok(nested),
                    Some(field) => Self::apply_field(nested, field),
                }
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

    /// `field:(...)` 把字段下推到所有叶子,拒绝覆盖已有字段。
    fn apply_field(node: Node, field: Field) -> Result<Node, QueryError> {
        Ok(match node {
            Node::Term { field: Some(_), .. } | Node::Phrase { field: Some(_), .. } => {
                return syntax("字段分组内不能再次指定字段，请将字段条件移到分组外");
            }
            Node::Term { text, .. } => Node::Term {
                text,
                field: Some(field),
            },
            Node::Phrase { text, .. } => Node::Phrase {
                text,
                field: Some(field),
            },
            Node::Not(child) => Node::Not(Box::new(Self::apply_field(*child, field)?)),
            Node::And(l, r) => Node::And(
                Box::new(Self::apply_field(*l, field)?),
                Box::new(Self::apply_field(*r, field)?),
            ),
            Node::Or(l, r) => Node::Or(
                Box::new(Self::apply_field(*l, field)?),
                Box::new(Self::apply_field(*r, field)?),
            ),
        })
    }
}

/// FTS5 短语字面量:含 `*` 或 `"` 拒绝(`"` 双写转义后才有意义,但词内一律报错)。
fn quote(value: &str) -> Result<String, QueryError> {
    if value.is_empty() || value.contains(['"', '*']) {
        return syntax("词语包含不支持的查询字符");
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

/// 宽松模式的中文分词:jieba 内置词典 + 用户词典(见 [`crate::dict`]);
/// 无 jieba feature 时退化为整词。
#[cfg(feature = "jieba")]
pub fn cut_loose(term: &str) -> Vec<String> {
    dict::cut_loose(term)
}

/// 无 jieba feature:宽松模式退化为精确模式(不报错)。
#[cfg(not(feature = "jieba"))]
pub fn cut_loose(term: &str) -> Vec<String> {
    vec![term.to_owned()]
}

/// 把一个词项切成若干片段、再各自扩展成同义变体组的策略。
#[derive(Clone, Copy)]
struct TermCut {
    /// 精确 = 整词一段;宽松 = jieba 切段
    cut: fn(&str) -> Vec<String>,
    /// 正向展开同义词;排除侧换成 [`identity`],不展开
    expand: fn(&str) -> Vec<String>,
}

/// 恒等:精确模式的切分,以及排除侧的同义词扩展。
fn identity(term: &str) -> Vec<String> {
    vec![term.to_owned()]
}

impl TermCut {
    /// 排除侧策略:切词照旧,同义词不展开。
    ///
    /// 正向漏召回只是少几条结果,负向误杀却更难察觉,所以宁可保守。
    /// 双否定与嵌套排除已在解析阶段拒绝,排除子树整棵沿用此策略。
    fn excluding(self) -> Self {
        Self {
            expand: identity,
            ..self
        }
    }
}

/// 编译期累积的字面量与配额。
#[derive(Default)]
struct Accumulator {
    /// 全部正向条件的字面量:短语取原文,词在宽松模式下是切出的各段与同义变体;
    /// 去重并保持出现顺序
    literals: Vec<String>,
    seen: HashSet<String>,
    /// 同义词**额外**展开出的变体数(用户自己写进查询的词不占配额)
    extra: usize,
}

impl Accumulator {
    fn push_literal(&mut self, value: &str) {
        if self.seen.insert(value.to_owned()) {
            self.literals.push(value.to_owned());
        }
    }

    /// 把词项扩展成同义变体组并计入配额;不在词典里时是只含自身的单元素组。
    fn variants_of(&mut self, text: &str, term_cut: TermCut) -> Result<Vec<String>, QueryError> {
        let mut variants = (term_cut.expand)(text);
        if variants.is_empty() {
            variants.push(text.to_owned());
        }
        self.extra += variants.len() - 1;
        if self.extra > dict::MAX_EXTRA_VARIANTS {
            return syntax("同义词展开后条件过多,请精简词典");
        }
        Ok(variants)
    }
}

/// 把一组变体编译成不带字段前缀的 FTS5 条件:单个直接引用,多个用 OR 括起。
fn compile_variants(variants: &[String]) -> Result<String, QueryError> {
    if variants.len() == 1 {
        return quote(&variants[0]);
    }
    let mut quoted = Vec::with_capacity(variants.len());
    for variant in variants {
        quoted.push(quote(variant)?);
    }
    Ok(format!("({})", quoted.join(" OR ")))
}

/// 把查询编译成 FTS5 MATCH 表达式。
///
/// `loose = true` 时单词项经 `cut_loose` 切段,各段 AND 连接;`loose = false`
/// 时整词作为一个短语。正向条件的字面量收集进 [`CompiledQuery::literals`]
/// (去重、保序);排除侧既不展开同义词也不收集字面量。
pub fn compile(input: &str, scope: Scope, loose: bool) -> Result<CompiledQuery, QueryError> {
    compile_with(input, scope, loose, dict::expand)
}

/// 同 [`compile`],但同义词扩展由调用方提供(单测用独立词典,不碰全局状态)。
fn compile_with(
    input: &str,
    scope: Scope,
    loose: bool,
    expand: fn(&str) -> Vec<String>,
) -> Result<CompiledQuery, QueryError> {
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
    let cut: fn(&str) -> Vec<String> = if loose { cut_loose } else { identity };
    let term_cut = TermCut { cut, expand };
    let mut accumulator = Accumulator::default();

    let match_expr = compile_node(&ast, None, default_field, term_cut, true, &mut accumulator)?;
    Ok(CompiledQuery {
        match_expr,
        literals: accumulator.literals,
    })
}

fn compile_node(
    node: &Node,
    inherited: Option<Field>,
    default_field: Option<Field>,
    term_cut: TermCut,
    positive_ctx: bool,
    accumulator: &mut Accumulator,
) -> Result<String, QueryError> {
    match node {
        Node::Not(_) => syntax("排除条件必须与正向条件组合"),
        Node::And(..) => {
            let mut positive = Vec::new();
            let mut excluded = Vec::new();
            node.conjunction_parts(&mut positive, &mut excluded);
            let mut parts = positive.into_iter();
            let Some(first) = parts.next() else {
                return syntax("排除条件必须与正向条件组合");
            };
            let mut expression = compile_node(
                first,
                inherited,
                default_field,
                term_cut,
                positive_ctx,
                accumulator,
            )?;
            for part in parts {
                let next = compile_node(
                    part,
                    inherited,
                    default_field,
                    term_cut,
                    positive_ctx,
                    accumulator,
                )?;
                expression = format!("({expression} AND {next})");
            }
            // 每个排除条件分别做差集,与输入顺序、AND 括号位置无关。
            for part in excluded {
                let next = compile_node(
                    part,
                    inherited,
                    default_field,
                    term_cut.excluding(),
                    false,
                    accumulator,
                )?;
                expression = format!("({expression} NOT {next})");
            }
            Ok(expression)
        }
        Node::Or(left, right) => {
            let left = compile_node(
                left,
                inherited,
                default_field,
                term_cut,
                positive_ctx,
                accumulator,
            )?;
            let right = compile_node(
                right,
                inherited,
                default_field,
                term_cut,
                positive_ctx,
                accumulator,
            )?;
            Ok(format!("({left} OR {right})"))
        }
        Node::Phrase { text, field } => {
            let field = field.or(inherited).or(default_field);
            // 短语也查一次同义词表:组里写的若是整条短语,它同样能带出变体。
            let variants = accumulator.variants_of(text, term_cut)?;
            if positive_ctx {
                for variant in &variants {
                    accumulator.push_literal(variant);
                }
            }
            Ok(format!(
                "{}{}",
                field.map(Field::prefix).unwrap_or(""),
                compile_variants(&variants)?
            ))
        }
        Node::Term { text, field } => {
            let field = field.or(inherited).or(default_field);
            // 切词过滤之前检查语法,避免星号被当作无效标点丢弃。
            if text.contains(['*', '"']) {
                return syntax("词语包含不支持的查询字符");
            }
            if !crate::tokenize::has_tokens(text) {
                return syntax("普通词不包含可检索的文字或数字");
            }
            let mut pieces = (term_cut.cut)(text);
            pieces.retain(|part| !part.is_empty());
            if pieces.is_empty() {
                return syntax("普通词不包含可检索的文字或数字");
            }
            // 段内是「同义词 OR」,段间是 AND:多段时整体仍要套一层括号。
            let mut parts = Vec::with_capacity(pieces.len());
            for piece in &pieces {
                let variants = accumulator.variants_of(piece, term_cut)?;
                if positive_ctx {
                    for variant in &variants {
                        accumulator.push_literal(variant);
                    }
                }
                parts.push(compile_variants(&variants)?);
            }
            let body = if parts.len() == 1 {
                parts.pop().expect("刚判过长度为 1")
            } else {
                format!("({})", parts.join(" AND "))
            };
            Ok(format!(
                "{}{}",
                field.map(Field::prefix).unwrap_or(""),
                body
            ))
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
    fn nested_fields_are_rejected() {
        for input in [
            "title:(content:合同)",
            "content:(title:合同)",
            "title:(title:合同)",
            "content:(content:\"合同\")",
            "title:(合同 OR (发票 content:草稿))",
            "title:(合同 -content:草稿)",
            "title:(content:(合同 OR 发票))",
            "title:content:合同",
        ] {
            for scope in [Scope::All, Scope::Title, Scope::Content] {
                for loose in [false, true] {
                    let error = compile(input, scope, loose).unwrap_err();
                    assert!(error.0.contains("不能再次指定字段"), "{input}: {error}");
                }
            }
        }
    }

    #[test]
    fn nested_negations_are_rejected() {
        for input in [
            "合同 - -文档",
            "合同 NOT NOT 文档",
            "--电脑",
            "合同 -(文档 -草稿)",
            "合同 NOT (文档 NOT 草稿)",
            "合同 -((文档 OR (-草稿)))",
            "合同 -(-文档 -草稿)",
        ] {
            let error = compile(input, Scope::All, false).unwrap_err();
            assert!(
                error.0.contains("不支持双否定或嵌套排除"),
                "{input}: {error}"
            );
        }
    }

    #[test]
    fn exclusions_match_boolean_results_in_fts() {
        let conn = crate::store::open_in_memory().unwrap();
        for (index, text) in [
            "合同",
            "合同 文档",
            "合同 草稿",
            "合同 文档 草稿",
            "无关",
            "文档",
            "草稿",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO documents_fts(rowid, title, content) VALUES (?1, '', ?2)",
                rusqlite::params![index as i64 + 1, text],
            )
            .unwrap();
        }
        for (input, expected) in [
            ("-文档 -草稿 合同", vec![1]),
            ("合同 -文档 -草稿", vec![1]),
            ("NOT 文档 NOT 草稿 合同", vec![1]),
            ("合同 (-文档 -草稿)", vec![1]),
            ("-文档 (合同 -草稿)", vec![1]),
            ("content:(-文档 -草稿 合同)", vec![1]),
            ("合同 -content:文档 -content:草稿", vec![1]),
            ("合同 -title:文档", vec![1, 2, 3, 4]),
            ("(-文档 合同) -草稿", vec![1]),
            ("合同 -(文档 OR 草稿)", vec![1]),
            ("合同 -(文档 草稿)", vec![1, 2, 3]),
            ("(合同 -文档) OR (合同 -草稿)", vec![1, 2, 3]),
        ] {
            for loose in [false, true] {
                let query = compile_with(input, Scope::Content, loose, identity).unwrap();
                let actual: Vec<i64> = conn.prepare("SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY rowid").unwrap()
                    .query_map([&query.match_expr], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
                assert_eq!(actual, expected, "{input}: {}", query.match_expr);
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
            ("title:-合同", "缺少词语、短语或括号条件"),
            ("title:-(合同 OR 发票)", "缺少词语、短语或括号条件"),
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

    #[cfg(feature = "jieba")]
    #[test]
    fn loose_punctuation_uses_real_fts_tokens() {
        let conn = crate::store::open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO documents_fts(rowid, title, content) VALUES
            (1, '', '文档'), (2, '', '文、档'), (3, '', '文 档'),
            (4, '', '文其他档'), (5, '', '无关')",
        )
        .unwrap();
        for input in ["文、档", "文🙂档", "文！档"] {
            let query = compile_with(input, Scope::All, true, identity).unwrap();
            let actual: Vec<i64> = conn
                .prepare(
                    "SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY rowid",
                )
                .unwrap()
                .query_map([&query.match_expr], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(actual, [1, 2, 3, 4], "{input}: {}", query.match_expr);
            assert_eq!(query.literals, ["文", "档"]);
        }
        for (input, loose) in [("文、档", false), ("\"文、档\"", true)] {
            let query = compile_with(input, Scope::All, loose, identity).unwrap();
            assert_eq!(query.match_expr, "\"文、档\"");
            assert_eq!(query.literals, ["文、档"]);
            let actual: Vec<i64> = conn
                .prepare(
                    "SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY rowid",
                )
                .unwrap()
                .query_map([&query.match_expr], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(actual, [1, 2, 3]);
        }
    }

    #[test]
    fn tokenless_terms_are_errors_even_in_boolean_conditions() {
        for input in ["、！", "🙂", "合同 、", "合同 OR 🙂", "合同 -🙂"] {
            for loose in [false, true] {
                let error = compile(input, Scope::All, loose).unwrap_err();
                assert!(
                    error.0.contains("普通词不包含可检索的文字或数字"),
                    "{input}: {error}"
                );
            }
        }
        // 标点过滤不能让原本不支持的通配符悄悄变成合法输入。
        for input in ["文*档", "合同*"] {
            assert!(compile(input, Scope::All, true).is_err());
        }
        let phrase = compile("\"、！\"", Scope::All, true).unwrap();
        assert_eq!(phrase.match_expr, "\"、！\"");
    }

    #[cfg(not(feature = "jieba"))]
    #[test]
    fn loose_without_jieba_keeps_exact_semantics() {
        for input in ["文、档", "文🙂档", "A4", "\"文、档\""] {
            let loose = compile(input, Scope::All, true).unwrap();
            let exact = compile(input, Scope::All, false).unwrap();
            assert_eq!(loose.match_expr, exact.match_expr);
            assert_eq!(loose.literals, exact.literals);
        }
    }

    // ---------- 同义词展开 ----------
    //
    // 用 `compile_with` 注入固定词典,不碰 `dict` 的全局状态——测试并行跑,
    // 往全局表里写会让别处的期望值随机失败。

    /// 测试词典:`fn` 指针不能捕获上下文,所以放在静态表里。
    static TEST_GROUPS: &[&[&str]] = &[&["电脑", "计算机", "PC"], &["文档", "文件"]];

    fn test_expand(term: &str) -> Vec<String> {
        for group in TEST_GROUPS {
            if group.iter().any(|member| member.eq_ignore_ascii_case(term)) {
                return group.iter().map(|member| (*member).to_owned()).collect();
            }
        }
        vec![term.to_owned()]
    }

    #[test]
    fn synonyms_expand_terms_into_or_groups() {
        let query = compile_with("电脑", Scope::All, false, test_expand).unwrap();
        assert_eq!(query.match_expr, "(\"电脑\" OR \"计算机\" OR \"PC\")");
        // 扁平 OR:每个变体都作为展示定位的字面量。
        assert_eq!(query.literals, ["电脑", "计算机", "PC"]);
    }

    #[test]
    fn synonyms_apply_in_exact_mode_and_leave_plain_terms_alone() {
        // 精确模式同样展开:同义词与切词粒度无关。
        assert_eq!(
            compile_with("文件", Scope::All, false, test_expand)
                .unwrap()
                .match_expr,
            "(\"文档\" OR \"文件\")"
        );
        // 不在词典里的词项保持改动前的产物形态,不多套括号。
        let plain = compile_with("合同", Scope::All, false, test_expand).unwrap();
        assert_eq!(plain.match_expr, "\"合同\"");
        assert_eq!(plain.literals, ["合同"]);
    }

    #[test]
    fn quoted_phrases_expand_too() {
        assert_eq!(
            compile_with("\"文档\"", Scope::All, false, test_expand)
                .unwrap()
                .match_expr,
            "(\"文档\" OR \"文件\")"
        );
        // 整条短语不在任何组里时原样保留。
        assert_eq!(
            compile_with("\"文档 管理\"", Scope::All, false, test_expand)
                .unwrap()
                .match_expr,
            "\"文档 管理\""
        );
    }

    #[test]
    fn exclusions_do_not_expand_synonyms() {
        let query = compile_with("文档 -电脑", Scope::All, false, test_expand).unwrap();
        // 排除侧只排字面「电脑」,不排「计算机」/「PC」。
        assert_eq!(query.match_expr, "((\"文档\" OR \"文件\") NOT \"电脑\")");
        assert_eq!(query.literals, ["文档", "文件"]);
    }

    #[test]
    fn consecutive_exclusions_keep_synonyms_and_literals_separate() {
        let conn = crate::store::open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO documents_fts(rowid, title, content) VALUES
            (1, '', '文件 计算机'), (2, '', '文档 PC'),
            (3, '', '文档 电脑'), (4, '', '文件 草稿'), (5, '', '无关')",
        )
        .unwrap();
        for input in [
            "-电脑 -草稿 文档",
            "文档 (-电脑 -草稿)",
            "文档 -(电脑 OR 草稿)",
        ] {
            let query = compile_with(input, Scope::All, false, test_expand).unwrap();
            assert_eq!(query.literals, ["文档", "文件"]);
            let actual: Vec<i64> = conn
                .prepare(
                    "SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY rowid",
                )
                .unwrap()
                .query_map([&query.match_expr], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(actual, [1, 2], "{input}: {}", query.match_expr);
        }
    }

    #[test]
    fn synonym_expansion_is_capped() {
        let input = std::iter::repeat_n("电脑", 40)
            .collect::<Vec<_>>()
            .join(" ");
        let error = compile_with(&input, Scope::All, false, test_expand).unwrap_err();
        assert!(error.0.contains("同义词展开后条件过多"), "{error}");
        assert!(compile_with("电脑 合同", Scope::All, false, test_expand).is_ok());
    }

    /// 展开出来的 OR 组要能被真正的 FTS5 接受(尤其带字段前缀的形态)。
    #[test]
    fn expanded_groups_run_against_fts5() {
        let conn = crate::store::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO documents_fts(rowid, title, content) VALUES \
             (1, '计算机采购', '正文'), (2, '电脑采购', '正文'), (3, '无关', '正文')",
            [],
        )
        .unwrap();
        for scope in [Scope::All, Scope::Title] {
            let query = compile_with("电脑", scope, false, test_expand).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM documents_fts WHERE documents_fts MATCH ?1",
                    [&query.match_expr],
                    |row| row.get(0),
                )
                .unwrap_or_else(|error| panic!("{:?} 的产物被 FTS5 拒绝: {error}", scope));
            // 组里的「计算机」也命中,所以两篇都在。
            assert_eq!(count, 2, "scope={scope:?} expr={}", query.match_expr);
        }
    }
}
