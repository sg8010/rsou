//! 文档解析:扩展名路由、anydoc 调用、txt/md 自读与中文错误映射。
//!
//! 路由:
//! - `.txt/.md` 自己读:BOM 去除 → UTF-8 严格 → GB18030 回退;
//! - 其余支持的扩展名走 anydoc:`Format::from_bytes` 探测、`from_path` 兜底;
//! - 不支持的扩展名在导入扫描时就被丢弃,不会走到这里。

use std::path::Path;

use anydoc::{ConvertError, Format};

/// 文档类别(与 documents.file_type 的取值一致)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Word,
    Excel,
    Ppt,
    Pdf,
    Text,
    Epub,
}

impl FileType {
    /// 入库值(小写)。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Word => "word",
            Self::Excel => "excel",
            Self::Ppt => "ppt",
            Self::Pdf => "pdf",
            Self::Text => "text",
            Self::Epub => "epub",
        }
    }

    /// 界面显示名。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Word => "Word",
            Self::Excel => "Excel",
            Self::Ppt => "PPT",
            Self::Pdf => "PDF",
            Self::Text => "文本",
            Self::Epub => "电子书",
        }
    }
}

/// 全部支持的扩展名(小写、不含点)。文件对话框过滤器与导入扫描共用这一份清单。
pub fn supported_extensions() -> &'static [&'static str] {
    &[
        "doc", "docx", "docm", "odt", "rtf", // word
        "xls", "xlsx", "xlsm", "xlsb", "ods", "csv", // excel
        "ppt", "pps", "pot", "pptx", "pptm", "ppsx", "ppsm", "odp", // ppt
        "pdf", // pdf
        "txt", "md",   // text
        "epub", // epub
    ]
}

/// 扩展名 → 文档类别;None = 不支持(导入时直接丢弃,不落库)。
pub fn file_type_of(path: &Path) -> Option<FileType> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "doc" | "docx" | "docm" | "odt" | "rtf" => FileType::Word,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | "csv" => FileType::Excel,
        "ppt" | "pps" | "pot" | "pptx" | "pptm" | "ppsx" | "ppsm" | "odp" => FileType::Ppt,
        "pdf" => FileType::Pdf,
        "txt" | "md" => FileType::Text,
        "epub" => FileType::Epub,
        _ => return None,
    })
}

/// 解析失败码(大写蛇形,对应 documents.parse_error_code)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorCode {
    Unsupported,
    Encrypted,
    Malformed,
    ResourceLimit,
    NeedsOcr,
    Io,
    TooLarge,
    Empty,
}

impl ParseErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unsupported => "UNSUPPORTED",
            Self::Encrypted => "ENCRYPTED",
            Self::Malformed => "MALFORMED",
            Self::ResourceLimit => "RESOURCE_LIMIT",
            Self::NeedsOcr => "NEEDS_OCR",
            Self::Io => "IO",
            Self::TooLarge => "TOO_LARGE",
            Self::Empty => "EMPTY",
        }
    }
}

/// 解析失败:`message` 是给用户看的中文原因,`detail` 是原始技术细节(可空)。
#[derive(Debug)]
pub struct ParseError {
    pub code: ParseErrorCode,
    pub message: String,
    pub detail: String,
}

impl ParseError {
    fn new(code: ParseErrorCode, message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}({})", self.message, self.detail)
        }
    }
}

impl std::error::Error for ParseError {}

/// 一份解析产物:统一 GFM Markdown + 警告列表。
#[derive(Debug)]
pub struct Parsed {
    pub markdown: String,
    pub warnings: Vec<String>,
    pub parser_name: &'static str,
    pub parser_version: &'static str,
}

/// 解析一段已读入的字节(不产生 I/O;路径只用于扩展名路由与 anydoc 兜底探测)。
pub fn parse_bytes(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    match file_type_of(path) {
        Some(FileType::Text) => parse_text(bytes),
        Some(_) => parse_anydoc(path, bytes),
        None => Err(ParseError::new(
            ParseErrorCode::Unsupported,
            "无法识别的格式,或该格式无法转换(例如纯图片 PDF)",
            format!("扩展名不支持: {}", path.display()),
        )),
    }
}

/// 读文件:先按元数据判超限,再整体读入(导入流程在解析前先算内容哈希)。
pub fn read_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ParseError> {
    let meta = std::fs::metadata(path).map_err(|e| io_error(path, &e))?;
    if meta.len() > max_bytes {
        return Err(ParseError::new(
            ParseErrorCode::TooLarge,
            format!("文件超过 {} MB 上限", max_bytes / (1024 * 1024)),
            format!("{} 字节", meta.len()),
        ));
    }
    std::fs::read(path).map_err(|e| io_error(path, &e))
}

/// 读文件并解析(`read_file` + `parse_bytes` 的组合)。
/// 返回解析产物与原始字节(调用方算内容哈希)。
pub fn parse_file(path: &Path, max_bytes: u64) -> Result<(Parsed, Vec<u8>), ParseError> {
    let bytes = read_file(path, max_bytes)?;
    let parsed = parse_bytes(path, &bytes)?;
    Ok((parsed, bytes))
}

fn io_error(path: &Path, error: &std::io::Error) -> ParseError {
    ParseError::new(
        ParseErrorCode::Io,
        "无法读取文件(权限/占用/路径失效)",
        format!("{}: {error}", path.display()),
    )
}

/// txt/md 自读:BOM 去除 → UTF-8 严格 → GB18030 回退;统一换行为 `\n`。
fn parse_text(bytes: &[u8]) -> Result<Parsed, ParseError> {
    let mut warnings = Vec::new();
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut text = match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            warnings.push("按 GB18030 解码".to_owned());
            let (cow, _, _) = encoding_rs::GB18030.decode(bytes);
            cow.into_owned()
        }
    };
    // UTF-8 BOM 已在字节层去掉;这里兜底 U+FEFF(例如 GB18030 解码出的 BOM)。
    if text.starts_with('\u{FEFF}') {
        text = text.trim_start_matches('\u{FEFF}').to_owned();
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.trim().is_empty() {
        return Err(ParseError::new(
            ParseErrorCode::Empty,
            "文本文件为空或没有可用内容",
            "",
        ));
    }
    Ok(Parsed {
        markdown: text,
        warnings,
        parser_name: "text",
        parser_version: "text.v1",
    })
}

fn parse_anydoc(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let format = Format::from_bytes(bytes).or_else(|| Format::from_path(path));
    let Some(format) = format else {
        return Err(ParseError::new(
            ParseErrorCode::Unsupported,
            "无法识别的格式,或该格式无法转换(例如纯图片 PDF)",
            format!("内容与扩展名均无法判定格式: {}", path.display()),
        ));
    };
    let markdown =
        anydoc::to_markdown_bytes(bytes, Some(format)).map_err(|e| map_convert_error(&e))?;
    if markdown.trim().is_empty() {
        return Err(ParseError::new(
            ParseErrorCode::Empty,
            "文档没有可提取的文本",
            "",
        ));
    }
    Ok(Parsed {
        markdown,
        warnings: Vec::new(),
        parser_name: "anydoc",
        parser_version: "0.2.4",
    })
}

/// ConvertError → 中文失败原因。
/// ConvertError 是 #[non_exhaustive],未知变体兜底按 Malformed 处理。
pub fn map_convert_error(error: &ConvertError) -> ParseError {
    match error {
        ConvertError::Unsupported(what) => ParseError::new(
            ParseErrorCode::Unsupported,
            "无法识别的格式,或该格式无法转换(例如纯图片 PDF)",
            what.clone(),
        ),
        ConvertError::Encrypted => ParseError::new(
            ParseErrorCode::Encrypted,
            "文件已加密或受密码保护",
            error.to_string(),
        ),
        ConvertError::Malformed { part, detail } => ParseError::new(
            ParseErrorCode::Malformed,
            "文件结构损坏,或缺少必要组成部件",
            match part {
                Some(part) => format!("{part}: {detail}"),
                None => detail.clone(),
            },
        ),
        ConvertError::MissingPart { part } => ParseError::new(
            ParseErrorCode::Malformed,
            "文件结构损坏,或缺少必要组成部件",
            format!("缺少 {part}"),
        ),
        ConvertError::ResourceLimit { limit, detail } => ParseError::new(
            ParseErrorCode::ResourceLimit,
            "文件超出安全上限(解压体积/节点数/表格格数)",
            format!("{limit}: {detail}"),
        ),
        ConvertError::NeedsOcr { pages, page_count } => ParseError::new(
            ParseErrorCode::NeedsOcr,
            "该 PDF 没有可提取文本,需要 OCR,本版本不支持",
            format!("{pages:?}/{page_count} 页需要 OCR"),
        ),
        ConvertError::Io(e) => ParseError::new(
            ParseErrorCode::Io,
            "无法读取文件(权限/占用/路径失效)",
            e.to_string(),
        ),
        _ => ParseError::new(
            ParseErrorCode::Malformed,
            "文件结构损坏,或缺少必要组成部件",
            error.to_string(),
        ),
    }
}
