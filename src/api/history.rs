//! 上传历史接口 (等价 app/api/history.py)

use axum::extract::{Path as AxPath, RawQuery};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};

use crate::db;
use crate::http::{ok_json, ApiError, ApiResult, Query};
use crate::pyutil::py_strip;
use crate::upload_types::*;

/// GET /api/history/{business_id}
pub async fn get_upload_history(AxPath(business_id): AxPath<String>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = Query::parse(raw.as_deref());
    let upload_type_value = py_strip(&q.get("upload_type").unwrap_or_default()).to_string();
    if !upload_type_value.is_empty() && !VALID_UPLOAD_TYPES.contains(&upload_type_value.as_str()) {
        return Err(ApiError::new(
            400,
            format!("upload_type必须为以下值之一: {}", valid_upload_types_sorted()),
        ));
    }

    let bid = business_id.clone();
    let rows = db::run(move |c| -> rusqlite::Result<Vec<Vec<SqlValue>>> {
        let cols = db::table_columns(c, "upload_history")?;
        let has_upload_type = cols.iter().any(|x| x == "upload_type");
        let upload_type_select = if has_upload_type {
            "COALESCE(NULLIF(upload_type, ''), ?) AS upload_type"
        } else {
            "? AS upload_type"
        };
        let mut where_clauses = vec!["business_id = ?".to_string(), "deleted_at IS NULL".to_string()];
        let mut params: Vec<SqlValue> = vec![db::text(bid)];
        if upload_type_value == UPLOAD_TYPE_LOGISTICS && has_upload_type {
            where_clauses.push("COALESCE(NULLIF(upload_type, ''), ?) = ?".into());
            params.push(db::text(DEFAULT_UPLOAD_TYPE));
            params.push(db::text(UPLOAD_TYPE_LOGISTICS));
        } else if upload_type_value == UPLOAD_TYPE_WAREHOUSE && has_upload_type {
            where_clauses.push("upload_type = ?".into());
            params.push(db::text(UPLOAD_TYPE_WAREHOUSE));
        } else if upload_type_value == UPLOAD_TYPE_WAREHOUSE {
            where_clauses.push("1 = 0".into());
        }
        let mut all = vec![db::text(DEFAULT_UPLOAD_TYPE)];
        all.extend(params);
        db::query_rows(
            c,
            &format!(
                r#"
            SELECT id, file_name, file_size, file_extension, upload_time,
                   status, error_code, error_message, yonyou_file_id, retry_count,
                   {}
            FROM upload_history
            WHERE {}
            ORDER BY upload_time DESC
        "#,
                upload_type_select,
                where_clauses.join(" AND ")
            ),
            &all,
        )
    })
    .await
    .map_err(|e| ApiError::new(500, e.to_string()))?;

    let mut records: Vec<Value> = Vec::new();
    let (mut success, mut failed) = (0, 0);
    for r in &rows {
        records.push(json!({
            "id": db::to_json(&r[0]),
            "file_name": db::to_json(&r[1]),
            "file_size": db::to_json(&r[2]),
            "file_extension": db::to_json(&r[3]),
            "upload_time": db::to_json(&r[4]),
            "status": db::to_json(&r[5]),
            "error_code": db::to_json(&r[6]),
            "error_message": db::to_json(&r[7]),
            "yonyou_file_id": db::to_json(&r[8]),
            "retry_count": db::to_json(&r[9]),
            "upload_type": db::to_json(&r[10]),
        }));
        if db::opt_text(&r[5]).as_deref() == Some("success") {
            success += 1;
        } else {
            failed += 1;
        }
    }

    ok_json(json!({
        "business_id": business_id,
        "total_count": records.len(),
        "success_count": success,
        "failed_count": failed,
        "records": records,
    }))
}
