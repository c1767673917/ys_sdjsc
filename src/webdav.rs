//! WebDAV 异步客户端 (等价 app/core/webdav_client.py)
//!
//! 重试/异常语义与原实现逐分支对齐, 包括:
//! - 401/403/404 立即抛出不重试;
//! - 5xx/超时/网络错误按 WEBDAV_RETRY_COUNT 间隔 WEBDAV_RETRY_DELAY 重试;
//! - 405/409 立即抛出(目录已存在等);
//! - 其它非成功状态码不等待直接进入下一次尝试, 最终抛出 "WebDAV请求失败"。

use base64::Engine;
use reqwest::{Method, Response};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;

use crate::config;
use crate::pyutil::{basename, beijing_now_iso, dirname, quote, unquote};

const LOG: &str = "app.core.webdav_client";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Generic,
    Auth,
    Permission,
    NotFound,
    Timeout,
    Network,
    Server,
}

#[derive(Debug, Clone)]
pub struct WebDavError {
    pub kind: Kind,
    pub message: String,
    pub status_code: Option<u16>,
}

impl fmt::Display for WebDavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WebDavError {}

impl WebDavError {
    fn new(kind: Kind, message: impl Into<String>, status_code: Option<u16>) -> Self {
        WebDavError { kind, message: message.into(), status_code }
    }
}

/// 格式化 reqwest 错误; 常见网络错误使用与原实现(httpx)一致的文案
pub fn describe_reqwest_error(e: &reqwest::Error) -> String {
    use std::error::Error;
    let chain = {
        let mut s = String::new();
        let mut cur: Option<&dyn Error> = e.source();
        while let Some(x) = cur {
            s.push_str(&x.to_string().to_lowercase());
            s.push(' ');
            cur = x.source();
        }
        s
    };
    if e.is_timeout() {
        return "timed out".to_string();
    }
    if e.is_connect() {
        if chain.contains("dns") || chain.contains("lookup") || chain.contains("resolve") {
            return "[Errno -2] Name or service not known".to_string();
        }
        return "All connection attempts failed".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&dyn Error> = e.source();
    while let Some(s) = cur {
        let t = s.to_string();
        if !parts.contains(&t) {
            parts.push(t);
        }
        cur = s.source();
    }
    if parts.is_empty() {
        return e.to_string();
    }
    parts.join(": ")
}

/// httpx 风格的请求日志: HTTP Request: GET url "HTTP/1.1 200 OK"
pub fn log_http_request(method: &str, url: &str, resp: &Response) {
    let status = resp.status();
    log::info!(
        target: "httpx",
        "HTTP Request: {} {} \"{:?} {} {}\"",
        method,
        url,
        resp.version(),
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
}

#[derive(Clone)]
pub struct WebDavClient {
    base_url: String,
    base_path: String,
    auth_string: String,
    retry_count: i64,
    retry_delay: i64,
    client: reqwest::Client,
}

pub struct DavResponse {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: bytes::Bytes,
}

impl WebDavClient {
    pub fn new() -> Self {
        let s = config::get();
        let base_url = s.webdav_url.trim_end_matches('/').to_string();
        let base_path = s.webdav_base_path.trim_matches('/').to_string();
        let auth_string = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", s.webdav_username, s.webdav_password).as_bytes());
        let timeout = Duration::from_secs(s.webdav_timeout as u64);
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .expect("构建HTTP客户端失败");

        log::info!(target: LOG, "WebDAV客户端初始化完成: {}", base_url);
        log::debug!(target: LOG, "基础路径: {}", base_path);
        log::debug!(target: LOG, "超时设置: {}秒", s.webdav_timeout);
        log::debug!(target: LOG, "重试次数: {}", s.webdav_retry_count);

        WebDavClient {
            base_url,
            base_path,
            auth_string,
            retry_count: s.webdav_retry_count,
            retry_delay: s.webdav_retry_delay,
            client,
        }
    }

    /// 获取完整的WebDAV URL
    pub fn full_url(&self, path: &str) -> String {
        let path = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
        let full_path = if self.base_path.is_empty() {
            path
        } else {
            format!("/{}{}", self.base_path, path)
        };
        format!("{}{}", self.base_url, quote(&full_path, "/"))
    }

    async fn sleep_retry(&self) {
        tokio::time::sleep(Duration::from_secs(self.retry_delay.max(0) as u64)).await;
    }

    /// 发起HTTP请求，包含重试机制
    pub async fn make_request(
        &self,
        method: &str,
        path: &str,
        content: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<DavResponse, WebDavError> {
        let url = self.full_url(path);
        let settings = config::get();
        let mut last_error: Option<String> = None;
        let m = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);

        let mut attempt: i64 = 0;
        while attempt <= self.retry_count {
            let this_attempt = attempt;
            attempt += 1;
            if settings.webdav_debug {
                log::debug!(target: LOG, "WebDAV请求 [{}/{}]: {} {}", this_attempt + 1, self.retry_count + 1, method, url);
            }

            let mut req = self
                .client
                .request(m.clone(), &url)
                .header("Authorization", format!("Basic {}", self.auth_string))
                .header("User-Agent", format!("UploadManager/{}", settings.app_version));
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if let Some(c) = &content {
                req = req.body(c.clone());
            }

            // 发送请求并读取完整响应体 (等价 httpx 非流式请求)
            let sent: Result<DavResponse, reqwest::Error> = async {
                let resp = req.send().await?;
                log_http_request(method, &url, &resp);
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let body = resp.bytes().await?;
                Ok(DavResponse { status, headers, body })
            }
            .await;

            let resp = match sent {
                Err(e) if e.is_timeout() => {
                    let msg = "WebDAV请求超时".to_string();
                    last_error = Some(msg.clone());
                    if this_attempt < self.retry_count {
                        log::warn!(target: LOG, "{}，{}秒后重试...", msg, self.retry_delay);
                        self.sleep_retry().await;
                        continue;
                    }
                    return Err(WebDavError::new(Kind::Timeout, msg, None));
                }
                Err(e) => {
                    let msg = format!("WebDAV网络错误: {}", describe_reqwest_error(&e));
                    last_error = Some(msg.clone());
                    if this_attempt < self.retry_count {
                        log::warn!(target: LOG, "{}，{}秒后重试...", msg, self.retry_delay);
                        self.sleep_retry().await;
                        continue;
                    }
                    return Err(WebDavError::new(Kind::Network, msg, None));
                }
                Ok(r) => r,
            };

            if settings.webdav_debug {
                log::debug!(target: LOG, "WebDAV响应: {}", resp.status);
            }

            let raised = match resp.status {
                200 | 201 | 204 | 207 => return Ok(resp),
                401 => WebDavError::new(Kind::Auth, "WebDAV认证失败，请检查用户名和密码", Some(401)),
                403 => WebDavError::new(Kind::Permission, "WebDAV权限不足，请检查目录权限", Some(403)),
                404 => WebDavError::new(Kind::NotFound, format!("WebDAV路径不存在: {}", path), Some(404)),
                s if s >= 500 => {
                    let msg = format!("WebDAV服务器错误: {}", s);
                    last_error = Some(msg.clone());
                    if this_attempt < self.retry_count {
                        log::warn!(target: LOG, "{}，{}秒后重试...", msg, self.retry_delay);
                        self.sleep_retry().await;
                        continue;
                    }
                    WebDavError::new(Kind::Server, msg, Some(s))
                }
                s => WebDavError::new(
                    Kind::Generic,
                    format!("WebDAV请求失败: {} - {}", s, String::from_utf8_lossy(&resp.body)),
                    Some(s),
                ),
            };

            // 原实现在 try 内抛出上述异常后由通用 except 处理:
            match raised.kind {
                Kind::Auth | Kind::Permission | Kind::NotFound => return Err(raised),
                _ => {
                    if matches!(raised.status_code, Some(405) | Some(409)) {
                        return Err(raised);
                    }
                    // 其它 WebDAVError: 不等待, 直接进入下一次尝试
                }
            }
        }

        Err(WebDavError::new(
            Kind::Generic,
            last_error.unwrap_or_else(|| "WebDAV请求失败".to_string()),
            None,
        ))
    }

    /// 检查WebDAV服务健康状态
    pub async fn health_check(&self) -> bool {
        log::debug!(target: LOG, "执行WebDAV健康检查...");
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n<D:propfind xmlns:D=\"DAV:\">\n    <D:prop>\n        <D:resourcetype/>\n    </D:prop>\n</D:propfind>";
        match self
            .make_request(
                "PROPFIND",
                "/",
                Some(xml.as_bytes().to_vec()),
                &[("Depth", "0"), ("Content-Type", "application/xml; charset=utf-8")],
            )
            .await
        {
            Ok(resp) => {
                let ok = resp.status == 200 || resp.status == 207;
                if ok {
                    log::debug!(target: LOG, "WebDAV健康检查通过");
                } else {
                    log::warn!(target: LOG, "WebDAV健康检查失败: {}", resp.status);
                }
                ok
            }
            Err(e) => {
                log::error!(target: LOG, "WebDAV健康检查异常: {}", e);
                false
            }
        }
    }

    /// 上传文件到WebDAV并做完整性校验 (GET 回读比对 SHA256)
    pub async fn upload_file(&self, local_path: &str, webdav_path: &str) -> Map<String, Value> {
        let r: Result<Map<String, Value>, String> = async {
            if !std::path::Path::new(local_path).exists() {
                return Err(format!("本地文件不存在: {}", local_path));
            }
            let file_size = std::fs::metadata(local_path).map(|m| m.len()).map_err(|e| e.to_string())?;
            log::info!(target: LOG, "开始上传文件: {} -> {} ({} bytes)", local_path, webdav_path, file_size);

            // 确保目标目录存在
            let directory = dirname(webdav_path);
            if !directory.is_empty() && directory != "/" {
                self.create_directory(&directory).await;
            }

            let file_content = tokio::fs::read(local_path).await.map_err(|e| e.to_string())?;

            let resp = self
                .make_request("PUT", webdav_path, Some(file_content.clone()), &[])
                .await
                .map_err(|e| e.to_string())?;

            let etag = resp
                .headers
                .get("ETag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .trim_matches('"')
                .to_string();

            // 上传后完整性校验
            self.verify_remote_content(webdav_path, &file_content, 3, 2.0).await?;

            let mut m = Map::new();
            m.insert("success".into(), Value::Bool(true));
            m.insert("webdav_path".into(), Value::String(webdav_path.to_string()));
            m.insert("file_size".into(), Value::from(file_size));
            m.insert("etag".into(), Value::String(etag));
            m.insert("upload_time".into(), Value::String(beijing_now_iso()));
            log::info!(target: LOG, "文件上传成功: {}", webdav_path);
            Ok(m)
        }
        .await;

        match r {
            Ok(m) => m,
            Err(e) => {
                let error_msg = format!("文件上传失败: {}", e);
                log::error!(target: LOG, "{}", error_msg);
                let mut m = Map::new();
                m.insert("success".into(), Value::Bool(false));
                m.insert("error".into(), Value::String(error_msg));
                m.insert("webdav_path".into(), Value::String(webdav_path.to_string()));
                m
            }
        }
    }

    /// GET回读远端文件并与预期内容比对, 不一致时返回错误
    pub async fn verify_remote_content(
        &self,
        webdav_path: &str,
        expected: &[u8],
        attempts: u32,
        delay: f64,
    ) -> Result<(), String> {
        let expected_size = expected.len();
        let expected_hash = Sha256::digest(expected);
        let mut last_error = String::new();

        for attempt in 1..=attempts {
            match self.download_file(webdav_path).await {
                Ok(remote) => {
                    if remote.len() == expected_size && Sha256::digest(&remote) == expected_hash {
                        return Ok(());
                    }
                    last_error = if remote.len() != expected_size {
                        format!("本地大小={}字节, 远端实际大小={}字节", expected_size, remote.len())
                    } else {
                        "远端内容SHA256与本地不一致".to_string()
                    };
                }
                Err(e) => {
                    last_error = format!("无法回读远端文件 - {}", e);
                }
            }
            if attempt < attempts {
                log::warn!(
                    target: LOG,
                    "WebDAV回读校验未通过 [{}/{}] {}: {}, {}秒后重试...",
                    attempt,
                    attempts,
                    webdav_path,
                    last_error,
                    crate::pyutil::py_float_repr(delay)
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }
        Err(format!("WebDAV完整性校验失败: {}", last_error))
    }

    /// GET回读远端文件得到的实际字节数, 文件不存在时返回None
    pub async fn get_remote_actual_size(&self, webdav_path: &str) -> Result<Option<usize>, String> {
        match self.download_file(webdav_path).await {
            Ok(c) => Ok(Some(c.len())),
            Err(e) => {
                if e.contains("不存在") || e.contains("404") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// 从WebDAV下载文件
    pub async fn download_file(&self, webdav_path: &str) -> Result<Vec<u8>, String> {
        log::debug!(target: LOG, "开始下载文件: {}", webdav_path);
        match self.make_request("GET", webdav_path, None, &[]).await {
            Ok(resp) => {
                log::debug!(target: LOG, "文件下载成功: {} ({} bytes)", webdav_path, resp.body.len());
                Ok(resp.body.to_vec())
            }
            Err(e) => {
                let error_msg = format!("文件下载失败: {}", e);
                log::error!(target: LOG, "{}", error_msg);
                log::error!(target: LOG, "异步函数 download_file 执行失败: {}", error_msg);
                Err(error_msg)
            }
        }
    }

    /// 删除WebDAV文件
    pub async fn delete_file(&self, webdav_path: &str) -> bool {
        log::debug!(target: LOG, "开始删除文件: {}", webdav_path);
        match self.make_request("DELETE", webdav_path, None, &[]).await {
            Ok(_) => {
                log::debug!(target: LOG, "文件删除成功: {}", webdav_path);
                true
            }
            Err(e) => {
                log::error!(target: LOG, "文件删除失败: {}", e);
                false
            }
        }
    }

    /// 递归创建目录 (405 视为已存在)
    pub fn create_directory<'a>(
        &'a self,
        path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if path.is_empty() || path == "/" {
                return true;
            }
            log::debug!(target: LOG, "创建目录: {}", path);
            let mut p = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
            p = p.trim_end_matches('/').to_string();

            let parent = dirname(&p);
            if !parent.is_empty() && parent != "/" {
                self.create_directory(&parent).await;
            }

            let mkcol_path = if p.ends_with('/') { p.clone() } else { format!("{}/", p) };
            match self.make_request("MKCOL", &mkcol_path, None, &[]).await {
                Ok(_) => {
                    log::debug!(target: LOG, "目录创建成功: {}", mkcol_path);
                    true
                }
                Err(e) => {
                    if e.status_code == Some(405) {
                        log::debug!(target: LOG, "目录已存在: {}", p);
                        return true;
                    }
                    log::error!(target: LOG, "目录创建失败: {}", e);
                    false
                }
            }
        })
    }

    /// 列出目录中的文件 (PROPFIND Depth 1)
    pub async fn list_files(&self, path: &str) -> Result<Vec<Map<String, Value>>, String> {
        let r: Result<Vec<Map<String, Value>>, String> = async {
            log::debug!(target: LOG, "列出目录内容: {}", path);
            let path = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
            let xml = "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n<D:propfind xmlns:D=\"DAV:\">\n    <D:prop>\n        <D:displayname/>\n        <D:getcontentlength/>\n        <D:getlastmodified/>\n        <D:resourcetype/>\n        <D:getetag/>\n    </D:prop>\n</D:propfind>";
            let resp = self
                .make_request(
                    "PROPFIND",
                    &path,
                    Some(xml.as_bytes().to_vec()),
                    &[("Depth", "1"), ("Content-Type", "application/xml; charset=utf-8")],
                )
                .await
                .map_err(|e| e.to_string())?;

            let text = String::from_utf8_lossy(&resp.body).into_owned();
            let doc = roxmltree::Document::parse(&text).map_err(|e| e.to_string())?;
            let is = |n: &roxmltree::Node, name: &str| {
                n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some("DAV:")
            };

            let mut files = Vec::new();
            for response_elem in doc.descendants().filter(|n| is(n, "response")) {
                let Some(href_elem) = response_elem.children().find(|n| is(n, "href")) else {
                    continue;
                };
                let href_text = href_elem
                    .text()
                    .ok_or_else(|| "expected string or bytes-like object, got 'NoneType'".to_string())?;
                let href = unquote(href_text);

                // 跳过根目录
                if href == path || href == path.trim_end_matches('/') {
                    continue;
                }

                let (name, is_directory) = if href.ends_with('/') {
                    (basename(href.trim_end_matches('/')), true)
                } else {
                    (basename(&href), false)
                };

                let props = response_elem
                    .children()
                    .filter(|n| is(n, "propstat"))
                    .find_map(|ps| ps.children().find(|n| is(n, "prop")));
                let Some(props) = props else { continue };

                let mut info = Map::new();
                info.insert("name".into(), Value::String(name));
                info.insert("path".into(), Value::String(href.clone()));
                info.insert("is_directory".into(), Value::Bool(is_directory));

                if let Some(size_elem) = props.children().find(|n| is(n, "getcontentlength")) {
                    let t = size_elem.text().ok_or_else(|| {
                        "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                            .to_string()
                    })?;
                    let v: i64 = t
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid literal for int() with base 10: '{}'", t))?;
                    info.insert("size".into(), Value::from(v));
                }
                if let Some(m) = props.children().find(|n| is(n, "getlastmodified")) {
                    info.insert(
                        "modified".into(),
                        m.text().map(|t| Value::String(t.to_string())).unwrap_or(Value::Null),
                    );
                }
                if let Some(et) = props.children().find(|n| is(n, "getetag")) {
                    let t = et
                        .text()
                        .ok_or_else(|| "'NoneType' object has no attribute 'strip'".to_string())?;
                    info.insert("etag".into(), Value::String(t.trim_matches('"').to_string()));
                }
                files.push(info);
            }
            log::debug!(target: LOG, "列出目录完成，共{}个项目", files.len());
            Ok(files)
        }
        .await;

        r.map_err(|e| {
            let error_msg = format!("列出目录失败: {}", e);
            log::error!(target: LOG, "{}", error_msg);
            error_msg
        })
    }
}
