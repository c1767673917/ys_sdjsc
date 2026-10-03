//! 数据库 (等价 app/core/database.py)
//!
//! 原实现用全局 RLock 串行化所有连接; 这里用单连接 + 互斥锁达到同样的串行语义,
//! 所有访问都在 blocking 线程池中执行, 不阻塞异步运行时。

use once_cell::sync::OnceCell;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde_json::Value;
use std::sync::Mutex;

use crate::config;

const LOG: &str = "app.core.database";

static CONN: OnceCell<Mutex<Connection>> = OnceCell::new();

fn open() -> rusqlite::Result<Connection> {
    let db_path = config::get().db_path();
    let dir = crate::pyutil::dirname(&db_path);
    if !dir.is_empty() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // 启用WAL模式以支持更好的并发读取
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA cache_size=1000; PRAGMA temp_store=MEMORY;")?;
    Ok(conn)
}

fn conn() -> &'static Mutex<Connection> {
    CONN.get_or_init(|| Mutex::new(open().expect("无法打开数据库")))
}

/// 在持有数据库锁的情况下执行同步闭包 (调用方需位于 blocking 上下文)
pub fn with_conn<T>(f: impl FnOnce(&mut Connection) -> T) -> T {
    let mut guard = match conn().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    f(&mut guard)
}

/// 在 blocking 线程池中执行数据库操作
pub async fn run<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || with_conn(f))
        .await
        .expect("数据库任务异常退出")
}

/// SQLite 值 -> JSON 值 (与 Python sqlite3 返回类型一致)
pub fn to_json(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(*i),
        SqlValue::Real(f) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
        SqlValue::Text(s) => Value::String(s.clone()),
        SqlValue::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
    }
}

pub fn opt_text(v: &SqlValue) -> Option<String> {
    match v {
        SqlValue::Null => None,
        SqlValue::Integer(i) => Some(i.to_string()),
        SqlValue::Real(f) => Some(crate::pyutil::py_float_repr(*f)),
        SqlValue::Text(s) => Some(s.clone()),
        SqlValue::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
    }
}

/// Python 真值 (bool(row[x]))
pub fn truthy(v: &SqlValue) -> bool {
    match v {
        SqlValue::Null => false,
        SqlValue::Integer(i) => *i != 0,
        SqlValue::Real(f) => *f != 0.0,
        SqlValue::Text(s) => !s.is_empty(),
        SqlValue::Blob(b) => !b.is_empty(),
    }
}

pub fn opt(s: Option<String>) -> SqlValue {
    match s {
        Some(s) => SqlValue::Text(s),
        None => SqlValue::Null,
    }
}

pub fn text(s: impl Into<String>) -> SqlValue {
    SqlValue::Text(s.into())
}

/// PRAGMA table_info 的列名集合
pub fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

/// 执行查询并以 Vec<Vec<SqlValue>> 返回所有行
pub fn query_rows(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> rusqlite::Result<Vec<Vec<SqlValue>>> {
    let mut stmt = conn.prepare(sql)?;
    let n = stmt.column_count();
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        let mut row = Vec::with_capacity(n);
        for i in 0..n {
            row.push(r.get::<_, SqlValue>(i)?);
        }
        Ok(row)
    })?;
    rows.collect()
}

/// 初始化数据库
pub fn init_database() -> rusqlite::Result<()> {
    with_conn(|conn| {
        let tx = conn.transaction()?;

        // 创建上传历史表
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS upload_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                business_id VARCHAR(50) NOT NULL,
                doc_number VARCHAR(100),
                doc_type VARCHAR(20),
                file_name VARCHAR(255) NOT NULL,
                file_size INTEGER NOT NULL,
                file_extension VARCHAR(20),
                upload_time DATETIME,
                status VARCHAR(20) NOT NULL,
                error_code VARCHAR(50),
                error_message TEXT,
                yonyou_file_id VARCHAR(255),
                retry_count INTEGER DEFAULT 0,
                local_file_path VARCHAR(500),
                logistics TEXT,
                customer_name TEXT,
                created_at DATETIME,
                updated_at DATETIME,
                deleted_at DATETIME
            )
            "#,
        )?;

        // 检查并新增字段（兼容现有数据库）
        let columns = table_columns(&tx, "upload_history")?;
        let has = |c: &str| columns.iter().any(|x| x == c);
        let alters: [(&str, &str); 10] = [
            ("doc_number", "ALTER TABLE upload_history ADD COLUMN doc_number VARCHAR(100)"),
            ("doc_type", "ALTER TABLE upload_history ADD COLUMN doc_type VARCHAR(20)"),
            ("local_file_path", "ALTER TABLE upload_history ADD COLUMN local_file_path VARCHAR(500)"),
            ("deleted_at", "ALTER TABLE upload_history ADD COLUMN deleted_at DATETIME DEFAULT NULL"),
            // 产品类型字段 (支持产品维度分类管理)
            ("product_type", "ALTER TABLE upload_history ADD COLUMN product_type TEXT DEFAULT NULL"),
            // 检查状态字段 (支持质量检查工作流)
            ("checked", "ALTER TABLE upload_history ADD COLUMN checked INTEGER DEFAULT 0"),
            // 备注字段
            ("notes", "ALTER TABLE upload_history ADD COLUMN notes TEXT DEFAULT NULL"),
            // 物流字段
            ("logistics", "ALTER TABLE upload_history ADD COLUMN logistics TEXT DEFAULT NULL"),
            // 客户名称字段
            ("customer_name", "ALTER TABLE upload_history ADD COLUMN customer_name TEXT DEFAULT NULL"),
            // 上传业务类型字段 (物流/仓库)
            ("upload_type", "ALTER TABLE upload_history ADD COLUMN upload_type VARCHAR(20)"),
        ];
        for (col, sql) in alters {
            if !has(col) {
                tx.execute_batch(sql)?;
            }
        }

        tx.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_business_id ON upload_history(business_id);
            CREATE INDEX IF NOT EXISTS idx_upload_time ON upload_history(upload_time);
            CREATE INDEX IF NOT EXISTS idx_status ON upload_history(status);
            CREATE INDEX IF NOT EXISTS idx_doc_number ON upload_history(doc_number);
            CREATE INDEX IF NOT EXISTS idx_doc_type ON upload_history(doc_type);
            CREATE INDEX IF NOT EXISTS idx_doc_type_upload_time ON upload_history(doc_type, upload_time);
            CREATE INDEX IF NOT EXISTS idx_deleted_at ON upload_history(deleted_at);
            CREATE INDEX IF NOT EXISTS idx_product_type ON upload_history(product_type);
            CREATE INDEX IF NOT EXISTS idx_checked ON upload_history(checked);
            CREATE INDEX IF NOT EXISTS idx_logistics ON upload_history(logistics);
            CREATE INDEX IF NOT EXISTS idx_customer_name ON upload_history(customer_name);
            CREATE INDEX IF NOT EXISTS idx_upload_type ON upload_history(upload_type);
            CREATE INDEX IF NOT EXISTS idx_upload_type_upload_time ON upload_history(upload_type, upload_time);

            CREATE TABLE IF NOT EXISTS logistics_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                logistics_name TEXT NOT NULL UNIQUE,
                token TEXT NOT NULL UNIQUE,
                enabled INTEGER DEFAULT 1,
                created_at DATETIME,
                updated_at DATETIME,
                last_access_at DATETIME
            );
            CREATE INDEX IF NOT EXISTS idx_lt_token ON logistics_tokens(token);

            CREATE TABLE IF NOT EXISTS delivery_snapshot (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                delivery_id TEXT NOT NULL UNIQUE,
                delivery_code TEXT,
                customer_name TEXT,
                vouchdate TEXT,
                logistics_name TEXT NOT NULL,
                freight REAL,
                shipping_memo TEXT,
                total_price_qty REAL,
                synced_at DATETIME
            );
            "#,
        )?;

        let snapshot_columns = table_columns(&tx, "delivery_snapshot")?;
        if !snapshot_columns.iter().any(|c| c == "shipping_memo") {
            tx.execute_batch("ALTER TABLE delivery_snapshot ADD COLUMN shipping_memo TEXT")?;
        }
        if !snapshot_columns.iter().any(|c| c == "total_price_qty") {
            tx.execute_batch("ALTER TABLE delivery_snapshot ADD COLUMN total_price_qty REAL")?;
        }

        tx.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_ds_logistics ON delivery_snapshot(logistics_name);
            CREATE INDEX IF NOT EXISTS idx_ds_code ON delivery_snapshot(delivery_code);
            CREATE TABLE IF NOT EXISTS app_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            "#,
        )?;

        tx.commit()
    })
}

/// 验证数据库schema是否包含WebDAV支持所需的必需字段
pub fn verify_database_schema() -> Result<(), String> {
    let result: Result<Result<(), String>, rusqlite::Error> = with_conn(|conn| {
        let columns = table_columns(conn, "upload_history")?;
        let required = [
            ("webdav_path", "WebDAV远程文件路径"),
            ("is_cached", "是否已缓存标志"),
            ("cache_expiry_time", "缓存过期时间"),
        ];
        let missing: Vec<_> = required
            .iter()
            .filter(|(f, _)| !columns.iter().any(|c| c == f))
            .collect();

        if !missing.is_empty() {
            let missing_list = missing
                .iter()
                .map(|(f, d)| format!("  - {}: {}", f, d))
                .collect::<Vec<_>>()
                .join("\n");
            let line = "=".repeat(60);
            let error_msg = format!(
                "\n{line}\n数据库Schema验证失败!\n{line}\nupload_history表缺少以下必需字段:\n{missing_list}\n\n修复方法:\n1. 如果有迁移脚本,请执行:\n   sqlite3 data/uploads.db < migrations/add_webdav_support.sql\n\n2. 或者手动添加字段:\n   ALTER TABLE upload_history ADD COLUMN webdav_path TEXT;\n   ALTER TABLE upload_history ADD COLUMN is_cached BOOLEAN DEFAULT 1;\n   ALTER TABLE upload_history ADD COLUMN cache_expiry_time DATETIME;\n{line}\n"
            );
            log::error!(target: LOG, "{}", error_msg);
            return Ok(Err(error_msg));
        }

        log::info!(target: LOG, "数据库Schema验证通过 - 所有必需字段都存在 ({}个)", required.len());

        let total: i64 = conn.query_row("SELECT COUNT(*) FROM upload_history", [], |r| r.get(0))?;
        let webdav: i64 = conn.query_row(
            "SELECT COUNT(*) FROM upload_history WHERE webdav_path IS NOT NULL AND webdav_path != ''",
            [],
            |r| r.get(0),
        )?;
        log::info!(target: LOG, "数据库统计: 总记录数={}, WebDAV记录数={}", total, webdav);
        Ok(Ok(()))
    });

    match result {
        Ok(r) => r,
        Err(e) => {
            let error_msg = format!("数据库Schema验证过程中出错: {}", e);
            log::error!(target: LOG, "{}", error_msg);
            Err(error_msg)
        }
    }
}
