//! 显式触发的 OCR 客户端;普通文档解析不会调用此模块。
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client as HttpClient, multipart};
use serde_json::Value;

use crate::parse::{ParseError, ParseErrorCode, Parsed};

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:9003";

/// 已保存的设置优先,其次为启动环境变量,最后为本机默认地址。
pub fn configured_base_url(
    conn: &rusqlite::Connection,
    environment: Option<&str>,
) -> anyhow::Result<String> {
    let saved = crate::repo::get_setting(conn, "ocr_base_url")?;
    normalize_base_url(saved.as_deref().or(environment).unwrap_or(DEFAULT_BASE_URL))
}

/// 统一校验与规范化,保存设置和创建客户端使用同一规则。
pub fn normalize_base_url(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| anyhow::anyhow!("请输入有效的 OCR 服务地址,例如 http://127.0.0.1:9003"))?;
    anyhow::ensure!(
        url.scheme() == "http" && url.host_str().is_some(),
        "OCR 服务地址须以 http:// 开头并包含主机名或 IP 地址"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "OCR 服务地址不能包含用户名、密码、查询参数或片段"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn save_base_url(conn: &rusqlite::Connection, value: &str) -> anyhow::Result<String> {
    let value = normalize_base_url(value)?;
    crate::repo::set_setting(conn, "ocr_base_url", &value)?;
    Ok(value)
}

pub struct Client {
    http: HttpClient,
    base_url: String,
}

fn failure(message: &str, detail: impl ToString) -> ParseError {
    ParseError {
        code: ParseErrorCode::NeedsOcr,
        message: message.to_owned(),
        detail: detail.to_string(),
    }
}

impl Client {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let base_url = normalize_base_url(base_url)?;
        Ok(Self {
            http: HttpClient::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()?,
            base_url,
        })
    }

    /// 只检查服务信息接口,不上传文档或创建识别任务。
    pub fn test_connection(&self) -> anyhow::Result<String> {
        use anyhow::Context;
        let response = self
            .http
            .get(format!("{}/api/info", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .context("无法连接 OCR 服务,请检查地址和服务状态")?
            .error_for_status()
            .context("OCR 服务返回错误状态")?;
        let info: Value = response
            .json()
            .context("OCR 服务返回的内容不是有效的 JSON")?;
        let engine = info["engine"]
            .as_str()
            .filter(|v| !v.trim().is_empty())
            .context("响应缺少识别引擎信息,请确认地址指向 OCR 服务")?;
        let workers = info["workers"]
            .as_u64()
            .filter(|v| *v > 0)
            .context("OCR 服务未报告有效的工作进程数")?;
        Ok(format!("连接成功（{engine},工作进程数 {workers}）"))
    }

    /// 完整识别后才返回正文。取消和任何服务错误仍归入 NEEDS_OCR,允许再次批量重试。
    pub fn parse_pdf(&self, bytes: Vec<u8>, cancel: &AtomicBool) -> Result<Parsed, ParseError> {
        let check = || {
            if cancel.load(Ordering::Relaxed) {
                Err(failure("已取消 OCR", ""))
            } else {
                Ok(())
            }
        };
        check()?;
        let part = multipart::Part::bytes(bytes).file_name("document.pdf");
        let response = self
            .http
            .post(format!("{}/api/ocr/pdf", self.base_url))
            .multipart(multipart::Form::new().part("file", part))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<Value>())
            .map_err(|e| failure("OCR 提交失败,请检查服务后重试", e))?;
        let id = response["job_id"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| failure("OCR 服务返回无效任务编号", ""))?;
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            check()?;
            if Instant::now() >= deadline {
                return Err(failure("OCR 等待超时,可重新识别", ""));
            }
            let result = self
                .http
                .get(format!("{}/api/ocr/job/{id}?wait=2", self.base_url))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Value>())
                .map_err(|e| failure("OCR 查询失败,请检查服务后重试", e))?;
            check()?;
            match result["status"].as_str() {
                Some("pending" | "running") => {
                    // 兼容立即返回的服务,避免忙轮询。
                    std::thread::sleep(Duration::from_millis(100));
                }
                Some("done") => return completed(&result),
                Some("partial" | "timeout" | "failed") => {
                    return Err(failure(
                        "OCR 未完整成功,可重新识别",
                        result["error"]
                            .as_str()
                            .unwrap_or("存在失败页、缺页或任务超时"),
                    ));
                }
                _ => return Err(failure("OCR 服务返回未知任务状态", "")),
            }
        }
    }
}

fn completed(result: &Value) -> Result<Parsed, ParseError> {
    if result["complete"] != true || result["truncated"] != false {
        return Err(failure("OCR 结果不完整,可重新识别", ""));
    }
    let markdown = result["markdown"].as_str().unwrap_or_default();
    if crate::text::markdown_to_plain(markdown)
        .text
        .trim()
        .is_empty()
    {
        return Err(failure("OCR 未识别出可用文字,请检查扫描质量", ""));
    }
    Ok(Parsed {
        markdown: markdown.to_owned(),
        warnings: Vec::new(),
        parser_name: "ocrapi",
        parser_version: "v1",
    })
}
