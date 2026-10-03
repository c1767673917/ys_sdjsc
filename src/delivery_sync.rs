//! 发货单快照同步服务 (等价 app/core/delivery_sync_service.py)
//!
//! 定时从用友"销售发货列表"拉取过去 N 天的发货单表头(isSum=true), 本地过滤
//! "供应商非自提 且 运费大于阈值"后单事务全量替换 delivery_snapshot 表,
//! 并为新出现的物流公司自动生成专属 token。任一页最终失败则整轮放弃, 保留旧快照。

use chrono::Duration as ChronoDuration;
use once_cell::sync::Lazy;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;
use crate::db;
use crate::pyutil::{beijing_now_iso, beijing_now_naive, py_parse_float, py_str, py_strip, py_truthy, token_urlsafe, urlencode};
use crate::retry_service::json_to_sql;
use crate::upload_types::{DEFAULT_UPLOAD_TYPE, UPLOAD_TYPE_LOGISTICS};
use crate::yonyou::YonYouClient;

const LOG: &str = "app.core.delivery_sync_service";

/// 单飞互斥: 同一时刻只允许一轮同步(定时与手动共用)
static SYNC_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
/// 手动同步冷却(进程内)
static LAST_MANUAL_SYNC_TS: Lazy<Mutex<f64>> = Lazy::new(|| Mutex::new(0.0));

/// 每页失败重试的等待秒数(重试2次)
const PAGE_RETRY_DELAYS: [u64; 2] = [2, 5];

pub const META_LAST_AT: &str = "delivery_sync_last_at";
pub const META_STATUS: &str = "delivery_sync_status";
pub const META_ERROR: &str = "delivery_sync_error";
pub const META_RECORD_COUNT: &str = "delivery_sync_record_count";

fn time_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn is_sync_running() -> bool {
    SYNC_LOCK.try_lock().is_err()
}

/// 手动同步冷却剩余秒数, 0表示可触发
pub fn get_manual_cooldown_remaining() -> i64 {
    let elapsed = time_now() - *LAST_MANUAL_SYNC_TS.lock().unwrap();
    let remaining = config::get().delivery_sync_manual_cooldown_seconds as f64 - elapsed;
    remaining.max(0.0).trunc() as i64
}

/// 执行一轮快照同步; trigger: "scheduled" / "manual"
pub async fn sync_delivery_snapshot(trigger: &str) -> Map<String, Value> {
    let to_map = |v: Value| v.as_object().cloned().unwrap();

    let Ok(_guard) = SYNC_LOCK.try_lock() else {
        return to_map(json!({"success": false, "skipped": "running", "error": "同步正在进行中"}));
    };

    if trigger == "manual" {
        let remaining = get_manual_cooldown_remaining();
        if remaining > 0 {
            return to_map(json!({
                "success": false,
                "skipped": "cooldown",
                "error": format!("请{}秒后再试", remaining),
            }));
        }
        *LAST_MANUAL_SYNC_TS.lock().unwrap() = time_now();
    }

    let s = config::get();
    let started = Instant::now();
    let now = beijing_now_naive();
    let begin = (now - ChronoDuration::days(s.delivery_sync_lookback_days))
        .format("%Y-%m-%d 00:00:00")
        .to_string();
    let end = now.format("%Y-%m-%d 23:59:59").to_string();

    let r: Result<(usize, usize, usize), String> = async {
        let client = YonYouClient::new();
        let (raw_records, pages) = fetch_all_pages(&client, &begin, &end).await?;
        let kept = extract_and_filter(&raw_records)?;
        let kept_count = kept.len();
        replace_snapshot_and_tokens(kept).await?;
        Ok((pages, raw_records.len(), kept_count))
    }
    .await;

    let duration_ms = started.elapsed().as_millis() as i64;
    match r {
        Ok((pages, raw_count, kept_count)) => {
            log::info!(
                target: LOG,
                "发货单快照同步完成({}): 窗口{}~{}, {}页/{}行原始, 过滤后保留{}张, 耗时{}ms",
                trigger,
                begin,
                end,
                pages,
                raw_count,
                kept_count,
                duration_ms
            );
            to_map(json!({
                "success": true,
                "skipped": null,
                "pages": pages,
                "raw_count": raw_count,
                "kept_count": kept_count,
                "duration_ms": duration_ms,
                "error": null,
            }))
        }
        Err(e) => {
            log::error!(target: LOG, "发货单快照同步失败({}): {}, 保留旧快照", trigger, e);
            set_sync_meta("failed", &e).await;
            to_map(json!({
                "success": false,
                "skipped": null,
                "duration_ms": duration_ms,
                "error": e,
            }))
        }
    }
}

/// page_count 的数值形式 (等价 Python `x or 0` 后与 int 比较)
fn page_count_of(v: &Value) -> Result<i64, String> {
    if !py_truthy(v) {
        return Ok(0);
    }
    match v {
        Value::Number(n) => Ok(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0).ceil() as i64)),
        Value::Bool(b) => Ok(*b as i64),
        other => Err(format!(
            "'<' not supported between instances of 'int' and '{}'",
            match other {
                Value::String(_) => "str",
                Value::Array(_) => "list",
                _ => "dict",
            }
        )),
    }
}

/// 逐页拉取全部表头行, 返回 (records, pages_fetched)
async fn fetch_all_pages(client: &YonYouClient, begin: &str, end: &str) -> Result<(Vec<Value>, usize), String> {
    let s = config::get();
    let page_size = s.delivery_sync_page_size;
    let max_pages = s.delivery_sync_max_pages;

    let first = fetch_page_with_retry(client, 1, page_size, begin, end).await?;
    let mut all: Vec<Value> = first.records.clone();
    let first_count = page_count_of(&first.page_count)?;
    let page_count = first_count.min(max_pages);
    if first_count > max_pages {
        log::warn!(
            target: LOG,
            "发货列表页数{}超过上限{}, 仅拉取前{}页",
            py_str(&first.page_count),
            max_pages,
            max_pages
        );
    }

    let mut pages_fetched = 1usize;
    for page_index in 2..=page_count {
        tokio::time::sleep(Duration::from_secs_f64(s.delivery_sync_page_interval_seconds.max(0.0))).await;
        let page = fetch_page_with_retry(client, page_index, page_size, begin, end).await?;
        pages_fetched += 1;
        if page.records.is_empty() {
            // pageCount 可能因同步期间数据变动而漂移, 空页视为拉完
            break;
        }
        all.extend(page.records);
    }
    Ok((all, pages_fetched))
}

async fn fetch_page_with_retry(
    client: &YonYouClient,
    page_index: i64,
    page_size: i64,
    begin: &str,
    end: &str,
) -> Result<crate::yonyou::DeliveryList, String> {
    let mut last_error = String::new();
    for attempt in 0..=PAGE_RETRY_DELAYS.len() {
        let result = client.get_delivery_list(page_index, page_size, begin, end).await;
        if result.success {
            return Ok(result);
        }
        last_error = format!(
            "{}: {}",
            result.error_code.clone().unwrap_or_else(|| "None".into()),
            result.error_message.clone().unwrap_or_else(|| "None".into())
        );
        if attempt < PAGE_RETRY_DELAYS.len() {
            log::warn!(
                target: LOG,
                "发货列表第{}页拉取失败({}), {}秒后重试",
                page_index,
                last_error,
                PAGE_RETRY_DELAYS[attempt]
            );
            tokio::time::sleep(Duration::from_secs(PAGE_RETRY_DELAYS[attempt])).await;
        }
    }
    Err(format!("第{}页拉取失败: {}", page_index, last_error))
}

fn parse_freight(v: Option<&Value>) -> f64 {
    let s = py_str(v.unwrap_or(&Value::Null)).replace(',', "");
    py_parse_float(py_strip(&s)).unwrap_or(0.0)
}

fn parse_optional_number(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => py_parse_float(py_strip(&py_str(v).replace(',', ""))),
    }
}

pub struct SnapshotRow {
    delivery_id: String,
    delivery_code: Value,
    customer_name: Value,
    vouchdate: String,
    logistics_name: String,
    freight: f64,
    shipping_memo: Option<String>,
    total_price_qty: Option<f64>,
}

/// 本地过滤 + 字段提取 + 按 delivery_id 去重
fn extract_and_filter(raw: &[Value]) -> Result<Vec<SnapshotRow>, String> {
    let min_freight = config::get().delivery_sync_min_freight;
    let mut order: Vec<String> = Vec::new();
    let mut kept: HashMap<String, SnapshotRow> = HashMap::new();

    for rec in raw {
        let Some(obj) = rec.as_object() else {
            return Err(format!("'{}' object has no attribute 'get'", py_type_name(rec)));
        };
        let empty = Map::new();
        let define = match obj.get("deliveryVoucherDefineCharacter") {
            Some(v) if py_truthy(v) => match v.as_object() {
                Some(m) => m,
                None => return Err(format!("'{}' object has no attribute 'get'", py_type_name(v))),
            },
            _ => &empty,
        };

        let logistics_raw = define.get("RX003_name").filter(|v| py_truthy(v));
        let logistics = py_strip(&logistics_raw.map(py_str).unwrap_or_default()).to_string();
        if logistics.is_empty() || logistics.contains("自提") {
            continue;
        }

        let freight = parse_freight(define.get("RX004"));
        if freight <= min_freight {
            continue;
        }

        let Some(id_val) = obj.get("id").filter(|v| !v.is_null()) else {
            continue;
        };
        let delivery_id = py_str(id_val);

        let vouchdate_src = obj.get("vouchdate").filter(|v| py_truthy(v)).map(py_str).unwrap_or_default();
        let vouchdate: String = vouchdate_src.chars().take(10).collect();

        let memo_src = obj.get("shippingMemo").filter(|v| py_truthy(v)).map(py_str).unwrap_or_default();
        let memo = py_strip(&memo_src).to_string();

        let row = SnapshotRow {
            delivery_id: delivery_id.clone(),
            delivery_code: obj.get("code").cloned().unwrap_or(Value::Null),
            customer_name: obj.get("agentId_name").cloned().unwrap_or(Value::Null),
            vouchdate,
            logistics_name: logistics,
            freight,
            shipping_memo: if memo.is_empty() { None } else { Some(memo) },
            // isSum=true 表头行的 totalOutStockPriceQty 已是整单汇总的计价数量
            total_price_qty: parse_optional_number(obj.get("totalOutStockPriceQty")),
        };
        if !kept.contains_key(&delivery_id) {
            order.push(delivery_id.clone());
        }
        kept.insert(delivery_id, row);
    }
    Ok(order.into_iter().filter_map(|k| kept.remove(&k)).collect())
}

fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => if n.is_f64() { "float" } else { "int" },
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn write_meta(conn: &rusqlite::Connection, key: &str, value: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO app_meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )
}

/// 单事务全量替换快照, 并为新物流名生成token, 更新同步状态元数据
async fn replace_snapshot_and_tokens(rows: Vec<SnapshotRow>) -> Result<(), String> {
    let now = beijing_now_iso();
    let mut names: Vec<String> = rows.iter().map(|r| r.logistics_name.clone()).collect();
    names.sort();
    names.dedup();

    db::run(move |conn| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM delivery_snapshot", [])?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO delivery_snapshot
                    (delivery_id, delivery_code, customer_name, vouchdate,
                     logistics_name, freight, shipping_memo, total_price_qty, synced_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )?;
            for r in &rows {
                stmt.execute(rusqlite::params![
                    r.delivery_id,
                    json_to_sql(&r.delivery_code),
                    json_to_sql(&r.customer_name),
                    r.vouchdate,
                    r.logistics_name,
                    r.freight,
                    r.shipping_memo,
                    r.total_price_qty,
                    now
                ])?;
            }
        }
        for name in &names {
            tx.execute(
                r#"
                INSERT OR IGNORE INTO logistics_tokens
                    (logistics_name, token, enabled, created_at, updated_at)
                VALUES (?, ?, 1, ?, ?)
                "#,
                rusqlite::params![name, token_urlsafe(16), now, now],
            )?;
        }
        write_meta(&tx, META_LAST_AT, &now)?;
        write_meta(&tx, META_STATUS, "success")?;
        write_meta(&tx, META_ERROR, "")?;
        write_meta(&tx, META_RECORD_COUNT, &rows.len().to_string())?;
        tx.commit()
    })
    .await
    .map_err(|e| e.to_string())
}

async fn set_sync_meta(status: &str, error: &str) {
    let (status, error) = (status.to_string(), error.to_string());
    let r = db::run(move |conn| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        write_meta(&tx, META_STATUS, &status)?;
        write_meta(&tx, META_ERROR, &error)?;
        tx.commit()
    })
    .await;
    if let Err(e) = r {
        log::error!(target: LOG, "写入同步状态失败: {}", e);
    }
}

/// 同步状态: {running, last_sync_at, last_status, last_error, record_count}
pub fn get_sync_state_blocking(conn: &rusqlite::Connection) -> rusqlite::Result<Map<String, Value>> {
    let rows = db::query_rows(
        conn,
        "SELECT key, value FROM app_meta WHERE key IN (?, ?, ?, ?)",
        &[
            db::text(META_LAST_AT),
            db::text(META_STATUS),
            db::text(META_ERROR),
            db::text(META_RECORD_COUNT),
        ],
    )?;
    let mut meta: HashMap<String, Value> = HashMap::new();
    for r in rows {
        if let Some(k) = db::opt_text(&r[0]) {
            meta.insert(k, db::to_json(&r[1]));
        }
    }
    let get = |k: &str| meta.get(k).cloned().unwrap_or(Value::Null);
    let record_count = match meta.get(META_RECORD_COUNT) {
        Some(v) if py_truthy(v) => py_str(v).trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    let last_error = match meta.get(META_ERROR) {
        Some(v) if py_truthy(v) => v.clone(),
        _ => Value::Null,
    };
    let mut m = Map::new();
    m.insert("running".into(), Value::Bool(is_sync_running()));
    m.insert("last_sync_at".into(), get(META_LAST_AT));
    m.insert("last_status".into(), get(META_STATUS));
    m.insert("last_error".into(), last_error);
    m.insert("record_count".into(), json!(record_count));
    Ok(m)
}

pub async fn get_sync_state() -> Result<Map<String, Value>, String> {
    db::run(|conn| get_sync_state_blocking(conn)).await.map_err(|e| e.to_string())
}

/// 门户查询里"已上传排除"的统一条件 (仅物流类、未删除的上传记录参与排除)
fn not_uploaded_condition() -> String {
    format!(
        r#"
    NOT EXISTS (
        SELECT 1 FROM upload_history u
        WHERE u.business_id = s.delivery_id
          AND u.deleted_at IS NULL
          AND COALESCE(NULLIF(u.upload_type, ''), '{d}') = '{l}'
    )
    AND NOT EXISTS (
        SELECT 1 FROM upload_history u
        WHERE u.doc_number = s.delivery_code
          AND u.deleted_at IS NULL
          AND COALESCE(NULLIF(u.upload_type, ''), '{d}') = '{l}'
    )
"#,
        d = DEFAULT_UPLOAD_TYPE,
        l = UPLOAD_TYPE_LOGISTICS
    )
}

/// 管理侧: 全部物流链接及各自待上传单据数
pub async fn list_links_with_pending() -> Result<Vec<Value>, String> {
    let sql = format!(
        r#"
            SELECT t.id, t.logistics_name, t.token, t.enabled,
                   t.created_at, t.last_access_at,
                   (SELECT COUNT(*) FROM delivery_snapshot s
                    WHERE s.logistics_name = t.logistics_name
                      AND {}) AS pending_count
            FROM logistics_tokens t
            ORDER BY pending_count DESC, t.logistics_name ASC
            "#,
        not_uploaded_condition()
    );
    db::run(move |conn| {
        let rows = db::query_rows(conn, &sql, &[])?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let token = db::opt_text(&r[2]).unwrap_or_else(|| "None".into());
                json!({
                    "id": db::to_json(&r[0]),
                    "logistics_name": db::to_json(&r[1]),
                    "token": db::to_json(&r[2]),
                    "enabled": db::truthy(&r[3]),
                    "created_at": db::to_json(&r[4]),
                    "last_access_at": db::to_json(&r[5]),
                    "pending_count": db::to_json(&r[6]),
                    "link_path": format!("/l/{}", token),
                })
            })
            .collect())
    })
    .await
    .map_err(|e: rusqlite::Error| e.to_string())
}

/// 重置指定物流的token, 旧链接立即失效。返回新token, 不存在返回None
pub async fn regenerate_token(link_id: i64) -> Result<Option<String>, String> {
    let new_token = token_urlsafe(16);
    let now = beijing_now_iso();
    db::run(move |conn| {
        let n = conn.execute(
            "UPDATE logistics_tokens SET token = ?, updated_at = ? WHERE id = ?",
            rusqlite::params![new_token, now, link_id],
        )?;
        Ok(if n == 0 { None } else { Some(new_token) })
    })
    .await
    .map_err(|e: rusqlite::Error| e.to_string())
}

/// 按token查有效(enabled)的物流链接记录: (id, logistics_name)
pub fn get_token_row_blocking(conn: &rusqlite::Connection, token: &str) -> rusqlite::Result<Option<(SqlValue, SqlValue)>> {
    let rows = db::query_rows(
        conn,
        "SELECT id, logistics_name FROM logistics_tokens WHERE token = ? AND enabled = 1",
        &[db::text(token)],
    )?;
    Ok(rows.into_iter().next().map(|mut r| {
        let name = r.pop().unwrap();
        let id = r.pop().unwrap();
        (id, name)
    }))
}

pub async fn get_token_row(token: &str) -> Result<Option<(SqlValue, SqlValue)>, String> {
    let token = token.to_string();
    db::run(move |conn| get_token_row_blocking(conn, &token))
        .await
        .map_err(|e| e.to_string())
}

/// 物流侧: 该物流公司的待上传单据清单。token无效/禁用返回None
pub async fn get_portal_data(token: &str) -> Result<Option<Map<String, Value>>, String> {
    let Some((id, name)) = get_token_row(token).await? else {
        return Ok(None);
    };
    let now = beijing_now_iso();
    let sql = format!(
        r#"
            SELECT s.delivery_id, s.delivery_code, s.customer_name,
                   s.vouchdate, s.shipping_memo, s.total_price_qty
            FROM delivery_snapshot s
            WHERE s.logistics_name = ?
              AND {}
            ORDER BY s.vouchdate ASC, s.delivery_code ASC
            "#,
        not_uploaded_condition()
    );
    let name2 = name.clone();
    let deliveries: Vec<Value> = db::run(move |conn| -> rusqlite::Result<Vec<Value>> {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE logistics_tokens SET last_access_at = ? WHERE id = ?",
            rusqlite::params![now, id],
        )?;
        let rows = db::query_rows(&tx, &sql, &[name2])?;
        let out = rows
            .into_iter()
            .map(|r| {
                let business_id = db::opt_text(&r[0]).unwrap_or_else(|| "None".into());
                let doc_number = db::opt_text(&r[1]).unwrap_or_default();
                json!({
                    "delivery_id": db::to_json(&r[0]),
                    "delivery_code": db::to_json(&r[1]),
                    "customer_name": db::to_json(&r[2]),
                    "vouchdate": db::to_json(&r[3]),
                    "shipping_memo": db::to_json(&r[4]),
                    "total_price_qty": db::to_json(&r[5]),
                    "upload_url": format!(
                        "/?{}",
                        urlencode(&[
                            ("business_id", business_id.as_str()),
                            ("doc_number", doc_number.as_str()),
                            ("doc_type", "销售"),
                        ])
                    ),
                })
            })
            .collect();
        tx.commit()?;
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?;

    let state = get_sync_state().await?;
    let mut m = Map::new();
    m.insert("logistics_name".into(), db::to_json(&name));
    m.insert("last_sync_at".into(), state.get("last_sync_at").cloned().unwrap_or(Value::Null));
    m.insert("total".into(), json!(deliveries.len()));
    m.insert("deliveries".into(), Value::Array(deliveries));
    Ok(Some(m))
}
