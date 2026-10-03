//! 物流链接管理 (管理侧, app/api/logistics_links.py) 与物流待上传门户 (物流侧, app/api/logistics_portal.py)

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use serde_json::json;

use crate::delivery_sync;
use crate::http::{self, json_response, ok_json, ok_map, ApiError, ApiResult};

const LOG: &str = "app.api.logistics_links";

/// GET /api/admin/logistics-links/
pub async fn list_links() -> ApiResult {
    let r: Result<_, String> = async {
        let state = delivery_sync::get_sync_state().await?;
        let links = delivery_sync::list_links_with_pending().await?;
        Ok(json!({
            "last_sync_at": state.get("last_sync_at"),
            "sync_status": state.get("last_status"),
            "links": links,
        }))
    }
    .await;
    match r {
        Ok(v) => ok_json(v),
        Err(e) => {
            log::error!(target: LOG, "获取物流链接列表失败: {}", e);
            Err(ApiError::new(500, format!("获取物流链接列表失败: {}", e)))
        }
    }
}

/// POST /api/admin/logistics-links/{link_id}/regenerate
pub async fn regenerate(AxPath(link_id): AxPath<String>) -> ApiResult {
    let link_id = http::path_int("link_id", &link_id)?;
    let token = delivery_sync::regenerate_token(link_id)
        .await
        .map_err(|e| ApiError::new(500, e))?;
    match token {
        None => Err(ApiError::new(404, "物流链接不存在")),
        Some(t) => {
            log::info!(target: LOG, "物流链接token已重置: link_id={}", link_id);
            ok_json(json!({"success": true, "token": t, "link_path": format!("/l/{}", t)}))
        }
    }
}

/// POST /api/admin/logistics-links/sync — 后台执行, 返回 202
pub async fn trigger_sync() -> ApiResult {
    if delivery_sync::is_sync_running() {
        return Err(ApiError::new(409, "同步正在进行中"));
    }
    let remaining = delivery_sync::get_manual_cooldown_remaining();
    if remaining > 0 {
        return Err(ApiError::new(409, format!("操作过于频繁, 请{}秒后再试", remaining)));
    }
    tokio::spawn(async {
        delivery_sync::sync_delivery_snapshot("manual").await;
    });
    Ok(json_response(StatusCode::ACCEPTED, &json!({"started": true})))
}

/// GET /api/admin/logistics-links/sync-status
pub async fn sync_status() -> ApiResult {
    let m = delivery_sync::get_sync_state().await.map_err(|e| ApiError::new(500, e))?;
    ok_map(m)
}

/// GET /api/portal/{token}/deliveries
pub async fn portal_deliveries(AxPath(token): AxPath<String>) -> ApiResult {
    match delivery_sync::get_portal_data(&token).await.map_err(|e| ApiError::new(500, e))? {
        None => Err(ApiError::new(404, "链接无效或已失效")),
        Some(m) => ok_map(m),
    }
}
