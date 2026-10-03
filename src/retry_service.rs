//! 用友云上传失败自动重试服务 (等价 app/core/yonyou_retry_service.py)
//!
//! 定时扫描近期(upload_time 在 lookback 窗口内) status='failed' 且 error_code='NETWORK_ERROR'
//! 的物流类记录, 从本地或 WebDAV 取回文件重新上传到用友云。
//! 网络 I/O 期间不持有数据库连接。

use chrono::Duration as ChronoDuration;
use once_cell::sync::Lazy;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::config;
use crate::db;
use crate::file_manager::FileManager;
use crate::pyutil::{beijing_now_iso, beijing_now_naive, isoformat};
use crate::upload_types::{business_type_for, DEFAULT_UPLOAD_TYPE, UPLOAD_TYPE_LOGISTICS, UPLOAD_TYPE_WAREHOUSE};
use crate::yonyou::YonYouClient;

const LOG: &str = "app.core.yonyou_retry_service";

/// 用友已上传成功但落库失败的记录: id -> (file_id, logistics, customer_name)。
/// 下一轮只补写数据库, 不再重新上传, 避免用友侧出现重复附件。
static UPLOADED_UNSAVED: Lazy<Mutex<HashMap<i64, (Value, Value, Value)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

struct Candidate {
    id: i64,
    business_id: Option<String>,
    doc_number: Option<String>,
    doc_type: Option<String>,
    file_name: Option<String>,
    local_file_path: Option<String>,
    webdav_path: Option<String>,
    retry_count: i64,
}

fn show(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "None".into())
}

async fn fetch_candidates(lookback_hours: i64, max_records: i64) -> Result<Vec<Candidate>, String> {
    let cutoff = isoformat(&(beijing_now_naive() - ChronoDuration::hours(lookback_hours)));
    db::run(move |conn| {
        let rows = db::query_rows(
            conn,
            r#"
            SELECT id, business_id, doc_number, doc_type, file_name,
                   local_file_path, webdav_path, retry_count
            FROM upload_history
            WHERE status = 'failed'
              AND error_code = 'NETWORK_ERROR'
              AND NULLIF(yonyou_file_id, '') IS NULL
              AND deleted_at IS NULL
              AND COALESCE(NULLIF(upload_type, ''), ?) = ?
              AND upload_time >= ?
            ORDER BY updated_at ASC, id ASC
            LIMIT ?
            "#,
            &[
                db::text(DEFAULT_UPLOAD_TYPE),
                db::text(UPLOAD_TYPE_LOGISTICS),
                db::text(cutoff),
                SqlValue::Integer(max_records),
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| Candidate {
                id: match r[0] {
                    SqlValue::Integer(i) => i,
                    _ => 0,
                },
                business_id: db::opt_text(&r[1]),
                doc_number: db::opt_text(&r[2]),
                doc_type: db::opt_text(&r[3]),
                file_name: db::opt_text(&r[4]),
                local_file_path: db::opt_text(&r[5]),
                webdav_path: db::opt_text(&r[6]),
                retry_count: match r[7] {
                    SqlValue::Integer(i) => i,
                    _ => 0,
                },
            })
            .collect())
    })
    .await
}

async fn mark_success(
    id: i64,
    yonyou_file_id: Value,
    logistics: Value,
    customer_name: Value,
    retry_count: i64,
) -> Result<usize, String> {
    let now = beijing_now_iso();
    db::run(move |conn| {
        conn.execute(
            r#"
            UPDATE upload_history
            SET status = 'success',
                yonyou_file_id = ?,
                logistics = ?,
                customer_name = ?,
                error_code = NULL,
                error_message = NULL,
                retry_count = ?,
                updated_at = ?
            WHERE id = ?
              AND status = 'failed'
              AND error_code = 'NETWORK_ERROR'
              AND NULLIF(yonyou_file_id, '') IS NULL
              AND deleted_at IS NULL
            "#,
            rusqlite::params![
                json_to_sql(&yonyou_file_id),
                json_to_sql(&logistics),
                json_to_sql(&customer_name),
                retry_count,
                now,
                id
            ],
        )
        .map_err(|e| e.to_string())
    })
    .await
}

/// mark_success 失败时短暂退避后重试 (共 3 次)
async fn mark_success_with_retry(
    id: i64,
    file_id: &Value,
    logistics: &Value,
    customer_name: &Value,
    retry_count: i64,
) -> Result<usize, String> {
    let mut last_err = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
        }
        match mark_success(id, file_id.clone(), logistics.clone(), customer_name.clone(), retry_count).await {
            Ok(n) => return Ok(n),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// 进程重启后恢复卡在 uploading 的记录, 返回影响行数:
/// - 已有用友文件号: 改为成功 (用友已收到)
/// - 物流类、未删除: 改为 NETWORK_ERROR, 交给定时重试
/// - 仓库类、未删除: 仓库上传不经过用友, 无法自动重试, 改为 WAREHOUSE_STORAGE_ERROR 提示重新上传
pub async fn recover_stale_uploading() -> Result<usize, String> {
    let now = beijing_now_iso();
    db::run(move |conn| {
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let mut n = tx
            .execute(
                "UPDATE upload_history SET status = 'success', updated_at = ?
                 WHERE status = 'uploading' AND NULLIF(yonyou_file_id, '') IS NOT NULL",
                rusqlite::params![now],
            )
            .map_err(|e| e.to_string())?;
        for (upload_type, code, msg) in [
            (UPLOAD_TYPE_WAREHOUSE, "WAREHOUSE_STORAGE_ERROR", "服务重启导致保存中断，请重新上传"),
            (UPLOAD_TYPE_LOGISTICS, "NETWORK_ERROR", "服务重启导致上传中断，等待自动重试"),
        ] {
            n += tx
                .execute(
                    "UPDATE upload_history SET status = 'failed', error_code = ?, error_message = ?, updated_at = ?
                     WHERE status = 'uploading' AND deleted_at IS NULL AND NULLIF(yonyou_file_id, '') IS NULL
                       AND COALESCE(NULLIF(upload_type, ''), ?) = ?",
                    rusqlite::params![code, msg, now, DEFAULT_UPLOAD_TYPE, upload_type],
                )
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(n)
    })
    .await
}

async fn mark_still_failed(
    id: i64,
    error_code: Option<String>,
    error_message: Option<String>,
    retry_count: i64,
) -> Result<usize, String> {
    let now = beijing_now_iso();
    db::run(move |conn| {
        conn.execute(
            r#"
            UPDATE upload_history
            SET error_code = ?,
                error_message = ?,
                retry_count = ?,
                updated_at = ?
            WHERE id = ? AND status = 'failed' AND deleted_at IS NULL
            "#,
            rusqlite::params![error_code, error_message, retry_count, now, id],
        )
        .map_err(|e| e.to_string())
    })
    .await
}

/// JSON 值 -> SQLite 绑定值 (等价 Python sqlite3 对 str/int/float/None/bool 的绑定)
pub fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(u) = n.as_u64() {
                SqlValue::Text(u.to_string())
            } else {
                SqlValue::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// 扫描并重试近期因网络瞬时故障失败的用友云上传; 返回 {scanned, succeeded, failed, skipped}
pub async fn retry_failed_yonyou_uploads(fm: &FileManager, yc: &YonYouClient) -> Map<String, Value> {
    let s = config::get();
    let candidates = fetch_candidates(s.yonyou_retry_lookback_hours, s.yonyou_retry_max_records)
        .await
        .unwrap_or_else(|e| {
            log::error!(target: LOG, "[用友重试] 查询候选记录失败: {}", e);
            vec![]
        });

    let (mut succeeded, mut failed, mut skipped) = (0i64, 0i64, 0i64);
    let scanned = candidates.len();
    let stats = |sc: usize, su: i64, fa: i64, sk: i64| {
        json!({"scanned": sc, "succeeded": su, "failed": fa, "skipped": sk})
            .as_object()
            .cloned()
            .unwrap()
    };

    if candidates.is_empty() {
        return stats(0, 0, 0, 0);
    }

    log::info!(target: LOG, "[用友重试] 发现 {} 条近期网络失败记录, 开始重试", scanned);

    for c in candidates {
        let r: Result<(), String> = async {
            let new_retry_count = c.retry_count + 1;
            let doc = show(&c.doc_number);

            // 0. 上一轮用友已成功但落库失败: 只补写数据库
            let unsaved = UPLOADED_UNSAVED.lock().unwrap().get(&c.id).cloned();
            if let Some((file_id, logistics, customer_name)) = unsaved {
                let n = mark_success(c.id, file_id.clone(), logistics, customer_name, new_retry_count).await?;
                UPLOADED_UNSAVED.lock().unwrap().remove(&c.id);
                if n == 0 {
                    skipped += 1;
                    log::info!(target: LOG, "[用友重试] 记录已变更, 跳过更新 id={} doc={}", c.id, doc);
                    return Ok(());
                }
                succeeded += 1;
                log::info!(
                    target: LOG,
                    "[用友重试] 补写成功状态 id={} doc={} file_id={}",
                    c.id,
                    doc,
                    crate::pyutil::py_str(&file_id)
                );
                return Ok(());
            }

            // 1. 取回文件内容(本地优先, 其次 WebDAV)
            let has_local = c.local_file_path.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false);
            let has_webdav = c.webdav_path.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
            if !has_local && !has_webdav {
                mark_still_failed(
                    c.id,
                    Some("FILE_NOT_FOUND".into()),
                    Some("本地文件与 webdav_path 均不可用".into()),
                    new_retry_count,
                )
                .await?;
                skipped += 1;
                log::warn!(target: LOG, "[用友重试] 文件来源缺失, 标记 FILE_NOT_FOUND id={} doc={}", c.id, doc);
                return Ok(());
            }

            let content: Result<Vec<u8>, String> = if has_local {
                tokio::fs::read(c.local_file_path.as_deref().unwrap()).await.map_err(|e| e.to_string())
            } else {
                fm.get_file(c.webdav_path.as_deref().unwrap()).await
            };
            let content = match content {
                Ok(b) => b,
                Err(e) => {
                    skipped += 1;
                    log::warn!(target: LOG, "[用友重试] 取文件失败, 跳过本轮 id={} doc={}: {}", c.id, doc, e);
                    return Ok(());
                }
            };

            // 2. 重新上传到用友云
            let business_type = business_type_for(c.doc_type.as_deref());
            let result = yc
                .upload_file(
                    &content,
                    c.file_name.as_deref().unwrap_or(""),
                    c.business_id.as_deref().unwrap_or(""),
                    Some(&business_type),
                )
                .await;

            if !result.success {
                failed += 1;
                mark_still_failed(c.id, result.error_code.clone(), result.error_message.clone(), new_retry_count)
                    .await?;
                log::warn!(
                    target: LOG,
                    "[用友重试] 仍失败 id={} doc={} code={} msg={}",
                    c.id,
                    doc,
                    show(&result.error_code),
                    show(&result.error_message)
                );
                return Ok(());
            }

            // 3. 上传成功, 校验响应中的 file id
            let file_id = match result.data.get("id") {
                Some(v) if crate::pyutil::py_truthy(v) => v.clone(),
                _ => {
                    failed += 1;
                    let data_repr = py_repr_upload_result(&result.data);
                    mark_still_failed(
                        c.id,
                        Some("INVALID_RESPONSE".into()),
                        Some(format!("上传成功但响应缺少 id: {}", data_repr)),
                        new_retry_count,
                    )
                    .await?;
                    log::warn!(target: LOG, "[用友重试] 响应缺少 file id id={} doc={}", c.id, doc);
                    return Ok(());
                }
            };

            // 4. 补全物流信息和客户名称(失败不影响主流程)
            let (mut logistics, mut customer_name) = (Value::Null, Value::Null);
            if let Some(bid) = c.business_id.as_deref().filter(|b| !b.is_empty()) {
                let detail = yc.get_delivery_detail(bid).await;
                if detail.success {
                    logistics = detail.logistics;
                    customer_name = detail.customer_name;
                }
            }

            let updated = match mark_success_with_retry(c.id, &file_id, &logistics, &customer_name, new_retry_count).await {
                Ok(n) => n,
                Err(e) => {
                    UPLOADED_UNSAVED
                        .lock()
                        .unwrap()
                        .insert(c.id, (file_id.clone(), logistics, customer_name));
                    log::error!(
                        target: LOG,
                        "[用友重试] 用友已上传成功但写库失败, 下一轮仅补写 id={} doc={} file_id={}: {}",
                        c.id,
                        doc,
                        crate::pyutil::py_str(&file_id),
                        e
                    );
                    return Err(e);
                }
            };
            if updated > 0 {
                succeeded += 1;
                log::info!(
                    target: LOG,
                    "[用友重试] 成功 id={} doc={} file_id={}",
                    c.id,
                    doc,
                    crate::pyutil::py_str(&file_id)
                );
            } else {
                skipped += 1;
                log::info!(target: LOG, "[用友重试] 记录已变更, 跳过更新 id={} doc={}", c.id, doc);
            }
            Ok(())
        }
        .await;

        if let Err(e) = r {
            failed += 1;
            log::error!(target: LOG, "[用友重试] 处理记录异常 id={} doc={}: {}", c.id, show(&c.doc_number), e);
        }
    }

    log::info!(
        target: LOG,
        "[用友重试] 完成: 扫描{} 成功{} 仍失败{} 跳过{}",
        scanned,
        succeeded,
        failed,
        skipped
    );
    stats(scanned, succeeded, failed, skipped)
}

/// 原实现在消息中输出整个 result 字典的 repr
fn py_repr_upload_result(data: &Value) -> String {
    format!("{{'success': True, 'data': {}}}", py_repr(data))
}

/// Python repr 的近似 (用于错误信息展示)
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(_) => crate::pyutil::py_str(v),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("'{}': {}", k, py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod e2e {
    /// 手动端到端测试: DJGL_E2E_DIR 指向含 .env 与 data/ 的运行目录
    /// 运行: DJGL_E2E_DIR=... cargo test retry_e2e -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn retry_e2e() {
        let dir = std::env::var("DJGL_E2E_DIR").expect("DJGL_E2E_DIR");
        std::env::set_current_dir(&dir).unwrap();
        crate::config::init().unwrap();
        let fm = crate::file_manager::FileManager::new();
        let yc = crate::yonyou::YonYouClient::new();
        let stats = super::retry_failed_yonyou_uploads(&fm, &yc).await;
        println!("STATS {}", serde_json::Value::Object(stats));
    }
}
