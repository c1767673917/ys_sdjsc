//! HTTP 公共层: 复现 FastAPI/Starlette 的对外行为
//! - HTTPException -> `{"detail": ...}`
//! - 请求参数校验失败 -> 422 `{"detail": [{type, loc, msg, input, ...}]}`
//! - FileResponse / StaticFiles 的响应头
//! - CORSMiddleware(allow_origins=*, allow_credentials=True)
//! - 未匹配路由 404 / 方法不允许 405 / 尾斜杠 307 重定向
//! - uvicorn 风格访问日志

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::Path;

const PYDANTIC_URL: &str = "https://errors.pydantic.dev/2.5/v/";

/// HTTPException 等价物
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: Value,
}

impl ApiError {
    pub fn new(status: u16, detail: impl Into<String>) -> Self {
        ApiError { status: StatusCode::from_u16(status).unwrap(), detail: Value::String(detail.into()) }
    }
    pub fn not_found() -> Self {
        Self::new(404, "Not Found")
    }
    pub fn validation(errors: Vec<Value>) -> Self {
        ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, detail: Value::Array(errors) }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(self.status, &json!({"detail": self.detail}))
    }
}

pub type ApiResult = Result<Response, ApiError>;

/// JSON 响应 (紧凑格式, 与 Starlette JSONResponse 一致)
pub fn json_response(status: StatusCode, v: &Value) -> Response {
    let body = serde_json::to_vec(v).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

pub fn ok_json(v: Value) -> ApiResult {
    Ok(json_response(StatusCode::OK, &v))
}

pub fn ok_map(m: Map<String, Value>) -> ApiResult {
    ok_json(Value::Object(m))
}

// ---------------------------------------------------------------------------
// pydantic 风格的校验错误
// ---------------------------------------------------------------------------

fn err_item(kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Value>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(kind));
    m.insert("loc".into(), Value::Array(loc));
    m.insert("msg".into(), json!(msg));
    m.insert("input".into(), input);
    if let Some(c) = ctx {
        m.insert("ctx".into(), c);
    }
    m.insert("url".into(), json!(format!("{}{}", PYDANTIC_URL, kind)));
    Value::Object(m)
}

pub fn missing(loc: Vec<Value>) -> Value {
    err_item("missing", loc, "Field required", Value::Null, None)
}

fn loc2(a: &str, b: &str) -> Vec<Value> {
    vec![json!(a), json!(b)]
}

// ---------------------------------------------------------------------------
// 查询参数
// ---------------------------------------------------------------------------

/// 原始查询参数 (同名取最后一个, 与 Starlette QueryParams.get 一致)
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    pub fn parse(raw: Option<&str>) -> Self {
        let pairs = raw
            .map(|q| {
                form_urlencoded_parse(q)
            })
            .unwrap_or_default();
        Query { pairs }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// 必填字符串参数
    pub fn required_str(&self, key: &str, errors: &mut Vec<Value>) -> String {
        match self.get(key) {
            Some(v) => v,
            None => {
                errors.push(missing(loc2("query", key)));
                String::new()
            }
        }
    }

    /// 带默认值与 ge/le 约束的整数参数
    pub fn int(&self, key: &str, default: i64, ge: Option<i64>, le: Option<i64>, errors: &mut Vec<Value>) -> i64 {
        let Some(raw) = self.get(key) else { return default };
        match parse_int_lax(&raw) {
            None => {
                errors.push(err_item(
                    "int_parsing",
                    loc2("query", key),
                    "Input should be a valid integer, unable to parse string as an integer",
                    json!(raw),
                    None,
                ));
                default
            }
            Some(v) => {
                if let Some(g) = ge {
                    if v < g {
                        errors.push(err_item(
                            "greater_than_equal",
                            loc2("query", key),
                            &format!("Input should be greater than or equal to {}", g),
                            json!(raw),
                            Some(json!({"ge": g})),
                        ));
                    }
                }
                if let Some(l) = le {
                    if v > l {
                        errors.push(err_item(
                            "less_than_equal",
                            loc2("query", key),
                            &format!("Input should be less than or equal to {}", l),
                            json!(raw),
                            Some(json!({"le": l})),
                        ));
                    }
                }
                v
            }
        }
    }

    pub fn bool(&self, key: &str, default: bool, errors: &mut Vec<Value>) -> bool {
        let Some(raw) = self.get(key) else { return default };
        match crate::config::parse_bool(&raw) {
            Some(b) => b,
            None => {
                errors.push(err_item(
                    "bool_parsing",
                    loc2("query", key),
                    "Input should be a valid boolean, unable to interpret input",
                    json!(raw),
                    None,
                ));
                default
            }
        }
    }
}

fn form_urlencoded_parse(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            // Starlette 使用 parse_qsl(keep_blank_values=True)
            let dec = |s: &str| crate::pyutil::unquote(&s.replace('+', " "));
            Some((dec(k), dec(v)))
        })
        .collect()
}

/// pydantic 宽松模式 str -> int
pub fn parse_int_lax(s: &str) -> Option<i64> {
    crate::config::parse_int(s)
}

/// 路径参数 int 校验
pub fn path_int(name: &str, raw: &str) -> Result<i64, ApiError> {
    parse_int_lax(raw).ok_or_else(|| {
        ApiError::validation(vec![err_item(
            "int_parsing",
            loc2("path", name),
            "Input should be a valid integer, unable to parse string as an integer",
            json!(raw),
            None,
        )])
    })
}

pub fn check(errors: Vec<Value>) -> Result<(), ApiError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation(errors))
    }
}

// ---------------------------------------------------------------------------
// JSON 请求体 (pydantic 模型的宽松校验)
// ---------------------------------------------------------------------------

/// 解析 JSON 请求体为对象; 失败返回 422
pub fn json_body(bytes: &[u8]) -> Result<Map<String, Value>, ApiError> {
    if bytes.is_empty() {
        return Err(ApiError::validation(vec![missing(vec![json!("body")])]));
    }
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            // FastAPI 手工构造该错误(不含 url), ctx.error 为 Python json 模块的报错信息
            return Err(ApiError::validation(vec![json!({
                "type": "json_invalid",
                "loc": ["body", json_error_pos(bytes, &e)],
                "msg": "JSON decode error",
                "input": {},
                "ctx": {"error": python_json_error_msg(&e)},
            })]))
        }
    };
    match v {
        Value::Object(m) => Ok(m),
        other => Err(ApiError::validation(vec![err_item(
            "model_attributes_type",
            vec![json!("body")],
            "Input should be a valid dictionary or object to extract fields from",
            other,
            None,
        )])),
    }
}

/// 模型字段缺失: input 为整个请求体对象
fn missing_field(m: &Map<String, Value>, loc: Vec<Value>) -> Value {
    err_item("missing", loc, "Field required", Value::Object(m.clone()), None)
}

/// serde_json 错误位置 -> 字符偏移 (近似 Python JSONDecodeError.pos)
pub fn json_error_pos(bytes: &[u8], e: &serde_json::Error) -> usize {
    let text = String::from_utf8_lossy(bytes);
    let mut pos = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i + 1 == e.line() {
            return pos + e.column().saturating_sub(1).min(line.chars().count());
        }
        pos += line.chars().count() + 1;
    }
    pos
}

/// serde_json 错误 -> Python json 模块的错误信息
pub fn python_json_error_msg(e: &serde_json::Error) -> &'static str {
    let m = e.to_string();
    if m.contains("key must be a string") {
        "Expecting property name enclosed in double quotes"
    } else if m.contains("expected `,` or `}`") || m.contains("expected `,` or `]`") {
        "Expecting ',' delimiter"
    } else if m.contains("expected `:`") {
        "Expecting ':' delimiter"
    } else if m.contains("trailing characters") {
        "Extra data"
    } else if m.contains("control character") || m.contains("EOF while parsing a string") {
        "Unterminated string starting at"
    } else {
        "Expecting value"
    }
}

pub fn body_bool(m: &Map<String, Value>, key: &str, default: Option<bool>, errors: &mut Vec<Value>) -> bool {
    let loc = vec![json!("body"), json!(key)];
    match m.get(key) {
        None => {
            if default.is_none() {
                errors.push(missing_field(m, loc));
            }
            default.unwrap_or(false)
        }
        Some(v) => match lax_bool(v) {
            Ok(b) => b,
            Err((kind, msg)) => {
                errors.push(err_item(kind, loc, msg, v.clone(), None));
                false
            }
        },
    }
}

fn lax_bool(v: &Value) -> Result<bool, (&'static str, &'static str)> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f == 0.0 => Ok(false),
            Some(f) if f == 1.0 => Ok(true),
            _ => Err(("bool_parsing", "Input should be a valid boolean, unable to interpret input")),
        },
        Value::String(s) => crate::config::parse_bool(s)
            .ok_or(("bool_parsing", "Input should be a valid boolean, unable to interpret input")),
        _ => Err(("bool_type", "Input should be a valid boolean")),
    }
}

pub fn body_str(m: &Map<String, Value>, key: &str, errors: &mut Vec<Value>) -> String {
    let loc = vec![json!("body"), json!(key)];
    match m.get(key) {
        None => {
            errors.push(missing_field(m, loc));
            String::new()
        }
        Some(Value::String(s)) => s.clone(),
        Some(v) => {
            errors.push(err_item("string_type", loc, "Input should be a valid string", v.clone(), None));
            String::new()
        }
    }
}

pub fn body_int_list(m: &Map<String, Value>, key: &str, errors: &mut Vec<Value>) -> Vec<i64> {
    let loc = || vec![json!("body"), json!(key)];
    match m.get(key) {
        None => {
            errors.push(missing_field(m, loc()));
            vec![]
        }
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let mut l = loc();
                l.push(json!(i));
                match it {
                    Value::Bool(b) => out.push(*b as i64),
                    Value::Number(n) => {
                        if let Some(v) = n.as_i64() {
                            out.push(v);
                        } else if let Some(f) = n.as_f64() {
                            if f.fract() == 0.0 {
                                out.push(f as i64);
                            } else {
                                errors.push(err_item(
                                    "int_from_float",
                                    l,
                                    "Input should be a valid integer, got a number with a fractional part",
                                    it.clone(),
                                    None,
                                ));
                            }
                        }
                    }
                    Value::String(s) => match parse_int_lax(s) {
                        Some(v) => out.push(v),
                        None => errors.push(err_item(
                            "int_parsing",
                            l,
                            "Input should be a valid integer, unable to parse string as an integer",
                            it.clone(),
                            None,
                        )),
                    },
                    _ => errors.push(err_item("int_type", l, "Input should be a valid integer", it.clone(), None)),
                }
            }
            out
        }
        Some(v) => {
            errors.push(err_item("list_type", loc(), "Input should be a valid list", v.clone(), None));
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// 文件响应
// ---------------------------------------------------------------------------

/// Starlette 的 Content-Disposition: ASCII 文件名用引号, 否则用 RFC 5987 编码
pub fn content_disposition(kind: &str, filename: &str) -> String {
    let quoted = crate::pyutil::quote(filename, "");
    if quoted != filename {
        format!("{}; filename*=utf-8''{}", kind, quoted)
    } else {
        format!("{}; filename=\"{}\"", kind, filename)
    }
}

/// mimetypes.guess_type 的近似, text/* 追加 charset
pub fn guess_media_type(path: &str) -> String {
    let lower = path.to_lowercase();
    let m = if lower.ends_with(".js") {
        "text/javascript".to_string()
    } else {
        mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string()
    };
    if m.starts_with("text/") {
        format!("{}; charset=utf-8", m)
    } else {
        m
    }
}

/// FileResponse(path, media_type=None, filename=None, headers)
pub async fn file_response(
    path: &str,
    media_type: Option<&str>,
    filename: Option<&str>,
    extra_headers: &[(&str, String)],
) -> Result<Response, String> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?;
    let body = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let size = meta.len();
    let etag = {
        use md5::{Digest, Md5};
        let base = format!("{}-{}", crate::pyutil::py_float_repr(mtime), size);
        hex::encode(Md5::digest(base.as_bytes()))
    };
    let last_modified = httpdate(mtime);
    let ct = match media_type {
        Some(m) => {
            if m.starts_with("text/") && !m.contains("charset") {
                format!("{}; charset=utf-8", m)
            } else {
                m.to_string()
            }
        }
        None => guess_media_type(path),
    };

    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::LAST_MODIFIED, last_modified)
        .header(header::ETAG, format!("\"{}\"", etag));
    let mut has_cd = false;
    for (k, v) in extra_headers {
        if k.eq_ignore_ascii_case("content-disposition") {
            has_cd = true;
        }
        b = b.header(*k, header_value(v)?);
    }
    if let (Some(f), false) = (filename, has_cd) {
        b = b.header(header::CONTENT_DISPOSITION, header_value(&content_disposition("attachment", f))?);
    }
    b.body(Body::from(body)).map_err(|e| e.to_string())
}

/// Starlette 以 latin-1 编码响应头; 无法编码时抛出与 Python 相同的错误
pub fn header_value(v: &str) -> Result<HeaderValue, String> {
    let mut bytes = Vec::with_capacity(v.len());
    for (i, c) in v.chars().enumerate() {
        let code = c as u32;
        if code > 0xFF {
            let end = v.chars().skip(i).take_while(|c| (*c as u32) > 0xFF).count() + i;
            let pos = if end - i > 1 {
                format!("characters in position {}-{}", i, end - 1)
            } else {
                format!("character '\\u{:04x}' in position {}", code, i)
            };
            return Err(format!("'latin-1' codec can't encode {}: ordinal not in range(256)", pos));
        }
        bytes.push(code as u8);
    }
    HeaderValue::from_bytes(&bytes).map_err(|e| e.to_string())
}

fn httpdate(ts: f64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0).unwrap_or_default();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// StaticFiles: 在 directory 下查找 rel 路径; 不存在/目录/越界 -> 404
pub async fn static_file(directory: &str, rel: &str, req_headers: &HeaderMap) -> Response {
    let base = match std::fs::canonicalize(directory) {
        Ok(b) => b,
        Err(_) => return ApiError::not_found().into_response(),
    };
    let rel = rel.trim_start_matches('/');
    let joined = Path::new(directory).join(rel);
    let Ok(full) = std::fs::canonicalize(&joined) else {
        return ApiError::not_found().into_response();
    };
    if !full.starts_with(&base) || !full.is_file() {
        return ApiError::not_found().into_response();
    }
    let p = full.to_string_lossy().to_string();
    match file_response(&p, Some(&guess_media_type(rel)), None, &[]).await {
        Ok(resp) => {
            // 条件请求: If-None-Match / If-Modified-Since -> 304
            let etag = resp.headers().get(header::ETAG).cloned();
            if let (Some(inm), Some(et)) = (req_headers.get(header::IF_NONE_MATCH), etag.as_ref()) {
                let inm = inm.to_str().unwrap_or("");
                let et = et.to_str().unwrap_or("");
                if inm.split(',').any(|t| t.trim().trim_start_matches("W/") == et) {
                    let mut r = Response::builder().status(StatusCode::NOT_MODIFIED);
                    for k in [header::ETAG, header::LAST_MODIFIED] {
                        if let Some(v) = resp.headers().get(&k) {
                            r = r.header(k, v.clone());
                        }
                    }
                    return r.body(Body::empty()).unwrap();
                }
            }
            resp
        }
        Err(_) => ApiError::not_found().into_response(),
    }
}

// ---------------------------------------------------------------------------
// 中间件
// ---------------------------------------------------------------------------

/// CORSMiddleware(allow_origins=["*"], allow_credentials=True, allow_methods=["*"], allow_headers=["*"])
pub async fn cors(req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).cloned();
    let Some(origin) = origin else {
        return next.run(req).await;
    };
    let is_preflight = req.method() == Method::OPTIONS
        && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    if is_preflight {
        let mut b = Response::builder()
            .status(StatusCode::OK)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone())
            .header(header::VARY, "Origin")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT")
            .header(header::ACCESS_CONTROL_MAX_AGE, "600")
            .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
        if let Some(h) = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
            b = b.header(header::ACCESS_CONTROL_ALLOW_HEADERS, h.clone());
        }
        return b.body(Body::from("OK")).unwrap();
    }
    let has_cookie = req.headers().contains_key(header::COOKIE);
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    if has_cookie {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.append(header::VARY, HeaderValue::from_static("Origin"));
    } else {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    resp
}

/// uvicorn 风格访问日志: INFO:     ip:port - "GET /path HTTP/1.1" 200 OK
pub async fn access_log(req: Request, next: Next) -> Response {
    let addr = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "-".into());
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let version = format!("{:?}", req.version()).replace("HTTP/", "");
    let resp = next.run(req).await;
    let st = resp.status();
    println!(
        "INFO:     {} - \"{} {} HTTP/{}\" {} {}",
        addr,
        method,
        path,
        version,
        st.as_u16(),
        st.canonical_reason().unwrap_or("")
    );
    resp
}

/// 405 统一为 JSON {"detail": "Method Not Allowed"}
pub async fn method_not_allowed_json(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        // FastAPI 每个方法是独立路由, 405 时 Allow 只列出首个匹配路由的方法, 且不含 HEAD
        let allow = resp
            .headers()
            .get(header::ALLOW)
            .and_then(|a| a.to_str().ok())
            .and_then(|a| a.split(',').map(|m| m.trim().to_string()).find(|m| m != "HEAD" && !m.is_empty()));
        let mut r = ApiError::new(405, "Method Not Allowed").into_response();
        if let Some(a) = allow {
            r.headers_mut().insert(header::ALLOW, HeaderValue::from_str(&a).unwrap());
        }
        return r;
    }
    resp
}
