//! 上传接口 (等价 app/api/upload.py)

use axum::extract::{Multipart, State};
use axum::response::Response;
use chrono::Duration as ChronoDuration;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use crate::config;
use crate::db;
use crate::file_manager::SaveResult;
use crate::http::{missing, ok_json, ApiError};
use crate::pyutil::{beijing_now_iso, beijing_now_naive, isoformat, path_join, py_float_repr, py_isdigit, py_strip};
use crate::retry_service::json_to_sql;
use crate::state::AppState;
use crate::upload_types::*;

/// 生成唯一文件名: {doc_number}_{时间戳}_{8位uuid}{扩展名}, 返回 (文件名, 完整路径)
pub fn generate_unique_filename(doc_number: &str, file_extension: &str, storage_path: &str) -> (String, String) {
    let timestamp = beijing_now_naive().format("%Y%m%d%H%M%S");
    let short_uuid: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
    let new_filename = format!("{}_{}_{}{}", doc_number, timestamp, short_uuid, file_extension);
    let full_path = path_join(storage_path, &new_filename);
    (new_filename, full_path)
}

/// doc_number 是否可安全用作文件名前缀
pub fn is_safe_doc_number(doc_number: &str) -> bool {
    !doc_number.contains("..")
        && !doc_number
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '\'' | '<' | '>' | '|' | '&'))
}

/// 保存文件到本地 (自动创建目录)
pub fn save_file_locally(content: &[u8], file_path: &str) -> std::io::Result<()> {
    let dir = crate::pyutil::dirname(file_path);
    if !dir.is_empty() {
        std::fs::create_dir_all(&dir)?;
    }
    std::fs::write(file_path, content)
}

/// 校验并规范化上传业务类型 (空值默认物流)
pub fn normalize_upload_type(upload_type: Option<&str>) -> Result<String, ApiError> {
    let v = py_strip(upload_type.unwrap_or(""));
    let v = if v.is_empty() { DEFAULT_UPLOAD_TYPE } else { v };
    if !VALID_UPLOAD_TYPES.contains(&v) {
        return Err(ApiError::new(
            400,
            format!("upload_type必须为以下值之一: {}", valid_upload_types_sorted()),
        ));
    }
    Ok(v.to_string())
}

/// file.filename.split(".")[-1].lower() 前加 "."
fn file_ext(filename: &str) -> String {
    format!(".{}", filename.rsplit('.').next().unwrap_or("").to_lowercase())
}

async fn set_uploading(id: i64) -> Result<(), String> {
    let now = beijing_now_iso();
    db::run(move |c| {
        c.execute(
            "UPDATE upload_history SET status = 'uploading', updated_at = ? WHERE id = ?",
            rusqlite::params![now, id],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .await
}

fn insert_file_metadata(c: &rusqlite::Connection, filename: &str, r: &SaveResult, now: &str) -> rusqlite::Result<usize> {
    c.execute(
        r#"
        INSERT INTO file_metadata
        (filename, webdav_path, local_cache_path, upload_time, file_size,
         is_cached, last_access_time, webdav_etag, is_synced, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        rusqlite::params![
            filename,
            r.webdav_path,
            r.local_cache_path,
            r.upload_time,
            r.file_size.map(|s| s as i64),
            r.is_cached_or_false(),
            now,
            r.webdav_etag,
            r.is_synced_or_false(),
            now,
            now
        ],
    )
}

/// 后台任务：上传文件到WebDAV + 用友云并更新数据库状态
pub async fn background_upload_to_yonyou(
    state: Arc<AppState>,
    file_content: Vec<u8>,
    new_filename: String,
    business_id: String,
    business_type: String,
    local_file_path: String,
    record_id: i64,
) {
    if let Err(e) = set_uploading(record_id).await {
        println!("更新uploading状态失败: {}", e);
    }

    let s = config::get();
    let r: Result<(), String> = async {
        // 1. 保存到WebDAV + 本地缓存
        let webdav_result = state.upload_fm.save_file(&file_content, &new_filename).await;
        if webdav_result.success {
            println!("WebDAV保存成功: {}", webdav_result.webdav_path.clone().unwrap_or_default());
        } else {
            println!("WebDAV保存失败: {}", webdav_result.error.clone().unwrap_or_else(|| "未知错误".into()));
        }

        // 2. 保存到本地作为备份（如果WebDAV失败）
        if !webdav_result.success && !local_file_path.is_empty() {
            match save_file_locally(&file_content, &local_file_path) {
                Ok(_) => println!("本地备份保存成功: {}", local_file_path),
                Err(e) => println!("本地备份保存失败: {}", e),
            }
        }

        // 3. 上传到用友云 (仅网络级错误重试; Token 错误由客户端内部处理)
        let mut yonyou_file_id: Option<Value> = None;
        let mut error_code: Option<String> = None;
        let mut error_message: Option<String> = None;
        let mut retry_count: i64 = 0;
        for attempt in 0..s.max_retry_count.max(0) {
            let result = state
                .upload_yc
                .upload_file(&file_content, &new_filename, &business_id, Some(&business_type))
                .await;
            if result.success {
                match result.data.get("id") {
                    Some(id) => yonyou_file_id = Some(id.clone()),
                    None => return Err("'id'".to_string()),
                }
                retry_count = attempt;
                break;
            }
            error_code = result.error_code.clone();
            error_message = result.error_message.clone();
            retry_count = attempt + 1;
            if error_code.as_deref() != Some("NETWORK_ERROR") {
                break;
            }
            if attempt < s.max_retry_count - 1 {
                tokio::time::sleep(Duration::from_secs(s.retry_delay.max(0) as u64)).await;
            }
        }

        // 物流信息 + 客户名称查询 (上传成功后)
        let mut logistics = Value::Null;
        let mut customer_name = Value::Null;
        let id_truthy = yonyou_file_id.as_ref().map(crate::pyutil::py_truthy).unwrap_or(false);
        if id_truthy && !business_id.is_empty() {
            let d = state.upload_yc.get_delivery_detail(&business_id).await;
            if d.success {
                logistics = d.logistics;
                customer_name = d.customer_name;
                let show = |v: &Value| {
                    if crate::pyutil::py_truthy(v) { crate::pyutil::py_str(v) } else { "(空)".to_string() }
                };
                println!("发货单详情获取成功: 物流={} 客户={}", show(&logistics), show(&customer_name));
            } else {
                println!("发货单详情获取失败: {}", d.error_message.unwrap_or_else(|| "未知错误".into()));
            }
        }

        // 4. 更新最终状态
        let now = beijing_now_naive();
        let cache_expiry_time = if webdav_result.success && webdav_result.is_cached_or_false() {
            Some(isoformat(&(now + ChronoDuration::days(s.cache_days))))
        } else {
            None
        };
        // WebDAV 降级到临时存储时，文件只落在 temp 目录，需写入 local_file_path
        let local_path_value = if webdav_result.storage_type.as_deref() == Some("temp") {
            webdav_result.local_cache_path.clone()
        } else {
            Some(local_file_path.clone())
        };
        let now_iso = beijing_now_iso();
        let wr = webdav_result.clone();
        let fname = new_filename.clone();

        db::run(move |c| -> Result<(), String> {
            if id_truthy {
                c.execute(
                    r#"
                    UPDATE upload_history
                    SET status = 'success', yonyou_file_id = ?, logistics = ?, customer_name = ?,
                        webdav_path = ?, is_cached = ?, cache_expiry_time = ?, local_file_path = ?,
                        retry_count = ?, updated_at = ?
                    WHERE id = ?
                    "#,
                    rusqlite::params![
                        json_to_sql(yonyou_file_id.as_ref().unwrap()),
                        json_to_sql(&logistics),
                        json_to_sql(&customer_name),
                        wr.webdav_path,
                        wr.is_cached_or_false(),
                        cache_expiry_time,
                        local_path_value,
                        retry_count,
                        now_iso,
                        record_id
                    ],
                )
                .map_err(|e| e.to_string())?;
            } else {
                c.execute(
                    r#"
                    UPDATE upload_history
                    SET status = 'failed', error_code = ?, error_message = ?, logistics = ?, customer_name = ?,
                        webdav_path = ?, is_cached = ?, cache_expiry_time = ?, local_file_path = ?,
                        retry_count = ?, updated_at = ?
                    WHERE id = ?
                    "#,
                    rusqlite::params![
                        error_code,
                        error_message,
                        SqlValue::Null,
                        SqlValue::Null,
                        wr.webdav_path,
                        wr.is_cached_or_false(),
                        cache_expiry_time,
                        local_path_value,
                        retry_count,
                        now_iso,
                        record_id
                    ],
                )
                .map_err(|e| e.to_string())?;
            }

            // 5. 如果WebDAV保存成功，插入文件元数据记录
            if wr.success {
                if let Err(e) = insert_file_metadata(c, &fname, &wr, &beijing_now_iso()) {
                    println!("插入文件元数据失败: {}", e);
                }
            }
            Ok(())
        })
        .await
    }
    .await;

    if let Err(e) = r {
        println!("后台上传任务异常: {}", e);
        let now = beijing_now_iso();
        let res = db::run(move |c| {
            c.execute(
                r#"
                UPDATE upload_history
                SET status = 'failed', error_code = 'BACKGROUND_TASK_ERROR', error_message = ?, updated_at = ?
                WHERE id = ?
                "#,
                rusqlite::params![e, now, record_id],
            )
        })
        .await;
        if let Err(inner) = res {
            println!("更新失败状态时出错: {}", inner);
        }
    }
}

/// 后台任务：仓库上传仅保存到应用存储，不调用用友云
pub async fn background_save_warehouse_upload(
    state: Arc<AppState>,
    file_content: Vec<u8>,
    new_filename: String,
    local_file_path: String,
    record_id: i64,
) {
    if let Err(e) = set_uploading(record_id).await {
        println!("更新仓库uploading状态失败: {}", e);
    }

    let s = config::get();
    let mut error_detail: Option<String> = None;

    let webdav_result = state.upload_fm.save_file(&file_content, &new_filename).await;
    let mut storage_success = webdav_result.success;
    if storage_success {
        println!("仓库文件保存成功: {}", webdav_result.webdav_path.clone().unwrap_or_else(|| "None".into()));
    } else {
        let d = webdav_result.error.clone().unwrap_or_else(|| "应用存储保存失败".into());
        println!("仓库文件保存失败: {}", d);
        error_detail = Some(d);
    }

    if !storage_success && !local_file_path.is_empty() {
        match save_file_locally(&file_content, &local_file_path) {
            Ok(_) => {
                storage_success = true;
                println!("仓库文件本地保存成功: {}", local_file_path);
            }
            Err(e) => {
                error_detail = Some(e.to_string());
                println!("仓库文件本地保存失败: {}", e);
            }
        }
    }

    let now_iso = beijing_now_iso();
    let cache_days = s.cache_days;
    let wr = webdav_result.clone();
    let fname = new_filename.clone();
    let r = db::run(move |c| -> Result<(), String> {
        if storage_success {
            let cache_expiry_time = if wr.success && wr.is_cached_or_false() {
                Some(isoformat(&(beijing_now_naive() + ChronoDuration::days(cache_days))))
            } else {
                None
            };
            let local_path_value = if wr.storage_type.as_deref() == Some("temp") {
                wr.local_cache_path.clone()
            } else {
                Some(local_file_path.clone())
            };
            let (wp, cached) = if wr.success { (wr.webdav_path.clone(), wr.is_cached_or_false()) } else { (None, false) };
            c.execute(
                r#"
                UPDATE upload_history
                SET status = 'success', yonyou_file_id = NULL, logistics = NULL, customer_name = NULL,
                    error_code = NULL, error_message = NULL, webdav_path = ?, is_cached = ?,
                    cache_expiry_time = ?, local_file_path = ?, retry_count = 0, updated_at = ?
                WHERE id = ?
                "#,
                rusqlite::params![wp, cached, cache_expiry_time, local_path_value, now_iso, record_id],
            )
            .map_err(|e| e.to_string())?;

            if wr.success {
                if let Err(e) = insert_file_metadata(c, &fname, &wr, &now_iso) {
                    println!("插入仓库文件元数据失败: {}", e);
                }
            }
        } else {
            c.execute(
                r#"
                UPDATE upload_history
                SET status = 'failed', error_code = 'WAREHOUSE_STORAGE_ERROR', error_message = ?,
                    yonyou_file_id = NULL, logistics = NULL, customer_name = NULL, updated_at = ?
                WHERE id = ?
                "#,
                rusqlite::params![error_detail.unwrap_or_else(|| "仓库文件保存失败".into()), now_iso, record_id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await;

    if let Err(e) = r {
        println!("仓库后台保存任务异常: {}", e);
        let now = beijing_now_iso();
        let res = db::run(move |c| {
            c.execute(
                r#"
                UPDATE upload_history
                SET status = 'failed', error_code = 'WAREHOUSE_STORAGE_ERROR', error_message = ?,
                    yonyou_file_id = NULL, logistics = NULL, customer_name = NULL, updated_at = ?
                WHERE id = ?
                "#,
                rusqlite::params![e, now, record_id],
            )
        })
        .await;
        if let Err(inner) = res {
            println!("更新仓库失败状态时出错: {}", inner);
        }
    }
}

struct UploadedFile {
    filename: String,
    content: Vec<u8>,
}

enum Job {
    Logistics { content: Vec<u8>, filename: String, local: String, id: i64 },
    Warehouse { content: Vec<u8>, filename: String, local: String, id: i64 },
}

/// POST /api/upload — 批量上传文件（记录入库后立即返回, 后台异步处理）
/// multipart 解析错误: 超过请求体上限返回 413, 其余 400
fn multipart_error(e: axum::extract::multipart::MultipartError) -> ApiError {
    if e.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::new(413, "请求体过大");
    }
    ApiError::new(400, format!("There was an error parsing the body: {}", e))
}

pub async fn upload_files(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> Result<Response, ApiError> {
    let s = config::get();

    // 解析表单 (同名字段取最后一个; 空字符串视为未提供)
    let mut fields: std::collections::HashMap<String, String> = Default::default();
    let mut files: Vec<UploadedFile> = Vec::new();
    let mut files_non_file: Vec<String> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(multipart_error(e)),
        };
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let data = field.bytes().await.map_err(multipart_error)?;
        match filename {
            Some(fname) => {
                if name == "files" {
                    files.push(UploadedFile { filename: fname, content: data.to_vec() });
                }
            }
            None => {
                let v = String::from_utf8_lossy(&data).into_owned();
                if name == "files" {
                    files_non_file.push(v.clone());
                }
                fields.insert(name, v);
            }
        }
    }

    let mut errors: Vec<Value> = Vec::new();
    let mut req = |k: &str| -> String {
        match fields.get(k).filter(|v| !v.is_empty()) {
            Some(v) => v.clone(),
            None => {
                errors.push(missing(vec![json!("body"), json!(k)]));
                String::new()
            }
        }
    };
    let business_id = req("business_id");
    let doc_number = req("doc_number");
    let doc_type = req("doc_type");
    let product_type = fields.get("product_type").filter(|v| !v.is_empty()).cloned();
    let upload_type = fields.get("upload_type").filter(|v| !v.is_empty()).cloned();
    if files.is_empty() && files_non_file.is_empty() {
        errors.push(missing(vec![json!("body"), json!("files")]));
    } else if !files_non_file.is_empty() {
        for (i, v) in files_non_file.iter().enumerate() {
            errors.push(json!({
                "type": "value_error",
                "loc": ["body", "files", files.len() + i],
                "msg": "Value error, Expected UploadFile, received: <class 'str'>",
                "input": v,
                "ctx": {"error": {}},
                "url": "https://errors.pydantic.dev/2.5/v/value_error"
            }));
        }
    }
    crate::http::check(errors)?;

    if !py_isdigit(&business_id) {
        return Err(ApiError::new(400, "businessId必须为纯数字"));
    }
    // doc_number 会拼进文件名和存储路径, 拒绝路径分隔符、".." 和 HTML 特殊字符
    if !is_safe_doc_number(&doc_number) {
        return Err(ApiError::new(400, "doc_number包含非法字符"));
    }
    if !VALID_DOC_TYPES.contains(&doc_type.as_str()) {
        return Err(ApiError::new(
            400,
            format!("doc_type必须为以下值之一: {}", VALID_DOC_TYPES.join(", ")),
        ));
    }
    let upload_type_value = normalize_upload_type(upload_type.as_deref())?;
    let business_type = business_type_for(Some(&doc_type));

    if files.len() as i64 > s.max_files_per_request {
        return Err(ApiError::new(400, format!("单次最多上传{}个文件", s.max_files_per_request)));
    }

    for f in &files {
        let ext = file_ext(&f.filename);
        if !s.allowed_extensions.contains(&ext) {
            return Err(ApiError::new(
                400,
                format!(
                    "不支持的文件格式: {}，支持的格式: {}",
                    ext,
                    s.allowed_extensions.join(", ")
                ),
            ));
        }
    }

    let total = files.len();
    let mut records: Vec<Value> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();

    for f in files {
        let file_size = f.content.len();
        if file_size as i64 > s.max_file_size {
            return Err(ApiError::new(
                400,
                format!(
                    "文件 {} 大小超过{}MB限制",
                    f.filename,
                    py_float_repr(s.max_file_size as f64 / 1024.0 / 1024.0)
                ),
            ));
        }
        let file_extension = file_ext(&f.filename);
        let (new_filename, local_file_path) =
            generate_unique_filename(&doc_number, &file_extension, &s.local_storage_path);

        let now = beijing_now_iso();
        let params: Vec<SqlValue> = vec![
            db::text(business_id.clone()),
            db::text(doc_number.clone()),
            db::text(doc_type.clone()),
            db::opt(product_type.clone()),
            db::text(new_filename.clone()),
            SqlValue::Integer(file_size as i64),
            db::text(file_extension.clone()),
            db::text(now.clone()),
            db::text("pending"),
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Null,
            SqlValue::Integer(0),
            db::text(local_file_path.clone()),
            db::text(upload_type_value.clone()),
            db::text(now.clone()),
            db::text(now),
        ];
        let record_id = db::run(move |c| -> rusqlite::Result<i64> {
            c.execute(
                r#"
                INSERT INTO upload_history
                (business_id, doc_number, doc_type, product_type, file_name, file_size, file_extension,
                 upload_time, status, error_code, error_message, yonyou_file_id, retry_count,
                 local_file_path, upload_type, created_at, updated_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                rusqlite::params_from_iter(params.iter()),
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .map_err(|e| {
            println!("写入上传记录失败: {}", e);
            ApiError::new(500, "服务器内部错误")
        })?;

        if upload_type_value == UPLOAD_TYPE_LOGISTICS {
            jobs.push(Job::Logistics {
                content: f.content,
                filename: new_filename.clone(),
                local: local_file_path,
                id: record_id,
            });
        } else if upload_type_value == UPLOAD_TYPE_WAREHOUSE {
            jobs.push(Job::Warehouse {
                content: f.content,
                filename: new_filename.clone(),
                local: local_file_path,
                id: record_id,
            });
        }

        records.push(json!({
            "id": record_id,
            "file_name": new_filename,
            "original_name": f.filename,
            "status": "pending",
            "file_size": file_size,
            "upload_type": upload_type_value,
        }));
    }

    // 后台任务按顺序依次执行 (等价 FastAPI BackgroundTasks)
    let st = state.clone();
    tokio::spawn(async move {
        for job in jobs {
            match job {
                Job::Logistics { content, filename, local, id } => {
                    background_upload_to_yonyou(
                        st.clone(),
                        content,
                        filename,
                        business_id.clone(),
                        business_type.clone(),
                        local,
                        id,
                    )
                    .await
                }
                Job::Warehouse { content, filename, local, id } => {
                    background_save_warehouse_upload(st.clone(), content, filename, local, id).await
                }
            }
        }
    });

    ok_json(json!({
        "success": true,
        "total": total,
        "message": format!("已接收{}个文件，正在后台上传中", total),
        "records": records,
    }))
}
