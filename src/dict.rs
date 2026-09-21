//! 用户词典:jieba 分词用户词与同义词组。
//!
//! 两个文件都放在数据目录下(路径见 [`user_words_path`] / [`synonyms_path`]),
//! 都是纯文本、UTF-8 编码(失败时回退 GB18030),空行与 `#` 起首的行忽略:
//!
//! - `user_dict.txt` —— jieba 用户词,每行 `词 [词频] [词性]`,空格分隔。
//!   词频省略时交给 jieba 的 `suggest_freq` 推断,**不能**按 0 写入:
//!   `freq = 0` 的词在分词时会永远落选,还会覆盖内置词典里同名词的词频。
//! - `synonyms.txt` —— 同义词组,**每行一组**,组内是空格分隔的若干词。
//!   每个词只能出现在一组里(第二次出现按坏行报出),组内任意一个词都能带出整组。
//!
//! 两者都只在**查询期**生效:索引是逐字切词的(见 [`crate::tokenize`]),
//! 所以改词典不需要重建索引。加载不 panic,逐行报告问题由调用方展示。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use crate::tokenize;

/// 分词用户词文件名(数据目录下)。
pub const USER_WORDS_FILE: &str = "user_dict.txt";

/// 同义词组文件名(数据目录下)。
pub const SYNONYMS_FILE: &str = "synonyms.txt";

/// 单条词的字符数上限(更长的基本是把整段文本粘了进来)。
pub const MAX_TERM_CHARS: usize = 64;

/// 一个同义词组的成员数上限。
pub const MAX_GROUP_MEMBERS: usize = 32;

/// 全部词条数上限(防御性:同时保护加载时间)。
pub const MAX_ENTRIES: usize = 10_000;

/// 一次查询最多由同义词**额外**展开出的变体数。
///
/// 变体数直接决定 [`crate::search::locate_literals`] 的扫描成本(每个变体都要
/// 在正文里走一遍),所以必须有上限。用户自己写进查询的词不占这个配额,
/// 只有词典带来的额外变体算数。
pub const MAX_EXTRA_VARIANTS: usize = 64;

/// 报告里最多保留几条问题(其余只记「还有更多」)。
pub const MAX_REPORTED_PROBLEMS: usize = 5;

/// 一条加载问题(行号 + 中文原因;行号为 0 表示与具体某行无关)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "第 {} 行:{}", self.line, self.message)
        }
    }
}

/// 单个词典文件的加载结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    pub path: PathBuf,
    /// 文件不存在。不是错误:出厂状态就没有这两个文件。
    pub missing: bool,
    /// 成功加载的条目数(用户词 = 词条数;同义词 = 组数)。
    pub loaded: usize,
    /// 涉及的词数(同义词 = 各组词数之和;用户词同 `loaded`)。
    pub terms: usize,
    /// 前 [`MAX_REPORTED_PROBLEMS`] 条问题。
    pub problems: Vec<Problem>,
    /// 还有未列出的问题。
    pub problems_truncated: bool,
}

impl FileReport {
    fn push_problem(&mut self, line: usize, message: String) {
        if self.problems.len() < MAX_REPORTED_PROBLEMS {
            self.problems.push(Problem { line, message });
        } else {
            self.problems_truncated = true;
        }
    }
}

/// 一次加载的完整结果(两个文件)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DictReport {
    pub user_words: FileReport,
    pub synonyms: FileReport,
}

impl DictReport {
    /// 是否有需要用户处理的问题。
    pub fn has_problems(&self) -> bool {
        self.user_words.problems_truncated
            || self.synonyms.problems_truncated
            || !self.user_words.problems.is_empty()
            || !self.synonyms.problems.is_empty()
    }

    /// 两个文件都没有可用条目。
    pub fn is_empty(&self) -> bool {
        self.user_words.loaded == 0 && self.synonyms.loaded == 0
    }

    /// 一行摘要(设置页与 CLI 共用)。
    pub fn summary(&self) -> String {
        let synonyms = self.synonyms.loaded;
        let terms = self.synonyms.terms;
        let mut text = format!(
            "用户词 {} 条;同义词 {synonyms} 组 / {terms} 词",
            self.user_words.loaded
        );
        if self.is_empty() {
            text.push_str("(未配置词典)");
        }
        if self.has_problems() {
            text.push_str(";有格式问题");
        }
        text
    }
}

/// 数据目录下的分词用户词文件。
pub fn user_words_path(data_dir: &Path) -> PathBuf {
    data_dir.join(USER_WORDS_FILE)
}

/// 数据目录下的同义词文件。
pub fn synonyms_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SYNONYMS_FILE)
}

const USER_WORDS_TEMPLATE: &str = "\
# 分词用户词:每行「词 [词频] [词性]」,空格分隔。
# 词频留空则自动推断,建议留空。改完在设置页点「重新加载」即可生效。
# 只在检索的「宽松」模式下影响切词。
# 示例:
# 深度学习
# 向量数据库 2000
";

const SYNONYMS_TEMPLATE: &str = "\
# 同义词:每行一组,组内用空格分隔;一个词只能出现在一组里。
# 搜到组内任意一个词,都会同时召回含其它词的文档。
# 示例:
# 电脑 计算机 pc
# 文档 文件 档案
";

/// 文件不存在时写入带注释的模板(设置页「打开词典文件」用,便于用户起步)。
pub fn ensure_template(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let template = if path.file_name().and_then(|name| name.to_str()) == Some(SYNONYMS_FILE) {
        SYNONYMS_TEMPLATE
    } else {
        USER_WORDS_TEMPLATE
    };
    std::fs::write(path, template)
}

// ---------- 全局状态 ----------

/// 一个同义词组:组内所有词共享同一份成员表。
type Group = Arc<Vec<String>>;

/// 词(ASCII 小写后的形态,与逐字索引的大小写口径一致)→ 它所在的组。
type SynonymMap = HashMap<String, Group>;

/// 同义词表的查找键:ASCII 小写,所以词典里写 `PC` 也能被 `pc` 查到。
fn ascii_key(term: &str) -> String {
    term.to_ascii_lowercase()
}

static SYNONYMS: OnceLock<RwLock<SynonymMap>> = OnceLock::new();

fn synonyms_lock() -> &'static RwLock<SynonymMap> {
    SYNONYMS.get_or_init(|| RwLock::new(SynonymMap::new()))
}

/// 把一个词项扩展成它所在的同义词组(含自身);不在任何组里时只返回自身。
///
/// 只查一次表:跨行的合并已经在加载期完成,这里不递归、不会因为词典里的
/// 环而打转。
pub fn expand(term: &str) -> Vec<String> {
    let map = synonyms_lock()
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    lookup(&map, term)
}

/// [`expand`] 的纯函数形态(不碰全局状态,便于单测)。
fn lookup(map: &SynonymMap, term: &str) -> Vec<String> {
    match map.get(&ascii_key(term)) {
        Some(group) => group.as_ref().clone(),
        None => vec![term.to_owned()],
    }
}

#[cfg(feature = "jieba")]
static JIEBA: OnceLock<RwLock<jieba_rs::Jieba>> = OnceLock::new();

/// jieba 实例。没有用户词时保持惰性:首次用到才构建,与只依赖内置词典时一致。
#[cfg(feature = "jieba")]
fn jieba_lock() -> &'static RwLock<jieba_rs::Jieba> {
    JIEBA.get_or_init(|| RwLock::new(jieba_rs::Jieba::new()))
}

/// 用当前词典做 jieba 切词,只保留索引 tokenizer 能产生词元的片段。
#[cfg(feature = "jieba")]
pub fn cut_loose(term: &str) -> Vec<String> {
    let jieba = jieba_lock().read().unwrap_or_else(PoisonError::into_inner);
    jieba
        .cut(term, false)
        .into_iter()
        .filter(|token| crate::tokenize::has_tokens(token.word))
        .map(|token| token.word.to_owned())
        .collect()
}

// ---------- 加载 ----------

/// 从数据目录加载两个词典文件,并立即对后续查询生效(热重载走同一条路径)。
///
/// 文件不存在时把对应状态清空,所以删掉词典再重载就能回到「无词典」行为。
pub fn load(data_dir: &Path) -> DictReport {
    let (words, user_words) = load_user_words(&user_words_path(data_dir));
    let (groups, synonyms) = load_synonyms(&synonyms_path(data_dir));

    // 没有 jieba 时宽松模式退化为精确,用户词无处可用。
    #[cfg(not(feature = "jieba"))]
    let user_words = if words.is_empty() {
        user_words
    } else {
        let mut report = user_words;
        report.push_problem(0, "当前构建未启用 jieba,分词用户词不生效".to_owned());
        report
    };

    apply_user_words(&words);
    apply_synonyms(&groups);

    DictReport {
        user_words,
        synonyms,
    }
}

/// 读词典文件并解码:UTF-8 优先,失败回退 GB18030(与 `parse::parse_text` 同口径)。
///
/// 中文用户手上的词表常常是 GBK,直接 `read_to_string` 会整份失败。
fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => encoding_rs::GB18030.decode(&bytes).0.into_owned(),
    };
    // 记事本一类编辑器会写入 BOM,不剥掉的话第一行会多一个不可见字符。
    Ok(match text.strip_prefix('\u{feff}') {
        Some(stripped) => stripped.to_owned(),
        None => text,
    })
}

/// 逐行取出有效内容:去掉首尾空白,跳过空行与 `#` 起首的注释行。
///
/// `#` 只在**行首**起注释作用,这样 `C#` 这类词条仍可书写。
fn entries(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().filter_map(|(index, raw)| {
        let line = raw.trim();
        (!line.is_empty() && !line.starts_with('#')).then_some((index + 1, line))
    })
}

/// 词条合法性检查(分词用户词与同义词共用)。
fn term_problem(term: &str) -> Option<String> {
    if term.chars().count() > MAX_TERM_CHARS {
        return Some(format!("词条超过 {MAX_TERM_CHARS} 字上限"));
    }
    if term.contains(['*', '"']) {
        // `query::quote` 拒绝这两个字符,放行会让整条查询直接报错。
        return Some("词条不能包含 * 或 \"".to_owned());
    }
    if tokenize::token_spans(term).is_empty() {
        // 逐字索引对它不产生任何词元,FTS5 短语会退化成空条件。
        return Some("不含任何可索引字符".to_owned());
    }
    None
}

/// 一条分词用户词。
#[derive(Debug, Clone, PartialEq, Eq)]
struct UserWord {
    word: String,
    /// `None` = 交给 jieba 的 `suggest_freq` 推断。
    freq: Option<usize>,
    tag: Option<String>,
}

fn load_user_words(path: &Path) -> (Vec<UserWord>, FileReport) {
    match read_text(path) {
        Ok(text) => parse_user_words(&text, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            Vec::new(),
            FileReport {
                path: path.to_path_buf(),
                missing: true,
                ..FileReport::default()
            },
        ),
        Err(error) => {
            let mut report = FileReport {
                path: path.to_path_buf(),
                ..FileReport::default()
            };
            report.push_problem(0, format!("读取失败: {error}"));
            (Vec::new(), report)
        }
    }
}

/// 校验一条待追加的用户词(写文件之前拦下明显错误)。
pub fn check_user_word(line: &str) -> Result<(), String> {
    let (words, report) = parse_user_words(line, Path::new(USER_WORDS_FILE));
    match report.problems.first() {
        Some(problem) => Err(problem.message.clone()),
        None if words.len() == 1 => Ok(()),
        None => Err("请填写一个词条".to_owned()),
    }
}

/// 校验一条待追加的同义词组。
pub fn check_synonym_group(line: &str) -> Result<(), String> {
    let (groups, report) = parse_synonyms(line, Path::new(SYNONYMS_FILE));
    match report.problems.first() {
        Some(problem) => Err(problem.message.clone()),
        None if groups.len() == 1 => Ok(()),
        None => Err("同义词组至少要两个词(组内用空格分隔)".to_owned()),
    }
}

/// 按磁盘上的当前词典试解析追加结果,避免未重载的外部编辑漏过校验。
pub fn check_synonym_group_in_file(path: &Path, line: &str) -> Result<(), String> {
    check_synonym_group(line)?;
    let mut text = match read_text(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("读取词典文件失败: {error}")),
    };
    let (before, _) = parse_synonyms(&text, path);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let added_line = text.lines().count() + 1;
    text.push_str(line.trim());
    let (after, report) = parse_synonyms(&text, path);
    if after.len() == before.len() + 1 {
        return Ok(());
    }
    Err(report
        .problems
        .iter()
        .find(|problem| problem.line >= added_line)
        .map(|problem| problem.message.clone())
        .unwrap_or_else(|| "新增同义词组未通过校验:词条重复或已达词典容量上限".to_owned()))
}

/// 向词典文件追加一行(文件不存在时先写模板)。
///
/// 只追加、不重写:用户的编辑器可能同时开着这个文件,整份重写会丢改动。
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    ensure_template(path)?;
    let bytes = std::fs::read(path)?;
    // 与 read_text 使用相同的编码判定,避免给 GB18030 文件混入 UTF-8。
    let encoding = if std::str::from_utf8(&bytes).is_ok() {
        encoding_rs::UTF_8
    } else {
        encoding_rs::GB18030
    };
    let (encoded, _, had_errors) = encoding.encode(line);
    // 部分私用区字符虽可编码,解码后却会变成其它字符,同样不能静默写入。
    if had_errors
        || (encoding == encoding_rs::GB18030 && encoding.decode(&encoded).0.as_ref() != line)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "新增词条含有无法用 {} 无损编码的字符,请先将词典转为 UTF-8",
                encoding.name()
            ),
        ));
    }
    // 编码成功后才开始写入;末尾没换行时先补一个,避免粘到最后一行。
    let needs_newline = !bytes.ends_with(b"\n");
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    if needs_newline {
        file.write_all(b"\n")?;
    }
    file.write_all(&encoded)?;
    file.write_all(b"\n")
}

/// 解析用户词文本(不碰文件系统;单行的「快速添加」校验也走这里)。
fn parse_user_words(text: &str, path: &Path) -> (Vec<UserWord>, FileReport) {
    let mut report = FileReport {
        path: path.to_path_buf(),
        ..FileReport::default()
    };
    let mut words = Vec::new();
    for (line, raw) in entries(text) {
        if words.len() >= MAX_ENTRIES {
            report.push_problem(line, format!("已达 {MAX_ENTRIES} 条上限,其余忽略"));
            break;
        }
        let mut fields = raw.split_whitespace();
        let Some(word) = fields.next() else {
            continue;
        };
        if let Some(message) = term_problem(word) {
            report.push_problem(line, message);
            continue;
        }
        let freq = match fields.next() {
            None => None,
            Some(value) => match value.parse::<usize>() {
                Ok(0) => {
                    report.push_problem(line, "词频必须为正整数".to_owned());
                    continue;
                }
                Ok(freq) => Some(freq),
                Err(_) => {
                    report.push_problem(line, format!("词频「{value}」不是正整数"));
                    continue;
                }
            },
        };
        // 第 3 列(词性)读进来但不用:rsou 只做 `cut`,不做词性标注。
        words.push(UserWord {
            word: word.to_owned(),
            freq,
            tag: fields.next().map(str::to_owned),
        });
    }
    report.loaded = words.len();
    report.terms = words.len();
    (words, report)
}

fn load_synonyms(path: &Path) -> (Vec<Vec<String>>, FileReport) {
    match read_text(path) {
        Ok(text) => parse_synonyms(&text, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            Vec::new(),
            FileReport {
                path: path.to_path_buf(),
                missing: true,
                ..FileReport::default()
            },
        ),
        Err(error) => {
            let mut report = FileReport {
                path: path.to_path_buf(),
                ..FileReport::default()
            };
            report.push_problem(0, format!("读取失败: {error}"));
            (Vec::new(), report)
        }
    }
}

/// 解析同义词文本(不碰文件系统;单行的「快速添加」校验也走这里)。
fn parse_synonyms(text: &str, path: &Path) -> (Vec<Vec<String>>, FileReport) {
    let mut report = FileReport {
        path: path.to_path_buf(),
        ..FileReport::default()
    };
    let mut groups: Vec<Vec<String>> = Vec::new();
    // 词 → 已登记它的行号。每个词只能属于一组,第二次出现即坏行。
    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut total_terms = 0usize;

    for (line, raw) in entries(text) {
        let members: Vec<&str> = raw.split_whitespace().collect();
        if members.len() < 2 {
            report.push_problem(line, "同义词组至少要两个词(组内用空格分隔)".to_owned());
            continue;
        }
        if members.len() > MAX_GROUP_MEMBERS {
            report.push_problem(line, format!("一组最多 {MAX_GROUP_MEMBERS} 个词"));
            continue;
        }

        // 先整行校验,通过后再登记:中途失败不能留下半行的归属关系。
        let mut keys = Vec::with_capacity(members.len());
        let mut bad = false;
        for member in &members {
            if let Some(message) = term_problem(member) {
                report.push_problem(line, format!("「{member}」{message}"));
                bad = true;
                break;
            }
            let key = ascii_key(member);
            if let Some(first) = owner.get(&key) {
                report.push_problem(
                    line,
                    format!("「{member}」已在第 {first} 行出现过(每个词只能属于一组)"),
                );
                bad = true;
                break;
            }
            if keys.contains(&key) {
                report.push_problem(line, format!("「{member}」在同一行里重复出现"));
                bad = true;
                break;
            }
            keys.push(key);
        }
        if bad {
            continue;
        }
        if total_terms + members.len() > MAX_ENTRIES {
            report.push_problem(line, format!("已达 {MAX_ENTRIES} 词上限,其余忽略"));
            break;
        }
        for key in keys {
            owner.insert(key, line);
        }
        total_terms += members.len();
        groups.push(members.iter().map(|member| (*member).to_owned()).collect());
    }

    report.loaded = groups.len();
    report.terms = total_terms;
    (groups, report)
}

/// 按用户词重建 jieba 实例并装入全局状态。
///
/// 没有用户词时**不打断惰性**:若 jieba 还没建过就什么都不做,避免给不用宽松
/// 模式的用户增加启动成本;若已经建过(重载把词典清空了),退回内置词典。
fn apply_user_words(words: &[UserWord]) {
    #[cfg(not(feature = "jieba"))]
    {
        let _ = words;
    }
    #[cfg(feature = "jieba")]
    match (words.is_empty(), JIEBA.get()) {
        (true, None) => {}
        (true, Some(lock)) => {
            *lock.write().unwrap_or_else(PoisonError::into_inner) = jieba_rs::Jieba::new();
        }
        (false, _) => {
            let fresh = build_jieba(words);
            *jieba_lock().write().unwrap_or_else(PoisonError::into_inner) = fresh;
        }
    }
}

/// 内置词典 + 用户词。
///
/// 词频走 `add_word(word, None, _)`,由 jieba 的 `suggest_freq` 推断:它对已存在
/// 的词返回 `max(推断值, 现有词频)`,**不会把词频调低**,因此不会改变现有分词
/// 结果;固定写一个数字则会覆盖内置词典里的真实词频。
#[cfg(feature = "jieba")]
fn build_jieba(words: &[UserWord]) -> jieba_rs::Jieba {
    let mut jieba = jieba_rs::Jieba::new();
    for word in words {
        jieba.add_word(&word.word, word.freq, word.tag.as_deref());
    }
    jieba
}

/// 装入同义词表:组内所有词指向同一份成员表。
fn apply_synonyms(groups: &[Vec<String>]) {
    let mut map = SynonymMap::new();
    for group in groups {
        let shared: Group = Arc::new(group.clone());
        for member in group {
            map.insert(ascii_key(member), Arc::clone(&shared));
        }
    }
    *synonyms_lock()
        .write()
        .unwrap_or_else(PoisonError::into_inner) = map;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份临时数据目录;`None` 表示不写该文件。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsou-dict-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn map_of(groups: &[&[&str]]) -> SynonymMap {
        let owned: Vec<Vec<String>> = groups
            .iter()
            .map(|group| group.iter().map(|term| (*term).to_owned()).collect())
            .collect();
        let mut map = SynonymMap::new();
        for group in &owned {
            let shared: Group = Arc::new(group.clone());
            for member in group {
                map.insert(ascii_key(member), Arc::clone(&shared));
            }
        }
        map
    }

    #[test]
    fn lookup_returns_whole_group_and_is_case_insensitive() {
        let map = map_of(&[&["电脑", "计算机", "PC"], &["文档", "文件"]]);
        assert_eq!(lookup(&map, "电脑"), ["电脑", "计算机", "PC"]);
        // 组内任何一个词都能带出整组 —— 包括 ASCII 大小写不同的写法。
        assert_eq!(lookup(&map, "pc"), ["电脑", "计算机", "PC"]);
        assert_eq!(lookup(&map, "PC"), ["电脑", "计算机", "PC"]);
        assert_eq!(lookup(&map, "计算机"), ["电脑", "计算机", "PC"]);
        // 不在词典里:只返回自身。
        assert_eq!(lookup(&map, "无关词"), ["无关词"]);
    }

    #[test]
    fn groups_parse_one_group_per_line() {
        let dir = temp_dir("parse");
        std::fs::write(
            synonyms_path(&dir),
            "# 注释\n\n电脑 计算机 pc\n文档 文件\n计算机 台式机\n",
        )
        .unwrap();
        let (groups, report) = load_synonyms(&synonyms_path(&dir));
        // 「计算机」已在第 3 行出现,第 5 行整行作废(严格单行:一个词只属于一组)。
        assert_eq!(
            groups,
            vec![vec!["电脑", "计算机", "pc"], vec!["文档", "文件"]]
        );
        assert_eq!(report.loaded, 2);
        assert_eq!(report.terms, 5);
        assert!(report.problems.iter().any(|p| p.line == 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn synonym_problems_are_reported_with_line_numbers() {
        let dir = temp_dir("problems");
        std::fs::write(
            synonyms_path(&dir),
            "只有一个词\n带星号 a*b 词\n有效 组\n电 脑 电 脑\n",
        )
        .unwrap();
        let (groups, report) = load_synonyms(&synonyms_path(&dir));
        assert_eq!(groups, vec![vec!["有效", "组"]]);
        // 1:只有一词;2:含 `*`;4:同一行重复(空格分隔下「电 脑」是两组词)。
        let lines: Vec<usize> = report.problems.iter().map(|p| p.line).collect();
        assert_eq!(lines, [1, 2, 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let dir = temp_dir("missing");
        let (groups, report) = load_synonyms(&synonyms_path(&dir));
        assert!(groups.is_empty());
        assert!(report.missing);
        assert!(!report.problems_truncated);
        assert!(report.problems.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_words_parse_optional_frequency_and_tag() {
        let dir = temp_dir("words");
        std::fs::write(
            user_words_path(&dir),
            "# 注释\n深度学习\n向量数据库 2000\n知识图谱 3000 nz\n坏词频 abc\n",
        )
        .unwrap();
        let (words, report) = load_user_words(&user_words_path(&dir));
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].word, "深度学习");
        assert_eq!(words[0].freq, None, "省略词频应交给 suggest_freq 推断");
        assert_eq!(words[0].tag, None);
        assert_eq!(words[1].freq, Some(2000));
        assert_eq!(words[2].tag.as_deref(), Some("nz"));
        assert!(report.problems.iter().any(|p| p.line == 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gb18030_dictionary_is_decoded() {
        let dir = temp_dir("gbk");
        let (encoded, _, _) = encoding_rs::GB18030.encode("电脑 计算机 微机\n");
        std::fs::write(synonyms_path(&dir), &encoded[..]).unwrap();
        let (groups, report) = load_synonyms(&synonyms_path(&dir));
        assert_eq!(groups, vec![vec!["电脑", "计算机", "微机"]], "{report:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_add_validation_matches_the_file_parser() {
        assert!(check_user_word("深度学习").is_ok());
        assert!(check_user_word("电脑 1").is_ok());
        assert_eq!(check_user_word("电脑 0").unwrap_err(), "词频必须为正整数");
        assert!(check_user_word("向量数据库 2000 nz").is_ok());
        assert!(check_user_word("坏 词频").is_err());
        assert!(check_user_word("带星号 a*b").is_err());
        assert!(check_synonym_group("电脑 计算机").is_ok());
        assert!(check_synonym_group("只有一个词").is_err());
    }

    #[test]
    fn user_words_with_zero_frequency_are_reported_and_skipped() {
        let dir = temp_dir("zero-frequency");
        let path = user_words_path(&dir);
        std::fs::write(&path, "# 用户词\n电脑 0\n深度学习\n向量数据库 1 nz\n").unwrap();
        let (words, report) = load_user_words(&path);
        assert_eq!(report.loaded, 2);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].line, 2);
        assert_eq!(report.problems[0].message, "词频必须为正整数");
        assert_eq!(words[0].word, "深度学习");
        assert_eq!(words[0].freq, None);
        assert_eq!(words[1].word, "向量数据库");
        assert_eq!(words[1].freq, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_add_checks_current_file_before_writing() {
        let dir = temp_dir("append-conflicts");
        let path = synonyms_path(&dir);
        assert!(check_synonym_group_in_file(&path, "电脑 计算机").is_ok());
        assert!(!path.exists());
        // 不加载到全局表,直接模拟编辑器修改,并覆盖 GB18030 文件。
        let (original, _, _) = encoding_rs::GB18030.encode("电脑 计算机 PC");
        std::fs::write(&path, &original).unwrap();
        for line in ["电脑 微机", "pc 主机"] {
            let error = check_synonym_group_in_file(&path, line).unwrap_err();
            assert!(error.contains("已在第 1 行出现过"), "{error}");
            assert_eq!(std::fs::read(&path).unwrap(), original.as_ref());
        }
        assert!(check_synonym_group_in_file(&path, "文档 文件").is_ok());
        append_line(&path, "文档 文件").unwrap();
        assert_eq!(load_synonyms(&path).1.loaded, 2);
        // 外部删除原词组后,不应继续按旧的全局表拒绝。
        std::fs::write(&path, "文档 文件\n").unwrap();
        assert!(check_synonym_group_in_file(&path, "电脑 微机").is_ok());
        assert!(check_synonym_group_in_file(&dir, "电脑 微机").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_group_near_capacity_does_not_hide_later_valid_group() {
        let dir = temp_dir("synonym-capacity");
        let path = synonyms_path(&dir);
        let mut text = String::new();
        for index in 0..(MAX_ENTRIES / 2 - 1) {
            text.push_str(&format!("a{index} b{index}\n"));
        }
        text.push_str("* 无效 词组\n");
        std::fs::write(&path, &text).unwrap();
        assert!(check_synonym_group_in_file(&path, "文档 文件").is_ok());
        text.push_str("文档 文件\n");
        std::fs::write(&path, &text).unwrap();
        let (groups, report) = load_synonyms(&path);
        assert_eq!(report.terms, MAX_ENTRIES);
        assert_eq!(groups.last().unwrap(), &["文档", "文件"]);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].message.contains("不能包含"));
        assert!(check_synonym_group_in_file(&path, "电脑 计算机").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_creates_template_and_keeps_a_clean_break() {
        let dir = temp_dir("append");
        // 文件不存在:先写模板,再追加。
        append_line(&synonyms_path(&dir), "电脑 计算机").unwrap();
        let text = std::fs::read_to_string(synonyms_path(&dir)).unwrap();
        assert!(text.starts_with('#'));
        assert!(text.trim_end().ends_with("电脑 计算机"));

        // 手工写的文件没有收尾换行:追加要先补一个,不能粘到上一行。
        std::fs::write(synonyms_path(&dir), "文档 文件").unwrap();
        append_line(&synonyms_path(&dir), "电脑 计算机").unwrap();
        assert_eq!(
            std::fs::read_to_string(synonyms_path(&dir)).unwrap(),
            "文档 文件\n电脑 计算机\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_preserves_gb18030_synonyms() {
        let dir = temp_dir("append-gb18030-synonyms");
        let path = synonyms_path(&dir);
        for ending in ["", "\n", "\r\n"] {
            let original = format!("电脑 计算机{ending}");
            let (bytes, _, had_errors) = encoding_rs::GB18030.encode(&original);
            assert!(!had_errors);
            std::fs::write(&path, &bytes).unwrap();
            append_line(&path, "文档 文件").unwrap();
            // 四字节 GB18030 字符也应能无损追加。
            append_line(&path, "𠀀 生僻字").unwrap();
            assert!(std::fs::read(&path).unwrap().starts_with(&bytes));
            let (groups, report) = load_synonyms(&path);
            assert!(report.problems.is_empty(), "{report:?}");
            assert_eq!(report.loaded, 3);
            let refs: Vec<Vec<&str>> = groups
                .iter()
                .map(|group| group.iter().map(String::as_str).collect())
                .collect();
            let map = map_of(&refs.iter().map(Vec::as_slice).collect::<Vec<_>>());
            assert_eq!(lookup(&map, "文档"), vec!["文档", "文件"]);
            assert_eq!(lookup(&map, "𠀀"), vec!["𠀀", "生僻字"]);
            assert_eq!(lookup(&map, "电脑"), vec!["电脑", "计算机"]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_preserves_gb18030_user_words() {
        let dir = temp_dir("append-gb18030-words");
        let path = user_words_path(&dir);
        let (bytes, _, _) = encoding_rs::GB18030.encode("深度学习 100 nz");
        std::fs::write(&path, &bytes).unwrap();
        append_line(&path, "向量数据库 2000 nz").unwrap();
        let (words, report) = load_user_words(&path);
        assert!(report.problems.is_empty(), "{report:?}");
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].word, "深度学习");
        assert_eq!(words[1].word, "向量数据库");
        assert_eq!(words[1].freq, Some(2000));
        assert_eq!(words[1].tag.as_deref(), Some("nz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_line_rejects_unencodable_terms_without_changing_file() {
        let dir = temp_dir("append-unencodable");
        let path = synonyms_path(&dir);
        // U+E5E5 在 encoding_rs 的 GB18030 编码器中不可编码。
        let (bytes, _, _) = encoding_rs::GB18030.encode("电脑 计算机");
        std::fs::write(&path, &bytes).unwrap();
        // U+E843 可以编码,但解码后会变成 U+9FB9,也必须拒绝。
        for line in ["文档 文件\u{e5e5}", "文档 文件\u{e843}"] {
            let error = append_line(&path, line).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(encoding_rs::GB18030.name()));
            assert_eq!(std::fs::read(&path).unwrap(), bytes.as_ref());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn template_is_written_only_when_missing() {
        let dir = temp_dir("template");
        let path = synonyms_path(&dir);
        ensure_template(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.starts_with('#'));
        std::fs::write(&path, "电脑 计算机\n").unwrap();
        ensure_template(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "电脑 计算机\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "jieba")]
    mod jieba_user_words {
        use super::*;

        fn words(jieba: &jieba_rs::Jieba, text: &str) -> Vec<String> {
            jieba
                .cut(text, false)
                .into_iter()
                .map(|token| token.word.to_owned())
                .collect()
        }

        fn word(text: &str) -> UserWord {
            UserWord {
                word: text.to_owned(),
                freq: None,
                tag: None,
            }
        }

        #[test]
        fn omitted_frequency_still_joins_the_user_word() {
            let jieba = build_jieba(&[word("深度检索")]);
            assert!(
                words(&jieba, "深度检索功能")
                    .iter()
                    .any(|w| w == "深度检索"),
                "{:?}",
                words(&jieba, "深度检索功能")
            );
        }

        /// 关键回归:省略词频不能把内置词典里已有词的词频改坏。
        #[test]
        fn user_word_already_in_default_dict_keeps_segmentation() {
            let plain = jieba_rs::Jieba::new();
            let with_user = build_jieba(&[word("电脑")]);
            for text in ["电脑的配置", "笔记本电脑", "电脑"] {
                assert_eq!(
                    words(&with_user, text),
                    words(&plain, text),
                    "「{text}」的分词结果不应被改变"
                );
            }
        }

        #[test]
        fn rejected_zero_frequency_keeps_default_segmentation() {
            let (user_words, report) = parse_user_words("电脑 0", Path::new(USER_WORDS_FILE));
            assert_eq!(report.problems.len(), 1);
            let with_user = build_jieba(&user_words);
            assert_eq!(words(&with_user, "电脑的配置"), ["电脑", "的", "配置"]);
        }
    }
}
