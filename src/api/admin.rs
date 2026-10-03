//! 管理接口 (等价 app/api/admin.py), 挂载于 /api/admin, 需管理鉴权

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxPath, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::db;
use crate::file_manager::FileManager;
use crate::http::{self, body_bool, body_int_list, body_str, check, json_body, ok_json, ApiError, ApiResult, Query};
use crate::image_overlay::overlay_doc_number;
use crate::pyutil::{beijing_now_iso, beijing_now_naive, py_strip};
use crate::state::AppState;
use crate::upload_types::*;

const LOG: &str = "app.api.admin";

/// 规范化可选的上传业务类型筛选值
fn normalize_upload_type_filter(v: Option<String>) -> Result<Option<String>, ApiError> {
    let n = py_strip(v.as_deref().unwrap_or("")).to_string();
    if n.is_empty() {
        return Ok(None);
    }
    if !VALID_UPLOAD_TYPES.contains(&n.as_str()) {
        return Err(ApiError::new(
            400,
            format!("upload_type必须为以下值之一: {}", valid_upload_types_sorted()),
        ));
    }
    Ok(Some(n))
}

/// 追加 upload_type 条件 (历史 NULL/空值按物流处理)
pub fn append_upload_type_filter(
    where_clauses: &mut Vec<String>,
    params: &mut Vec<SqlValue>,
    upload_type: Option<&str>,
    has_upload_type: bool,
) {
    match (upload_type, has_upload_type) {
        (Some(UPLOAD_TYPE_LOGISTICS), true) => {
            where_clauses.push("COALESCE(NULLIF(upload_type, ''), ?) = ?".into());
            params.push(db::text(DEFAULT_UPLOAD_TYPE));
            params.push(db::text(UPLOAD_TYPE_LOGISTICS));
        }
        (Some(UPLOAD_TYPE_WAREHOUSE), true) => {
            where_clauses.push("upload_type = ?".into());
            params.push(db::text(UPLOAD_TYPE_WAREHOUSE));
        }
        (Some(UPLOAD_TYPE_WAREHOUSE), false) => where_clauses.push("1 = 0".into()),
        _ => {}
    }
}

/// 记录筛选条件 (records 与 export 共用)
struct Filters {
    search: Option<String>,
    doc_type: Option<String>,
    product_type: Option<String>,
    status: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    logistics: Option<String>,
    customer_name: Option<String>,
    upload_type: Option<String>,
}

impl Filters {
    fn from_query(q: &Query) -> Self {
        Filters {
            search: q.get("search"),
            doc_type: q.get("doc_type"),
            product_type: q.get("product_type"),
            status: q.get("status"),
            start_date: q.get("start_date"),
            end_date: q.get("end_date"),
            logistics: q.get("logistics"),
            customer_name: q.get("customer_name"),
            upload_type: q.get("upload_type"),
        }
    }

    fn build(&self, upload_type: Option<&str>, has_upload_type: bool) -> (String, Vec<SqlValue>) {
        let nonempty = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(|s| s.to_string());
        let mut w: Vec<String> = vec!["deleted_at IS NULL".into()];
        let mut p: Vec<SqlValue> = Vec::new();
        if let Some(s) = nonempty(&self.search) {
            w.push("(doc_number LIKE ? OR file_name LIKE ?)".into());
            let pat = format!("%{}%", s);
            p.push(db::text(pat.clone()));
            p.push(db::text(pat));
        }
        if let Some(v) = nonempty(&self.doc_type) {
            w.push("doc_type = ?".into());
            p.push(db::text(v));
        }
        if let Some(v) = nonempty(&self.product_type) {
            w.push("product_type = ?".into());
            p.push(db::text(v));
        }
        if let Some(v) = nonempty(&self.status) {
            w.push("status = ?".into());
            p.push(db::text(v));
        }
        if let Some(v) = nonempty(&self.start_date) {
            w.push("DATE(upload_time) >= ?".into());
            p.push(db::text(v));
        }
        if let Some(v) = nonempty(&self.end_date) {
            w.push("DATE(upload_time) <= ?".into());
            p.push(db::text(v));
        }
        if let Some(v) = nonempty(&self.logistics) {
            if v != "全部物流" {
                w.push("logistics = ?".into());
                p.push(db::text(v));
            }
        }
        if let Some(v) = &self.customer_name {
            let t = py_strip(v);
            if !t.is_empty() {
                w.push("customer_name LIKE ?".into());
                p.push(db::text(format!("%{}%", t)));
            }
        }
        append_upload_type_filter(&mut w, &mut p, upload_type, has_upload_type);
        (w.join(" AND "), p)
    }
}

fn db_err(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(500, e.to_string())
}

/// GET /api/admin/records
pub async fn get_records(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Query::parse(raw.as_deref());
    let mut errors = Vec::new();
    let page = q.int("page", 1, Some(1), None, &mut errors);
    let page_size = q.int("page_size", 20, Some(1), Some(100), &mut errors);
    check(errors)?;
    let filters = Filters::from_query(&q);
    let upload_type = normalize_upload_type_filter(filters.upload_type.clone())?;

    let result = db::run(move |c| -> rusqlite::Result<Value> {
        let cols = db::table_columns(c, "upload_history")?;
        let has_upload_type = cols.iter().any(|x| x == "upload_type");
        let upload_type_select = if has_upload_type {
            "COALESCE(NULLIF(upload_type, ''), ?) AS upload_type"
        } else {
            "? AS upload_type"
        };
        let (where_sql, params) = filters.build(upload_type.as_deref(), has_upload_type);

        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM upload_history WHERE {}", where_sql),
            rusqlite::params_from_iter(params.iter()),
            |r| r.get(0),
        )?;
        let total_pages = (total + page_size - 1).div_euclid(page_size);
        let offset = (page - 1) * page_size;

        let mut all: Vec<SqlValue> = vec![db::text(DEFAULT_UPLOAD_TYPE)];
        all.extend(params);
        all.push(SqlValue::Integer(page_size));
        all.push(SqlValue::Integer(offset));
        let rows = db::query_rows(
            c,
            &format!(
                r#"
            SELECT id, business_id, doc_number, doc_type, product_type, file_name, file_size,
                   upload_time, status, error_code, error_message, checked, notes, logistics,
                   customer_name, {}
            FROM upload_history
            WHERE {}
            ORDER BY upload_time DESC
            LIMIT ? OFFSET ?
        "#,
                upload_type_select, where_sql
            ),
            &all,
        )?;
        let records: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": db::to_json(&r[0]),
                    "business_id": db::to_json(&r[1]),
                    "doc_number": db::to_json(&r[2]),
                    "doc_type": db::to_json(&r[3]),
                    "product_type": db::to_json(&r[4]),
                    "file_name": db::to_json(&r[5]),
                    "file_size": db::to_json(&r[6]),
                    "upload_time": db::to_json(&r[7]),
                    "status": db::to_json(&r[8]),
                    "error_code": db::to_json(&r[9]),
                    "error_message": db::to_json(&r[10]),
                    "checked": db::truthy(&r[11]),
                    "notes": db::to_json(&r[12]),
                    "logistics": db::to_json(&r[13]),
                    "customer_name": db::to_json(&r[14]),
                    "upload_type": db::to_json(&r[15]),
                })
            })
            .collect();
        Ok(json!({
            "total": total,
            "page": page,
            "page_size": page_size,
            "total_pages": total_pages,
            "records": records,
        }))
    })
    .await
    .map_err(db_err)?;
    ok_json(result)
}

/// GET /api/admin/logistics-options
pub async fn logistics_options() -> ApiResult {
    let rows = db::run(|c| {
        db::query_rows(
            c,
            r#"
            SELECT DISTINCT logistics
            FROM upload_history
            WHERE logistics IS NOT NULL AND logistics != ''
            ORDER BY logistics ASC
        "#,
            &[],
        )
    })
    .await
    .map_err(db_err)?;
    let mut list = vec![json!("全部物流")];
    for r in rows {
        if db::truthy(&r[0]) {
            list.push(db::to_json(&r[0]));
        }
    }
    ok_json(json!({"logistics_list": list}))
}

struct ImageJob {
    arcname: String,
    local_path: Option<String>,
    webdav_path: Option<String>,
    doc_number: Option<String>,
}

/// 临时目录随响应体一起释放 (等价 BackgroundTask(shutil.rmtree, temp_dir))
fn stream_file_then_cleanup(path: std::path::PathBuf, dir: tempfile::TempDir) -> Body {
    let stream = async_stream_file(path, dir);
    Body::from_stream(stream)
}

fn async_stream_file(
    path: std::path::PathBuf,
    dir: tempfile::TempDir,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    futures::stream::once(async move { tokio::fs::File::open(path).await })
        .map(move |f| {
            let _keep = &dir;
            f
        })
        .flat_map(|res| match res {
            Ok(file) => tokio_util::io::ReaderStream::new(file).left_stream(),
            Err(e) => futures::stream::once(async move { Err(e) }).right_stream(),
        })
}

fn export_file_response(path: std::path::PathBuf, dir: tempfile::TempDir, media: &str, filename: &str, empty: bool) -> Response {
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, media)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, http::content_disposition("attachment", filename));
    if empty {
        b = b.header("X-Export-Empty", "true");
    }
    // tempdir 在流结束(或连接断开)时被释放
    let body = stream_file_then_cleanup(path, dir);
    b.body(body).unwrap()
}

/// GET /api/admin/export
pub async fn export_records(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Query::parse(raw.as_deref());
    let mut errors = Vec::new();
    let include_excel = q.bool("include_excel", true, &mut errors);
    let include_images = q.bool("include_images", true, &mut errors);
    let include_doc_number = q.bool("include_doc_number", true, &mut errors);
    check(errors)?;

    if !include_excel && !include_images {
        return Err(ApiError::new(400, "至少选择一项导出内容 (include_excel 或 include_images)"));
    }
    let filters = Filters::from_query(&q);
    let upload_type = normalize_upload_type_filter(filters.upload_type.clone())?;

    let r: Result<Response, String> = async {
        let rows = db::run(move |c| -> rusqlite::Result<Vec<Vec<SqlValue>>> {
            let cols = db::table_columns(c, "upload_history")?;
            let has_upload_type = cols.iter().any(|x| x == "upload_type");
            let upload_type_select = if has_upload_type {
                "COALESCE(NULLIF(upload_type, ''), ?) AS upload_type"
            } else {
                "? AS upload_type"
            };
            let (where_sql, params) = filters.build(upload_type.as_deref(), has_upload_type);
            let webdav_select = if cols.iter().any(|x| x == "webdav_path") { "webdav_path" } else { "NULL as webdav_path" };
            let mut all = vec![db::text(DEFAULT_UPLOAD_TYPE)];
            all.extend(params);
            db::query_rows(
                c,
                &format!(
                    r#"
                SELECT {},
                       doc_number, doc_type, product_type, customer_name, business_id, upload_time, file_name,
                       file_size, status, local_file_path, notes, {}
                FROM upload_history
                WHERE {}
                ORDER BY upload_time DESC
            "#,
                    upload_type_select, webdav_select, where_sql
                ),
                &all,
            )
        })
        .await
        .map_err(|e| e.to_string())?;

        log::info!(
            target: LOG,
            "[导出] 查询到 {} 条记录, include_excel={}, include_images={}, include_doc_number={}",
            rows.len(),
            py_bool(include_excel),
            py_bool(include_images),
            py_bool(include_doc_number)
        );

        let temp_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let timestamp = beijing_now_naive().format("%Y%m%d_%H%M%S").to_string();
        let is_empty = rows.is_empty();

        // 生成Excel
        let mut excel_path: Option<std::path::PathBuf> = None;
        if include_excel {
            let excel_filename = format!("upload_records_{}.xlsx", timestamp);
            let p = temp_dir.path().join(&excel_filename);
            write_excel(&rows, &p).map_err(|e| format!("Excel {}", e))?;
            log::info!(target: LOG, "[导出] Excel生成完成: {}", excel_filename);
            excel_path = Some(p);
        }

        // 收集图片文件
        let mut image_files: Vec<ImageJob> = Vec::new();
        if include_images {
            for r in &rows {
                let doc_number = db::opt_text(&r[1]);
                let business_id = db::opt_text(&r[5]).unwrap_or_else(|| "None".into());
                let file_name = db::opt_text(&r[7]).filter(|s| !s.is_empty());
                let local = db::opt_text(&r[10]).filter(|s| !s.is_empty());
                let webdav = db::opt_text(&r[12]).filter(|s| !s.is_empty());
                let base = file_name.unwrap_or_else(|| {
                    format!(
                        "{}_{}",
                        business_id,
                        doc_number.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into())
                    )
                });
                // ZIP 条目名只保留 basename, 防止解压时 "../" 越出 images/
                let base = base.replace('\\', "/").rsplit('/').next().unwrap_or("").replace("..", "_");
                let arcname = format!("images/{}", if base.is_empty() { "unknown".to_string() } else { base });
                if local.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false) {
                    image_files.push(ImageJob { arcname, local_path: local, webdav_path: None, doc_number });
                } else if webdav.is_some() {
                    image_files.push(ImageJob { arcname, local_path: None, webdav_path: webdav, doc_number });
                } else {
                    log::debug!(
                        target: LOG,
                        "[导出] 记录无可用图片 doc_number={} business_id={}",
                        doc_number.clone().unwrap_or_else(|| "None".into()),
                        business_id
                    );
                }
            }
        }

        if include_excel && include_images {
            let zip_filename = format!("upload_records_{}.zip", timestamp);
            let zip_path = temp_dir.path().join(&zip_filename);
            let zip = new_zip(&zip_path)?;
            {
                let excel = excel_path.clone().unwrap();
                let name = format!("upload_records_{}.xlsx", timestamp);
                let zip2 = zip.clone();
                tokio::task::spawn_blocking(move || -> Result<(), String> {
                    let data = std::fs::read(&excel).map_err(|e| e.to_string())?;
                    zip_write(&zip2, &name, &data)
                })
                .await
                .map_err(|e| e.to_string())??;
            }
            let (local, webdav, _missing) = add_images_to_zip(&zip, image_files, include_doc_number).await;
            if local == 0 && webdav == 0 {
                zip_add_dir(&zip, "images/")?;
            }
            finish_zip(zip)?;
            log::info!(target: LOG, "[导出] ZIP打包完成: {}", zip_filename);
            return Ok(export_file_response(zip_path, temp_dir, "application/zip", &zip_filename, is_empty));
        }

        if include_excel && !include_images {
            let name = format!("upload_records_{}.xlsx", timestamp);
            return Ok(export_file_response(
                excel_path.unwrap(),
                temp_dir,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &name,
                is_empty,
            ));
        }

        // 仅图片
        let zip_filename = format!("images_{}.zip", timestamp);
        let zip_path = temp_dir.path().join(&zip_filename);
        let zip = new_zip(&zip_path)?;
        let (local, webdav, missing_count) = add_images_to_zip(&zip, image_files, include_doc_number).await;
        if local == 0 && webdav == 0 {
            zip_add_dir(&zip, "images/")?;
        }
        finish_zip(zip)?;
        log::info!(
            target: LOG,
            "[导出] 图片ZIP打包完成: {}, 本地={}, WebDAV={}, 缺失={}",
            zip_filename,
            local,
            webdav,
            missing_count
        );
        Ok(export_file_response(
            zip_path,
            temp_dir,
            "application/zip",
            &zip_filename,
            is_empty || (local == 0 && webdav == 0),
        ))
    }
    .await;

    r.map_err(|e| {
        log::error!(target: LOG, "[导出] 导出失败: {}", e);
        if e.contains("Excel") || e.contains("Workbook") {
            ApiError::new(500, "Excel文件生成失败")
        } else if e.contains("ZIP") || e.contains("zipfile") {
            ApiError::new(500, "压缩文件创建失败")
        } else {
            ApiError::new(500, format!("导出失败: {}", e))
        }
    })
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

fn write_excel(rows: &[Vec<SqlValue>], path: &Path) -> Result<(), String> {
    use rust_xlsxwriter::Workbook;
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("上传记录").map_err(|e| e.to_string())?;
    let headers = [
        "上传业务类型", "单据编号", "单据类型", "产品类型", "客户名称", "业务ID", "上传时间", "文件名", "文件大小(字节)", "状态", "备注",
    ];
    for (i, h) in headers.iter().enumerate() {
        ws.write_string(0, i as u16, *h).map_err(|e| e.to_string())?;
    }
    for (ri, r) in rows.iter().enumerate() {
        let row = (ri + 1) as u32;
        // upload_type, doc_number, doc_type, product_type or '', customer_name or '',
        // business_id, upload_time, file_name, file_size, status, notes or ''
        let cells: [(&SqlValue, bool); 11] = [
            (&r[0], false),
            (&r[1], false),
            (&r[2], false),
            (&r[3], true),
            (&r[4], true),
            (&r[5], false),
            (&r[6], false),
            (&r[7], false),
            (&r[8], false),
            (&r[9], false),
            (&r[11], true),
        ];
        for (ci, (v, or_empty)) in cells.iter().enumerate() {
            let col = ci as u16;
            let empty_like = !db::truthy(v);
            match v {
                _ if *or_empty && empty_like => {
                    ws.write_string(row, col, "").map_err(|e| e.to_string())?;
                }
                SqlValue::Null => {}
                SqlValue::Integer(i) => {
                    ws.write_number(row, col, *i as f64).map_err(|e| e.to_string())?;
                }
                SqlValue::Real(f) => {
                    ws.write_number(row, col, *f).map_err(|e| e.to_string())?;
                }
                SqlValue::Text(s) => {
                    ws.write_string(row, col, s).map_err(|e| e.to_string())?;
                }
                SqlValue::Blob(b) => {
                    ws.write_string(row, col, String::from_utf8_lossy(b)).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    wb.save(path).map_err(|e| e.to_string())
}

type SharedZip = Arc<Mutex<Option<zip::ZipWriter<std::fs::File>>>>;

fn new_zip(path: &Path) -> Result<SharedZip, String> {
    let f = std::fs::File::create(path).map_err(|e| format!("ZIP {}", e))?;
    Ok(Arc::new(Mutex::new(Some(zip::ZipWriter::new(f)))))
}

fn zip_options() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(false)
}

fn zip_write(zip: &SharedZip, name: &str, data: &[u8]) -> Result<(), String> {
    let mut g = zip.lock().unwrap();
    let z = g.as_mut().ok_or("ZIP closed")?;
    z.start_file(name, zip_options().large_file(data.len() as u64 >= 0xFFFF_FFFF))
        .map_err(|e| format!("ZIP {}", e))?;
    z.write_all(data).map_err(|e| format!("ZIP {}", e))
}

fn zip_add_dir(zip: &SharedZip, name: &str) -> Result<(), String> {
    let mut g = zip.lock().unwrap();
    let z = g.as_mut().ok_or("ZIP closed")?;
    z.add_directory(name.trim_end_matches('/'), zip::write::SimpleFileOptions::default())
        .map_err(|e| format!("ZIP {}", e))
}

fn finish_zip(zip: SharedZip) -> Result<(), String> {
    let z = zip.lock().unwrap().take().ok_or("ZIP closed")?;
    z.finish().map_err(|e| format!("ZIP {}", e))?;
    Ok(())
}

/// 将图片加入ZIP: 本地文件顺序处理, WebDAV 下载并发(上限15); 返回 (本地数, WebDAV数, 缺失数)
async fn add_images_to_zip(zip: &SharedZip, jobs: Vec<ImageJob>, include_doc_number: bool) -> (i64, i64, i64) {
    let (mut local_count, mut webdav_count, mut missing) = (0i64, 0i64, 0i64);
    let mut downloads: Vec<ImageJob> = Vec::new();

    for img in jobs {
        if let Some(lp) = img.local_path.clone() {
            let zip2 = zip.clone();
            let arc = img.arcname.clone();
            let doc = img.doc_number.clone();
            let r = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let raw = std::fs::read(&lp).map_err(|e| e.to_string())?;
                let data = if include_doc_number { overlay_doc_number(&raw, doc.as_deref()) } else { raw };
                zip_write(&zip2, &arc, &data)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok(_) => local_count += 1,
                Err(e) => {
                    log::warn!(target: LOG, "[导出] 本地图片添加失败: {}, 错误: {}", img.arcname, e);
                    missing += 1;
                }
            }
        } else if img.webdav_path.is_some() {
            downloads.push(img);
        } else {
            missing += 1;
        }
    }

    if !downloads.is_empty() {
        let fm = Arc::new(FileManager::new());
        let results: Vec<bool> = futures::stream::iter(downloads.into_iter().map(|job| {
            let fm = fm.clone();
            let zip = zip.clone();
            async move {
                let wp = job.webdav_path.clone().unwrap();
                match fm.get_file(&wp).await {
                    Ok(content) => {
                        let arc = job.arcname.clone();
                        let doc = job.doc_number.clone();
                        let r = tokio::task::spawn_blocking(move || {
                            let data = if include_doc_number {
                                overlay_doc_number(&content, doc.as_deref())
                            } else {
                                content
                            };
                            zip_write(&zip, &arc, &data)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                        match r {
                            Ok(_) => true,
                            Err(e) => {
                                log::warn!(
                                    target: LOG,
                                    "[导出] WebDAV图片获取失败 doc_number={} webdav_path={} 错误={}",
                                    job.doc_number.clone().unwrap_or_else(|| "None".into()),
                                    wp,
                                    e
                                );
                                false
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            target: LOG,
                            "[导出] WebDAV图片获取失败 doc_number={} webdav_path={} 错误={}",
                            job.doc_number.clone().unwrap_or_else(|| "None".into()),
                            wp,
                            e
                        );
                        false
                    }
                }
            }
        }))
        .buffer_unordered(15)
        .collect()
        .await;
        let ok = results.iter().filter(|r| **r).count() as i64;
        webdav_count += ok;
        missing += results.len() as i64 - ok;
    }
    (local_count, webdav_count, missing)
}

/// GET /api/admin/statistics
pub async fn statistics() -> ApiResult {
    let v = db::run(|c| -> rusqlite::Result<Value> {
        let row = db::query_rows(
            c,
            r#"
            SELECT
                COUNT(*) as total,
                SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END) as pending,
                SUM(CASE WHEN status = 'uploading' THEN 1 ELSE 0 END) as uploading,
                SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as success,
                SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) as failed
            FROM upload_history
            WHERE deleted_at IS NULL
        "#,
            &[],
        )?
        .remove(0);
        let or0 = |v: &SqlValue| if db::truthy(v) { db::to_json(v) } else { json!(0) };
        let total = db::to_json(&row[0]);

        let mut by_doc_type = Map::new();
        for r in db::query_rows(
            c,
            r#"
            SELECT doc_type, COUNT(*) as count
            FROM upload_history
            WHERE doc_type IS NOT NULL AND deleted_at IS NULL
            GROUP BY doc_type
        "#,
            &[],
        )? {
            by_doc_type.insert(json_key(&r[0]), db::to_json(&r[1]));
        }

        let mut by_upload_type = Map::new();
        by_upload_type.insert(UPLOAD_TYPE_LOGISTICS.into(), total.clone());
        by_upload_type.insert(UPLOAD_TYPE_WAREHOUSE.into(), json!(0));
        let cols = db::table_columns(c, "upload_history")?;
        if cols.iter().any(|x| x == "upload_type") {
            by_upload_type = Map::new();
            by_upload_type.insert(UPLOAD_TYPE_LOGISTICS.into(), json!(0));
            by_upload_type.insert(UPLOAD_TYPE_WAREHOUSE.into(), json!(0));
            for r in db::query_rows(
                c,
                r#"
                SELECT COALESCE(NULLIF(upload_type, ''), ?) AS upload_type, COUNT(*) as count
                FROM upload_history
                WHERE deleted_at IS NULL
                GROUP BY COALESCE(NULLIF(upload_type, ''), ?)
            "#,
                &[db::text(DEFAULT_UPLOAD_TYPE), db::text(DEFAULT_UPLOAD_TYPE)],
            )? {
                by_upload_type.insert(json_key(&r[0]), db::to_json(&r[1]));
            }
        }

        Ok(json!({
            "total_uploads": total,
            "pending_count": or0(&row[1]),
            "uploading_count": or0(&row[2]),
            "success_count": or0(&row[3]),
            "failed_count": or0(&row[4]),
            "by_doc_type": by_doc_type,
            "by_upload_type": by_upload_type,
        }))
    })
    .await
    .map_err(db_err)?;
    ok_json(v)
}

/// JSON 对象键 (Python json.dumps 对非字符串键的转换)
fn json_key(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => "null".into(),
        SqlValue::Integer(i) => i.to_string(),
        SqlValue::Real(f) => crate::pyutil::py_float_repr(*f),
        SqlValue::Text(s) => s.clone(),
        SqlValue::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// DELETE /api/admin/records — 软删除(批量)
pub async fn delete_records(body: Bytes) -> ApiResult {
    let m = json_body(&body)?;
    let mut errors = Vec::new();
    let ids = body_int_list(&m, "ids", &mut errors);
    check(errors)?;
    if ids.is_empty() {
        return Err(ApiError::new(400, "请至少选择一条记录"));
    }
    if ids.iter().any(|i| *i <= 0) {
        return Err(ApiError::new(400, "无效的记录ID"));
    }
    let now = beijing_now_iso();
    let deleted = db::run(move |c| -> rusqlite::Result<usize> {
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut params: Vec<SqlValue> = vec![db::text(now)];
        params.extend(ids.iter().map(|i| SqlValue::Integer(*i)));
        c.execute(
            &format!(
                "UPDATE upload_history SET deleted_at = ? WHERE id IN ({}) AND deleted_at IS NULL",
                placeholders
            ),
            rusqlite::params_from_iter(params.iter()),
        )
    })
    .await
    .map_err(|e| ApiError::new(500, format!("删除失败: {}", e)))?;
    ok_json(json!({
        "success": true,
        "deleted_count": deleted,
        "message": format!("成功删除{}条记录", deleted),
    }))
}

async fn record_exists(id: i64) -> Result<bool, String> {
    db::run(move |c| {
        db::query_rows(
            c,
            "SELECT id FROM upload_history WHERE id = ? AND deleted_at IS NULL",
            &[SqlValue::Integer(id)],
        )
        .map(|r| !r.is_empty())
        .map_err(|e| e.to_string())
    })
    .await
}

/// PATCH /api/admin/records/{record_id}/check
pub async fn update_check(AxPath(record_id): AxPath<String>, body: Bytes) -> ApiResult {
    let id_res = http::path_int("record_id", &record_id);
    let body_res = json_body(&body);
    // 路径与请求体错误合并报告
    let id = match (id_res, &body_res) {
        (Ok(i), _) => i,
        (Err(mut e), Err(be)) => {
            if let (Value::Array(a), Value::Array(b)) = (&mut e.detail, &be.detail) {
                a.extend(b.clone());
            }
            return Err(e);
        }
        (Err(e), Ok(_)) => return Err(e),
    };
    let m = body_res?;
    let mut errors = Vec::new();
    let checked = body_bool(&m, "checked", None, &mut errors);
    check(errors)?;

    if !record_exists(id).await.map_err(|e| ApiError::new(500, format!("更新失败: {}", e)))? {
        return Err(ApiError::new(404, "记录不存在或已删除"));
    }
    let now = beijing_now_iso();
    db::run(move |c| {
        c.execute(
            "UPDATE upload_history SET checked = ?, updated_at = ? WHERE id = ?",
            rusqlite::params![checked as i64, now, id],
        )
    })
    .await
    .map_err(|e| ApiError::new(500, format!("更新失败: {}", e)))?;
    ok_json(json!({"success": true, "id": id, "checked": checked, "message": "检查状态已更新"}))
}

/// PATCH /api/admin/records/{record_id}/notes
pub async fn update_notes(AxPath(record_id): AxPath<String>, body: Bytes) -> ApiResult {
    let id_res = http::path_int("record_id", &record_id);
    let body_res = json_body(&body);
    let id = match (id_res, &body_res) {
        (Ok(i), _) => i,
        (Err(mut e), Err(be)) => {
            if let (Value::Array(a), Value::Array(b)) = (&mut e.detail, &be.detail) {
                a.extend(b.clone());
            }
            return Err(e);
        }
        (Err(e), Ok(_)) => return Err(e),
    };
    let m = body_res?;
    let mut errors = Vec::new();
    let notes = body_str(&m, "notes", &mut errors);
    check(errors)?;

    if notes.chars().count() > 1000 {
        return Err(ApiError::new(400, "备注内容不能超过1000字符"));
    }
    if !record_exists(id).await.map_err(|e| ApiError::new(500, format!("更新失败: {}", e)))? {
        return Err(ApiError::new(404, "记录不存在或已删除"));
    }
    let now = beijing_now_iso();
    let stripped = py_strip(&notes).to_string();
    let notes_value = if stripped.is_empty() { None } else { Some(stripped) };
    let nv = notes_value.clone();
    db::run(move |c| {
        c.execute(
            "UPDATE upload_history SET notes = ?, updated_at = ? WHERE id = ?",
            rusqlite::params![nv, now, id],
        )
    })
    .await
    .map_err(|e| ApiError::new(500, format!("更新失败: {}", e)))?;
    ok_json(json!({"success": true, "id": id, "notes": notes_value, "message": "备注已更新"}))
}

fn mime_for_ext(ext: Option<&str>) -> &'static str {
    match ext.map(|e| e.to_lowercase()).as_deref() {
        Some(".jpg") | Some(".jpeg") => "image/jpeg",
        Some(".png") => "image/png",
        Some(".gif") => "image/gif",
        Some(".bmp") => "image/bmp",
        Some(".webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

fn ms(start: Instant) -> String {
    format!("{:.2}", start.elapsed().as_secs_f64() * 1000.0)
}

/// GET /api/admin/files/{record_id}/preview
pub async fn preview_file(AxPath(record_id): AxPath<String>) -> ApiResult {
    let record_id = http::path_int("record_id", &record_id)?;
    let fm = FileManager::new();
    let start = Instant::now();
    let row = db::run(move |c| {
        db::query_rows(
            c,
            "SELECT local_file_path, file_extension, file_name, webdav_path FROM upload_history WHERE id = ? AND deleted_at IS NULL",
            &[SqlValue::Integer(record_id)],
        )
    })
    .await
    .map_err(|e| {
        log::error!(target: LOG, "[性能] 预览失败 record_id={} 耗时={}ms 错误={}", record_id, ms(start), e);
        ApiError::new(500, format!("预览失败: {}", e))
    })?;
    let Some(row) = row.into_iter().next() else {
        return Err(ApiError::new(404, "记录不存在或已删除"));
    };
    let local = db::opt_text(&row[0]).filter(|s| !s.is_empty());
    let ext = db::opt_text(&row[1]);
    let file_name = db::opt_text(&row[2]).unwrap_or_else(|| "None".into());
    let webdav = db::opt_text(&row[3]).filter(|s| !s.is_empty());
    let media = mime_for_ext(ext.as_deref());

    if let Some(lp) = local.filter(|p| Path::new(p).exists()) {
        log::info!(target: LOG, "[性能] 预览文件 record_id={} 方式=本地文件 耗时={}ms", record_id, ms(start));
        return http::file_response(&lp, Some(media), Some(&file_name), &[])
            .await
            .map_err(|e| ApiError::new(500, format!("预览失败: {}", e)));
    }

    if let Some(wp) = webdav {
        let cache_path = fm.get_cache_path(&wp);
        let method = if fm.is_cache_valid(&cache_path) { "cache" } else { "webdav" };
        match fm.get_file(&wp).await {
            Ok(content) => {
                log::info!(target: LOG, "[性能] 预览文件 record_id={} 方式={} 耗时={}ms", record_id, method, ms(start));
                let cd = http::header_value(&http::content_disposition("inline", &file_name))
                    .map_err(|e| ApiError::new(500, format!("预览失败: {}", e)))?;
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, media)
                    .header(header::CONTENT_LENGTH, content.len())
                    .header(header::CONTENT_DISPOSITION, cd)
                    .header(header::CACHE_CONTROL, "public, max-age=3600")
                    .body(Body::from(content))
                    .unwrap());
            }
            Err(e) => {
                log::warn!(
                    target: LOG,
                    "[性能] 预览文件失败 record_id={} 方式={} 耗时={}ms 错误={}",
                    record_id,
                    method,
                    ms(start),
                    e
                );
            }
        }
    }
    Err(ApiError::new(404, "文件不存在或无法访问"))
}

/// GET /api/admin/files/{record_id}/download
pub async fn download_file(AxPath(record_id): AxPath<String>) -> ApiResult {
    let record_id = http::path_int("record_id", &record_id)?;
    let fm = FileManager::new();
    let start = Instant::now();
    let row = db::run(move |c| {
        db::query_rows(
            c,
            "SELECT local_file_path, file_name, webdav_path FROM upload_history WHERE id = ? AND deleted_at IS NULL",
            &[SqlValue::Integer(record_id)],
        )
    })
    .await
    .map_err(|e| {
        log::error!(target: LOG, "[性能] 下载失败 record_id={} 耗时={}ms 错误={}", record_id, ms(start), e);
        ApiError::new(500, format!("下载失败: {}", e))
    })?;
    let Some(row) = row.into_iter().next() else {
        return Err(ApiError::new(404, "记录不存在或已删除"));
    };
    let local = db::opt_text(&row[0]).filter(|s| !s.is_empty());
    let file_name = db::opt_text(&row[1]).unwrap_or_else(|| "None".into());
    let webdav = db::opt_text(&row[2]).filter(|s| !s.is_empty());

    // 非 ASCII / 含引号的文件名用 RFC 5987 filename*, 避免 latin-1 编码失败和头部拆分
    let cd = http::content_disposition("attachment", &file_name);
    if let Some(lp) = local.filter(|p| Path::new(p).exists()) {
        log::info!(target: LOG, "[性能] 下载文件 record_id={} 方式=本地文件 耗时={}ms", record_id, ms(start));
        return http::file_response(
            &lp,
            Some("application/octet-stream"),
            Some(&file_name),
            &[("content-disposition", cd)],
        )
        .await
        .map_err(|e| ApiError::new(500, format!("下载失败: {}", e)));
    }

    if let Some(wp) = webdav {
        let cache_path = fm.get_cache_path(&wp);
        let method = if fm.is_cache_valid(&cache_path) { "cache" } else { "webdav" };
        match fm.get_file(&wp).await {
            Ok(content) => {
                log::info!(target: LOG, "[性能] 下载文件 record_id={} 方式={} 耗时={}ms", record_id, method, ms(start));
                let cdv = http::header_value(&cd).map_err(|e| ApiError::new(500, format!("下载失败: {}", e)))?;
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, content.len())
                    .header(header::CONTENT_DISPOSITION, cdv)
                    .body(Body::from(content))
                    .unwrap());
            }
            Err(e) => {
                log::warn!(
                    target: LOG,
                    "[性能] 下载文件失败 record_id={} 方式={} 耗时={}ms 错误={}",
                    record_id,
                    method,
                    ms(start),
                    e
                );
            }
        }
    }
    Err(ApiError::new(404, "文件不存在或无法访问"))
}

#[allow(dead_code)]
pub type Shared = State<Arc<AppState>>;
