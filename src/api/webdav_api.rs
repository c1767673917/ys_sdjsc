//! WebDAV 状态/管理接口 (等价 app/api/webdav.py), 挂载于 /api/admin/webdav

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config;
use crate::http::{body_bool, check, json_body, ok_json, ApiError, ApiResult, Query};
use crate::pyutil::{beijing_now_iso, py_round};
use crate::state::AppState;

const LOG: &str = "app.api.webdav";

/// GET /status
pub async fn status(State(st): State<Arc<AppState>>) -> ApiResult {
    let available = st.webdav_fm.check_webdav_health().await;
    let cache_stats = st.webdav_fm.get_cache_stats().await;
    let pending = st.webdav_fm.get_pending_sync_count().await;
    ok_json(json!({
        "success": true,
        "webdav_available": available,
        "last_check": beijing_now_iso(),
        "pending_sync_count": pending,
        "total_cached_files": cache_stats.get("total_files").cloned().unwrap_or(json!(0)),
        "cache_size_mb": cache_stats.get("total_size_mb").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "message": if available { "WebDAV服务正常" } else { "WebDAV服务不可用" },
    }))
}

/// POST /sync — 手动触发同步
pub async fn sync(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let m = json_body(&body)?;
    let mut errors = Vec::new();
    let force = body_bool(&m, "force", Some(false), &mut errors);
    check(errors)?;

    if !st.webdav_fm.check_webdav_health().await {
        return Err(ApiError::new(503, "WebDAV服务不可用，无法执行同步"));
    }
    let pending = st.webdav_fm.get_pending_sync_count().await;
    if pending == 0 && !force {
        return ok_json(json!({
            "success": true,
            "sync_started": false,
            "pending_files": 0,
            "message": "没有待同步文件",
        }));
    }
    let fm = st.webdav_fm.clone();
    tokio::spawn(async move {
        fm.sync_pending_files().await;
    });
    log::info!(target: LOG, "手动触发同步任务，待同步文件数: {}", pending);
    ok_json(json!({
        "success": true,
        "sync_started": true,
        "pending_files": pending,
        "message": format!("同步任务已启动，{}个文件待同步", pending),
    }))
}

/// GET /files?path=/
pub async fn files(State(st): State<Arc<AppState>>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = Query::parse(raw.as_deref());
    let path = q.get("path").unwrap_or_else(|| "/".into());
    if !st.webdav_fm.check_webdav_health().await {
        return Err(ApiError::new(503, "WebDAV服务不可用"));
    }
    match st.webdav_client.list_files(&path).await {
        Ok(files) => ok_json(json!({
            "success": true,
            "path": path,
            "files": files,
            "total_count": files.len(),
        })),
        Err(e) => {
            let msg = format!("列出WebDAV文件失败: {}", e);
            log::error!(target: LOG, "{}", msg);
            Err(ApiError::new(500, msg))
        }
    }
}

/// 用临时文件上传测试内容, 结束后删除本地临时文件
async fn upload_test_file(st: &AppState, remote: &str, content: &[u8]) -> bool {
    let Ok(tmp) = tempfile::NamedTempFile::new() else { return false };
    let path = tmp.path().to_string_lossy().to_string();
    if std::fs::write(&path, content).is_err() {
        return false;
    }
    let r = st.webdav_client.upload_file(&path, remote).await;
    drop(tmp);
    r.get("success") == Some(&Value::Bool(true))
}

/// GET /health — 详细健康检查
pub async fn health(State(st): State<Arc<AppState>>) -> ApiResult {
    let s = config::get();
    let mut connection = false;
    let mut auth = false;
    let mut write = false;
    let mut read = false;

    connection = connection || st.webdav_client.health_check().await;
    if connection {
        match st.webdav_client.list_files("/").await {
            Ok(_) => {
                auth = true;
                let test_content = b"health_check_test";
                let test_path = "/health_check_test.txt";
                if upload_test_file(&st, test_path, test_content).await {
                    write = true;
                    if let Ok(downloaded) = st.webdav_client.download_file(test_path).await {
                        if downloaded == test_content {
                            read = true;
                        }
                        st.webdav_client.delete_file(test_path).await;
                    }
                }
            }
            Err(e) => log::warn!(target: LOG, "WebDAV健康检查异常: {}", e),
        }
    }

    let cache_directory = std::path::Path::new(&s.cache_dir).exists();
    let temp_directory = std::path::Path::new(&s.temp_storage_dir).exists();
    let checks = [connection, auth, write, read, cache_directory, temp_directory];
    let healthy = checks.iter().filter(|b| **b).count();
    let total = checks.len();
    let overall = if healthy == total {
        "healthy"
    } else if healthy as f64 >= total as f64 * 0.7 {
        "degraded"
    } else {
        "unhealthy"
    };

    ok_json(json!({
        "success": true,
        "health": {
            "webdav_connection": connection,
            "webdav_auth": auth,
            "webdav_write": write,
            "webdav_read": read,
            "cache_directory": cache_directory,
            "temp_directory": temp_directory,
            "overall_status": overall,
            "check_time": beijing_now_iso(),
            "healthy_checks": healthy,
            "total_checks": total,
        }
    }))
}

/// GET /cache/stats
pub async fn cache_stats(State(st): State<Arc<AppState>>) -> ApiResult {
    let mut stats = st.webdav_fm.get_cache_stats().await;
    let cache_dir = stats
        .get("cache_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| config::get().cache_dir.clone());
    match statvfs(&cache_dir) {
        Some((total, free)) => {
            let used = total - free;
            let usage = stats.get("total_size").and_then(|v| v.as_f64()).unwrap_or(0.0);
            stats.insert("disk_total_space".into(), json!(total));
            stats.insert("disk_free_space".into(), json!(free));
            stats.insert("disk_used_space".into(), json!(used));
            stats.insert(
                "cache_usage_percent".into(),
                if total > 0 { json!(py_round(usage / total as f64 * 100.0, 2)) } else { json!(0) },
            );
        }
        None => {
            stats.insert("disk_total_space".into(), json!(0));
            stats.insert("disk_free_space".into(), json!(0));
            stats.insert("disk_used_space".into(), json!(0));
            stats.insert("cache_usage_percent".into(), json!(0));
        }
    }
    ok_json(json!({"success": true, "cache_stats": stats}))
}

/// os.statvfs: (f_frsize*f_blocks, f_frsize*f_bavail)
fn statvfs(path: &str) -> Option<(i64, i64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if r != 0 {
        return None;
    }
    let frsize = st.f_frsize as i64;
    Some((frsize * st.f_blocks as i64, frsize * st.f_bavail as i64))
}

/// POST /cache/cleanup
pub async fn cache_cleanup(State(st): State<Arc<AppState>>) -> ApiResult {
    let fm = st.webdav_fm.clone();
    tokio::spawn(async move {
        fm.cleanup_cache().await;
    });
    log::info!(target: LOG, "手动触发缓存清理任务");
    ok_json(json!({"success": true, "message": "缓存清理任务已启动"}))
}

/// GET /backup/status
pub async fn backup_status(State(st): State<Arc<AppState>>) -> ApiResult {
    let b = st.backup.get_backup_status().await;
    ok_json(json!({
        "success": true,
        "last_backup": b.get("last_backup").cloned().unwrap_or(Value::Null),
        "next_backup": b.get("next_backup").cloned().unwrap_or(Value::Null),
        "backup_count": b.get("backup_count").cloned().unwrap_or(json!(0)),
        "total_size_mb": b.get("total_size_mb").and_then(|v| v.as_f64()).unwrap_or(0.0),
        "webdav_backup_count": b.get("webdav_backup_count").cloned().unwrap_or(json!(0)),
    }))
}

/// POST /backup/trigger
pub async fn backup_trigger(State(st): State<Arc<AppState>>) -> ApiResult {
    if !config::get().backup_enabled {
        return Err(ApiError::new(403, "备份功能已禁用"));
    }
    let b = st.backup.clone();
    tokio::spawn(async move {
        b.manual_backup().await;
    });
    log::info!(target: LOG, "手动触发备份任务");
    ok_json(json!({"success": true, "backup_started": true, "message": "备份任务已启动"}))
}

/// GET /config
pub async fn get_config() -> ApiResult {
    let s = config::get();
    ok_json(json!({
        "success": true,
        "config": {
            "webdav_url": s.webdav_url,
            "username": s.webdav_username,
            "base_path": s.webdav_base_path,
            "timeout": s.webdav_timeout,
            "retry_count": s.webdav_retry_count,
            "retry_delay": s.webdav_retry_delay,
            "cache_dir": s.cache_dir,
            "cache_days": s.cache_days,
            "temp_storage_dir": s.temp_storage_dir,
            "backup_enabled": s.backup_enabled,
            "backup_retention_days": s.backup_retention_days,
            "health_check_interval": s.health_check_interval,
            "sync_retry_interval": s.sync_retry_interval,
        }
    }))
}

/// POST /test-connection
pub async fn test_connection(State(st): State<Arc<AppState>>) -> ApiResult {
    if !st.webdav_client.health_check().await {
        return ok_json(json!({
            "success": false,
            "test_results": {
                "connection": false,
                "authentication": false,
                "write_permission": false,
                "read_permission": false,
            },
            "message": "WebDAV连接失败",
        }));
    }
    let (mut authentication, mut write_permission, mut read_permission) = (false, false, false);
    let r: Result<(), String> = async {
        st.webdav_client.list_files("/").await?;
        authentication = true;
        let test_file = "/connection_test.txt";
        let content = b"test";
        if upload_test_file(&st, test_file, content).await {
            write_permission = true;
            let downloaded = st.webdav_client.download_file(test_file).await?;
            if downloaded == content {
                read_permission = true;
            }
            st.webdav_client.delete_file(test_file).await;
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        log::warn!(target: LOG, "WebDAV权限测试异常: {}", e);
    }
    ok_json(json!({
        "success": true,
        "test_results": {
            "connection": true,
            "authentication": authentication,
            "write_permission": write_permission,
            "read_permission": read_permission,
        },
        "message": "WebDAV连接测试完成",
    }))
}
