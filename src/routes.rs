//! 路由表 (等价 app/main.py 的 include_router / mount)

use axum::middleware;
use axum::routing::{any, delete, get, patch, post};
use axum::Router;
use std::sync::Arc;

use crate::api::{admin, docs, history, logistics, migration, pages, upload, webdav_api};
use crate::config;
use crate::state::AppState;

/// 用于尾斜杠重定向判断的路由模式; "*" 匹配单个非空路径段
fn patterns() -> Vec<String> {
    let token = &config::get().admin_token;
    let mut v: Vec<String> = [
        "/",
        "/api/health",
        "/api/upload",
        "/api/history/*",
        "/api/admin/records",
        "/api/admin/logistics-options",
        "/api/admin/export",
        "/api/admin/statistics",
        "/api/admin/records/*/check",
        "/api/admin/records/*/notes",
        "/api/admin/files/*/preview",
        "/api/admin/files/*/download",
        "/api/admin/migration/start",
        "/api/admin/migration/status/*",
        "/api/admin/migration/list",
        "/api/admin/migration/cleanup",
        "/api/admin/migration/stats",
        "/api/admin/webdav/status",
        "/api/admin/webdav/sync",
        "/api/admin/webdav/files",
        "/api/admin/webdav/health",
        "/api/admin/webdav/cache/stats",
        "/api/admin/webdav/cache/cleanup",
        "/api/admin/webdav/backup/status",
        "/api/admin/webdav/backup/trigger",
        "/api/admin/webdav/config",
        "/api/admin/webdav/test-connection",
        "/api/admin/logistics-links/",
        "/api/admin/logistics-links/*/regenerate",
        "/api/admin/logistics-links/sync",
        "/api/admin/logistics-links/sync-status",
        "/api/portal/*/deliveries",
        "/l/*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if config::get().debug {
        v.extend(["/openapi.json".to_string(), "/docs".to_string(), "/docs/oauth2-redirect".to_string()]);
    }
    v.push(format!("/{}", token));
    v.push(format!("/{}/logistics-links", token));
    v
}

/// 判断路径是否能匹配任一路由 (含静态挂载前缀)
pub fn path_matches(path: &str) -> bool {
    for mount in ["/static/", "/uploaded_files/", "/cache/"] {
        if path.starts_with(mount) {
            return true;
        }
    }
    let segs: Vec<&str> = path.split('/').collect();
    patterns().iter().any(|p| {
        let ps: Vec<&str> = p.split('/').collect();
        ps.len() == segs.len()
            && ps.iter().zip(segs.iter()).all(|(a, b)| if *a == "*" { !b.is_empty() } else { a == b })
    })
}

pub fn build(state: Arc<AppState>) -> Router {
    let token = config::get().admin_token.clone();

    let admin_routes: Router<Arc<AppState>> = Router::new()
        // app/api/admin.py
        .route("/api/admin/records", get(admin::get_records).delete(admin::delete_records))
        .route("/api/admin/logistics-options", get(admin::logistics_options))
        .route("/api/admin/export", get(admin::export_records))
        .route("/api/admin/statistics", get(admin::statistics))
        .route("/api/admin/records/{record_id}/check", patch(admin::update_check))
        .route("/api/admin/records/{record_id}/notes", patch(admin::update_notes))
        .route("/api/admin/files/{record_id}/preview", get(admin::preview_file))
        .route("/api/admin/files/{record_id}/download", get(admin::download_file))
        // app/api/migration.py
        .route("/api/admin/migration/start", post(migration::start))
        .route("/api/admin/migration/status/{migration_id}", get(migration::status))
        .route("/api/admin/migration/list", get(migration::list))
        .route("/api/admin/migration/cleanup", delete(migration::cleanup))
        .route("/api/admin/migration/stats", get(migration::stats))
        // app/api/webdav.py
        .route("/api/admin/webdav/status", get(webdav_api::status))
        .route("/api/admin/webdav/sync", post(webdav_api::sync))
        .route("/api/admin/webdav/files", get(webdav_api::files))
        .route("/api/admin/webdav/health", get(webdav_api::health))
        .route("/api/admin/webdav/cache/stats", get(webdav_api::cache_stats))
        .route("/api/admin/webdav/cache/cleanup", post(webdav_api::cache_cleanup))
        .route("/api/admin/webdav/backup/status", get(webdav_api::backup_status))
        .route("/api/admin/webdav/backup/trigger", post(webdav_api::backup_trigger))
        .route("/api/admin/webdav/config", get(webdav_api::get_config))
        .route("/api/admin/webdav/test-connection", post(webdav_api::test_connection))
        // app/api/logistics_links.py
        .route("/api/admin/logistics-links/", get(logistics::list_links))
        .route("/api/admin/logistics-links/{link_id}/regenerate", post(logistics::regenerate))
        .route("/api/admin/logistics-links/sync", post(logistics::trigger_sync))
        .route("/api/admin/logistics-links/sync-status", get(logistics::sync_status))
        // 存储目录静态挂载: 单据图片仅管理员可读 (管理页通过 Cookie 访问)
        .route("/uploaded_files/{*rest}", any(pages::uploaded_files))
        .route("/cache/{*rest}", any(pages::cache_files))
        .route_layer(middleware::from_fn(pages::require_admin));

    // 上传请求体上限: 单文件上限 x 单次文件数 + 1MB 表单开销, 超限 413
    let s = config::get();
    let upload_limit = (s.max_file_size.max(0) as u64)
        .saturating_mul(s.max_files_per_request.max(1) as u64)
        .saturating_add(1024 * 1024)
        .min(usize::MAX as u64) as usize;

    let inner = Router::new()
        .route("/static/{*rest}", any(pages::static_files))
        .route(
            "/api/upload",
            post(upload::upload_files).layer(axum::extract::DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/history/{business_id}", get(history::get_upload_history))
        .merge(admin_routes)
        .route("/api/portal/{token}/deliveries", get(logistics::portal_deliveries))
        .route("/", get(pages::upload_page))
        .route("/api/health", get(pages::health))
        .route(&format!("/{}", token), get(pages::admin_page))
        .route(&format!("/{}/logistics-links", token), get(pages::logistics_links_page))
        .route("/l/{token}", get(pages::logistics_portal_page))
        .fallback(pages::fallback)
        .layer(axum::extract::DefaultBodyLimit::disable());

    // 仅 DEBUG 模式开放接口文档, 避免泄露全部API路径
    let inner = if config::get().debug {
        inner
            .route("/openapi.json", get(docs::openapi))
            .route("/docs", get(docs::swagger_ui))
            .route("/docs/oauth2-redirect", get(docs::oauth2_redirect))
    } else {
        inner
    };
    let inner = inner.with_state(state);

    // 外层包装: axum 在路由层之后才写入 Allow 头, 405 改写需在整个路由器之外进行
    Router::new()
        .fallback_service(inner)
        .layer(middleware::from_fn(crate::http::method_not_allowed_json))
        .layer(middleware::from_fn(crate::http::cors))
        .layer(middleware::from_fn(crate::http::access_log))
}

#[cfg(test)]
mod tests {
    #[test]
    fn segment_matching() {
        let segs = |p: &str| p.split('/').map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(segs("/api/history/1").len(), 4);
    }
}
