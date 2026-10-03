//! 文件迁移接口 (等价 app/api/migration.py), 挂载于 /api/admin/migration
//! 迁移任务状态仅保存在进程内存中。

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::db;
use crate::http::{body_bool, check, json_body, ok_json, ApiError, ApiResult};
use crate::pyutil::{beijing_now_iso, beijing_now_naive, parse_iso_naive, py_round};
use crate::state::AppState;

const LOG: &str = "app.api.migration";

struct LocalFile {
    id: i64,
    filename: String,
    local_path: String,
    upload_time: Option<String>,
    file_size: Value,
}

/// 查询已成功上传但未同步到WebDAV、且本地文件存在的记录
async fn get_local_files_to_migrate() -> Vec<LocalFile> {
    let r = db::run(|c| {
        db::query_rows(
            c,
            r#"
            SELECT id, file_name, local_file_path, upload_time, file_size
            FROM upload_history
            WHERE status = 'success'
            AND local_file_path IS NOT NULL
            AND (webdav_path IS NULL OR webdav_path = '')
            ORDER BY upload_time ASC
        "#,
            &[],
        )
    })
    .await;
    match r {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| {
                let path = db::opt_text(&r[2])?;
                if !Path::new(&path).exists() {
                    return None;
                }
                Some(LocalFile {
                    id: if let SqlValue::Integer(i) = r[0] { i } else { 0 },
                    filename: db::opt_text(&r[1]).unwrap_or_default(),
                    local_path: path,
                    upload_time: db::opt_text(&r[3]),
                    file_size: db::to_json(&r[4]),
                })
            })
            .collect(),
        Err(e) => {
            log::error!(target: LOG, "获取本地文件列表失败: {}", e);
            vec![]
        }
    }
}

/// 生成WebDAV路径 (按上传日期), 解析失败使用当前日期
fn generate_webdav_path(filename: &str, upload_time: Option<&str>) -> String {
    let dt = upload_time
        .map(|t| t.replace('Z', "+00:00"))
        .and_then(|t| parse_iso_naive(&t))
        .unwrap_or_else(beijing_now_naive);
    format!("files/{}/{}", dt.format("%Y/%m/%d"), filename)
}

fn update_task(st: &AppState, id: &str, updates: Value) {
    let mut g = st.migrations.lock().unwrap();
    if let Some(Value::Object(task)) = g.get_mut(id) {
        if let Value::Object(u) = updates {
            for (k, v) in u {
                task.insert(k, v);
            }
        }
    }
}

async fn background_migration_task(st: Arc<AppState>, migration_id: String, dry_run: bool) {
    update_task(&st, &migration_id, json!({"status": "running", "start_time": beijing_now_iso()}));
    log::info!(
        target: LOG,
        "开始执行迁移任务: {} (演练模式: {})",
        migration_id,
        if dry_run { "True" } else { "False" }
    );

    let files = get_local_files_to_migrate().await;
    let total = files.len();
    if total == 0 {
        update_task(&st, &migration_id, json!({"status": "completed", "message": "没有需要迁移的文件"}));
        return;
    }
    update_task(
        &st,
        &migration_id,
        json!({"progress": {"total": total, "completed": 0, "failed": 0, "percentage": 0.0}}),
    );

    let (mut completed, mut failed) = (0usize, 0usize);
    let mut errors: Vec<Value> = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let webdav_path = generate_webdav_path(&f.filename, f.upload_time.as_deref());
        if !dry_run {
            let r = st.migration_client.upload_file(&f.local_path, &webdav_path).await;
            if r.get("success") == Some(&Value::Bool(true)) {
                let (id, wp, now) = (f.id, webdav_path.clone(), beijing_now_iso());
                let upd = db::run(move |c| {
                    c.execute(
                        "UPDATE upload_history SET webdav_path = ?, is_cached = 1, updated_at = ? WHERE id = ?",
                        rusqlite::params![wp, now, id],
                    )
                })
                .await;
                match upd {
                    Ok(_) => {
                        completed += 1;
                        log::info!(target: LOG, "文件迁移成功: {} -> {}", f.filename, webdav_path);
                    }
                    Err(e) => {
                        failed += 1;
                        errors.push(json!({"filename": f.filename, "error": e.to_string()}));
                        log::error!(target: LOG, "迁移文件异常: {} - {}", f.filename, e);
                    }
                }
            } else {
                failed += 1;
                let err = r.get("error").cloned().unwrap_or(json!("上传失败"));
                log::error!(
                    target: LOG,
                    "文件迁移失败: {} - {}",
                    f.filename,
                    err.as_str().unwrap_or("")
                );
                errors.push(json!({"filename": f.filename, "error": err}));
            }
        } else {
            completed += 1;
            log::info!(target: LOG, "[演练] 文件迁移: {} -> {}", f.filename, webdav_path);
        }

        let percentage = (i + 1) as f64 / total as f64 * 100.0;
        update_task(
            &st,
            &migration_id,
            json!({"progress": {
                "total": total,
                "completed": completed,
                "failed": failed,
                "percentage": py_round(percentage, 1),
            }}),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let final_status = if failed == 0 { "completed" } else { "completed_with_errors" };
    let final_message = format!("迁移完成: 成功{}个，失败{}个", completed, failed);
    update_task(
        &st,
        &migration_id,
        json!({
            "status": final_status,
            "end_time": beijing_now_iso(),
            "message": final_message,
            "errors": errors,
        }),
    );
    log::info!(target: LOG, "迁移任务完成: {} - {}", migration_id, final_message);
}

/// POST /start
pub async fn start(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let m = json_body(&body)?;
    let mut errors = Vec::new();
    let dry_run = body_bool(&m, "dry_run", Some(false), &mut errors);
    check(errors)?;

    let running: Option<String> = {
        let g = st.migrations.lock().unwrap();
        g.iter()
            .find(|(_, t)| matches!(t.get("status").and_then(|s| s.as_str()), Some("running") | Some("pending")))
            .map(|(k, _)| k.clone())
    };
    if let Some(id) = running {
        // 原实现在 try 内抛出 409 后被通用 except 包装为 500
        let msg = format!("启动迁移任务失败: 409: 已有运行中的迁移任务: {}", id);
        log::error!(target: LOG, "{}", msg);
        return Err(ApiError::new(500, msg));
    }

    let migration_id = uuid::Uuid::new_v4().to_string();
    let files = get_local_files_to_migrate().await;
    let n = files.len();
    st.migrations.lock().unwrap().insert(
        migration_id.clone(),
        json!({
            "migration_id": migration_id,
            "status": "pending",
            "total_files": n,
            "progress": {"total": n, "completed": 0, "failed": 0, "percentage": 0.0},
            "created_at": beijing_now_iso(),
            "dry_run": dry_run,
        }),
    );

    let st2 = st.clone();
    let mid = migration_id.clone();
    tokio::spawn(async move { background_migration_task(st2, mid, dry_run).await });
    log::info!(target: LOG, "迁移任务已启动: {}", migration_id);

    ok_json(json!({
        "success": true,
        "migration_id": migration_id,
        "total_files": n,
        "dry_run": dry_run,
        "message": format!("迁移任务已启动 ({})", if dry_run { "演练模式" } else { "正式模式" }),
    }))
}

/// GET /status/{migration_id}
pub async fn status(State(st): State<Arc<AppState>>, AxPath(migration_id): AxPath<String>) -> ApiResult {
    let task = st.migrations.lock().unwrap().get(&migration_id).cloned();
    let Some(task) = task else {
        return Err(ApiError::new(404, "迁移任务不存在"));
    };
    let progress = match task.get("progress") {
        Some(Value::Object(p)) => json!({
            "total": p.get("total").cloned().unwrap_or(json!(0)),
            "completed": p.get("completed").cloned().unwrap_or(json!(0)),
            "failed": p.get("failed").cloned().unwrap_or(json!(0)),
            "percentage": p.get("percentage").and_then(|v| v.as_f64()).unwrap_or(0.0),
        }),
        _ => Value::Null,
    };
    ok_json(json!({
        "success": true,
        "migration_id": migration_id,
        "status": task.get("status").cloned().unwrap_or(json!("unknown")),
        "progress": progress,
        "errors": task.get("errors").cloned().unwrap_or(json!([])),
        "message": task.get("message").cloned().unwrap_or(Value::Null),
    }))
}

/// GET /list
pub async fn list(State(st): State<Arc<AppState>>) -> ApiResult {
    let g = st.migrations.lock().unwrap();
    let mut tasks: Vec<Value> = g
        .iter()
        .map(|(id, t)| {
            json!({
                "migration_id": id,
                "status": t.get("status").cloned().unwrap_or(Value::Null),
                "total_files": t.get("total_files").cloned().unwrap_or(json!(0)),
                "created_at": t.get("created_at").cloned().unwrap_or(Value::Null),
                "start_time": t.get("start_time").cloned().unwrap_or(Value::Null),
                "end_time": t.get("end_time").cloned().unwrap_or(Value::Null),
                "dry_run": t.get("dry_run").cloned().unwrap_or(json!(false)),
                "progress": t.get("progress").cloned().unwrap_or(json!({})),
            })
        })
        .collect();
    // 按创建时间倒序 (稳定排序)
    tasks.sort_by(|a, b| {
        let ka = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    let total = tasks.len();
    ok_json(json!({"success": true, "tasks": tasks, "total": total}))
}

/// DELETE /cleanup — 保留最近10个, 仅删除已结束的任务
pub async fn cleanup(State(st): State<Arc<AppState>>) -> ApiResult {
    let mut g = st.migrations.lock().unwrap();
    let mut sorted: Vec<(String, String, String)> = g
        .iter()
        .map(|(k, t)| {
            (
                k.clone(),
                t.get("created_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                t.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            )
        })
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    let mut deleted = 0;
    for (id, _, status) in sorted.into_iter().skip(10) {
        if matches!(status.as_str(), "completed" | "completed_with_errors" | "failed") {
            g.shift_remove(&id);
            deleted += 1;
        }
    }
    ok_json(json!({
        "success": true,
        "deleted_count": deleted,
        "remaining_count": g.len(),
        "message": format!("已清理{}个历史迁移任务", deleted),
    }))
}

/// GET /stats
pub async fn stats() -> ApiResult {
    let local = get_local_files_to_migrate().await;
    let local_count = local.len() as i64;
    let mut local_size_i: i64 = 0;
    let mut local_size_f: f64 = 0.0;
    let mut is_float = false;
    for f in &local {
        match &f.file_size {
            Value::Number(n) if n.is_i64() => local_size_i += n.as_i64().unwrap(),
            Value::Number(n) => {
                is_float = true;
                local_size_f += n.as_f64().unwrap_or(0.0);
            }
            _ => {
                return Err(ApiError::new(
                    500,
                    "获取迁移统计失败: unsupported operand type(s) for +: 'int' and 'NoneType'",
                ))
            }
        }
    }
    let local_size_total = local_size_i as f64 + local_size_f;
    let local_size_json = if is_float { json!(local_size_total) } else { json!(local_size_i) };

    let migrated = db::run(|c| {
        db::query_rows(
            c,
            r#"
                SELECT COUNT(*), SUM(file_size)
                FROM upload_history
                WHERE status = 'success' AND webdav_path IS NOT NULL AND webdav_path != ''
            "#,
            &[],
        )
    })
    .await
    .map_err(|e| ApiError::new(500, format!("获取迁移统计失败: {}", e)))?;
    let row = migrated.into_iter().next().unwrap_or_else(|| vec![SqlValue::Integer(0), SqlValue::Null]);
    let migrated_count = if db::truthy(&row[0]) { db::to_json(&row[0]) } else { json!(0) };
    let migrated_size = if db::truthy(&row[1]) { db::to_json(&row[1]) } else { json!(0) };
    let mc = migrated_count.as_f64().unwrap_or(0.0);
    let ms = migrated_size.as_f64().unwrap_or(0.0);
    let total_files = local_count as f64 + mc;

    let mut out = Map::new();
    out.insert("success".into(), json!(true));
    out.insert(
        "local_files".into(),
        json!({
            "count": local_count,
            "size": local_size_json,
            "size_mb": py_round(local_size_total / 1024.0 / 1024.0, 2),
        }),
    );
    out.insert(
        "migrated_files".into(),
        json!({
            "count": migrated_count,
            "size": migrated_size,
            "size_mb": py_round(ms / 1024.0 / 1024.0, 2),
        }),
    );
    out.insert("total_files".into(), json!(local_count + mc as i64));
    out.insert(
        "migration_progress".into(),
        if total_files > 0.0 { json!(py_round(mc / total_files * 100.0, 2)) } else { json!(0) },
    );
    ok_json(Value::Object(out))
}
