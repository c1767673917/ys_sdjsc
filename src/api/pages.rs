//! 页面与静态资源路由 (等价 app/main.py 中的页面路由和 StaticFiles 挂载)

use axum::extract::{Path as AxPath, RawQuery, Request};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::config;
use crate::delivery_sync;
use crate::http::{self, check, ok_json, ApiError, ApiResult, Query};
use crate::pyutil::py_isdigit;
use crate::upload_types::VALID_DOC_TYPES;

pub const ADMIN_COOKIE_NAME: &str = "admin_token";
pub const ADMIN_COOKIE_MAX_AGE: i64 = 7 * 86400;

async fn html(path: &str) -> ApiResult {
    http::file_response(path, Some("text/html"), None, &[])
        .await
        .map_err(|e| ApiError::new(500, e))
}

/// GET / — 上传页面入口 (?business_id=&doc_number=&doc_type=)
pub async fn upload_page(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Query::parse(raw.as_deref());
    let mut errors = Vec::new();
    let business_id = q.required_str("business_id", &mut errors);
    let _doc_number = q.required_str("doc_number", &mut errors);
    let doc_type = q.required_str("doc_type", &mut errors);
    check(errors)?;

    if business_id.is_empty() || !py_isdigit(&business_id) {
        return Err(ApiError::new(400, "business_id必须为纯数字"));
    }
    if !VALID_DOC_TYPES.contains(&doc_type.as_str()) {
        return Err(ApiError::new(
            400,
            format!("doc_type必须为以下值之一: {}", VALID_DOC_TYPES.join(", ")),
        ));
    }
    html("app/static/index.html").await
}

/// GET /api/health
pub async fn health() -> ApiResult {
    let s = config::get();
    ok_json(json!({"status": "healthy", "app": s.app_name, "version": s.app_version}))
}

/// 管理页面: 返回页面并种下 HttpOnly 鉴权 Cookie
pub async fn admin_page_response(path: &str) -> ApiResult {
    let s = config::get();
    let mut resp = html(path).await?;
    let mut cookie = format!(
        "{}={}; HttpOnly; Max-Age={}; Path=/; SameSite=lax",
        ADMIN_COOKIE_NAME, s.admin_token, ADMIN_COOKIE_MAX_AGE
    );
    if s.admin_cookie_secure {
        cookie.push_str("; Secure");
    }
    resp.headers_mut()
        .append(header::SET_COOKIE, cookie.parse().unwrap());
    Ok(resp)
}

pub async fn admin_page() -> ApiResult {
    admin_page_response("app/pages/admin.html").await
}

pub async fn logistics_links_page() -> ApiResult {
    admin_page_response("app/pages/logistics-links.html").await
}

/// GET /l/{token} — 物流待上传门户页面, 无效token 404
pub async fn logistics_portal_page(AxPath(token): AxPath<String>) -> ApiResult {
    match delivery_sync::get_token_row(&token).await.map_err(|e| ApiError::new(500, e))? {
        None => Err(ApiError::new(404, "链接无效或已失效")),
        Some(_) => html("app/static/logistics-portal.html").await,
    }
}

/// /static/*
pub async fn static_files(req: Request) -> Response {
    let rel = req.uri().path().strip_prefix("/static").unwrap_or("").to_string();
    http::static_file("app/static", &rel, req.headers()).await
}

/// /uploaded_files/* — LOCAL_STORAGE_PATH 静态挂载
pub async fn uploaded_files(req: Request) -> Response {
    let rel = req.uri().path().strip_prefix("/uploaded_files").unwrap_or("").to_string();
    let rel = crate::pyutil::unquote(&rel);
    http::static_file(&config::get().local_storage_path, &rel, req.headers()).await
}

/// /cache/* — CACHE_DIR 静态挂载
pub async fn cache_files(req: Request) -> Response {
    let rel = req.uri().path().strip_prefix("/cache").unwrap_or("").to_string();
    let rel = crate::pyutil::unquote(&rel);
    http::static_file(&config::get().cache_dir, &rel, req.headers()).await
}

/// 未匹配路由: 尾斜杠互换后可匹配则 307 重定向, 否则 404
pub async fn fallback(req: Request) -> Response {
    let path = req.uri().path().to_string();
    if path != "/" {
        let alt = if path.ends_with('/') { path.trim_end_matches('/').to_string() } else { format!("{}/", path) };
        if crate::routes::path_matches(&alt) {
            let mut location = alt;
            if let Some(q) = req.uri().query() {
                location = format!("{}?{}", location, q);
            }
            // 站内相对跳转: 不信任 Host 头, 也不把 HTTPS 降级为 http
            return Response::builder()
                .status(307)
                .header(header::LOCATION, location)
                .header(header::CONTENT_LENGTH, 0)
                .body(axum::body::Body::empty())
                .unwrap();
        }
    }
    ApiError::not_found().into_response()
}

/// 管理鉴权: Cookie admin_token 或 X-Admin-Token 头任一匹配即放行, 否则 404
pub async fn require_admin(req: Request, next: axum::middleware::Next) -> Response {
    let expected = config::get().admin_token.as_bytes();
    let cookie_token = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k.trim() == ADMIN_COOKIE_NAME).then(|| v.trim().trim_matches('"').to_string())
        })
        .last()
        .unwrap_or_default();
    let header_token = req
        .headers()
        .get("X-Admin-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // 两者分别做常量时间比较, 避免短路
    let cookie_ok = constant_time_eq(cookie_token.as_bytes(), expected);
    let header_ok = constant_time_eq(header_token.as_bytes(), expected);
    if !(cookie_ok || header_ok) {
        return ApiError::not_found().into_response();
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8 | ((a.len() ^ b.len()) >> 8) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0 && a.len() == b.len()
}
