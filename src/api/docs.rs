//! DEBUG 模式下的接口文档 (等价 FastAPI 的 docs_url="/docs"、openapi_url="/openapi.json")
//! OpenAPI 文档取自原应用生成的结果, 运行时替换标题与版本。

use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::config;
use crate::http::{json_response, ApiResult};

const OPENAPI: &str = include_str!("../../assets/docs/openapi.json");
const SWAGGER_HTML: &str = include_str!("../../assets/docs/swagger.html");
const OAUTH2_REDIRECT_HTML: &str = include_str!("../../assets/docs/oauth2-redirect.html");

fn html(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len())
        .body(body.into())
        .unwrap()
}

pub async fn openapi() -> ApiResult {
    let s = config::get();
    let mut v: Value = serde_json::from_str(OPENAPI).unwrap_or(Value::Null);
    if let Some(info) = v.get_mut("info").and_then(|i| i.as_object_mut()) {
        info.insert("title".into(), Value::String(s.app_name.clone()));
        info.insert("version".into(), Value::String(s.app_version.clone()));
    }
    Ok(json_response(StatusCode::OK, &v))
}

pub async fn swagger_ui() -> Response {
    html(SWAGGER_HTML.replace("{{TITLE}}", &config::get().app_name))
}

pub async fn oauth2_redirect() -> Response {
    html(OAUTH2_REDIRECT_HTML.to_string())
}
