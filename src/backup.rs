//! 备份服务 (等价 app/core/backup_service.py)
//!
//! 注意: 原实现的 `_log_backup_result` / `get_backup_status` 以 `conn = get_db_connection()`
//! 的方式使用上下文管理器(未用 with), 调用 `conn.cursor()` 必然抛出 AttributeError 并被捕获。
//! 因此原系统实际上从不写入 backup_logs, 备份状态接口也总是返回空统计。
//! 为保持行为一致, 这里原样保留该表现(记录相同的错误日志)。

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use flate2::write::GzEncoder;
use flate2::Compression;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::path::Path;

use crate::config;
use crate::pyutil::{basename, beijing_now_iso, beijing_now_naive, isoformat, path_join};
use crate::webdav::WebDavClient;

const LOG: &str = "app.core.backup_service";
const CONTEXT_MANAGER_ERROR: &str = "'_GeneratorContextManager' object has no attribute 'cursor'";

/// 定时任务与手动备份共用，整段流程互斥
static BACKUP_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

pub struct BackupService {
    webdav_client: WebDavClient,
    db_path: String,
    env_path: String,
    backup_dir: String,
}

struct CreateResult {
    success: bool,
    filename: String,
    backup_path: String,
    file_size: u64,
    error: Option<String>,
}

impl BackupService {
    pub fn new() -> Self {
        let s = config::get();
        let svc = BackupService {
            webdav_client: WebDavClient::new(),
            db_path: s.db_path(),
            env_path: ".env".into(),
            backup_dir: "temp_backups".into(),
        };
        let _ = std::fs::create_dir_all(&svc.backup_dir);
        log::info!(target: LOG, "备份服务初始化完成");
        log::info!(target: LOG, "数据库路径: {}", svc.db_path);
        log::info!(target: LOG, "配置文件路径: {}", svc.env_path);
        log::info!(target: LOG, "备份保留天数: {}", s.backup_retention_days);
        svc
    }

    fn generate_backup_filename(&self) -> String {
        format!("backup_{}.tar.gz", beijing_now_naive().format("%Y%m%d_%H%M%S"))
    }

    fn generate_backup_metadata(&self) -> String {
        let s = config::get();
        let v = json!({
            "backup_time": beijing_now_iso(),
            "app_name": s.app_name,
            "app_version": s.app_version,
            "database_path": self.db_path,
            "config_included": Path::new(&self.env_path).exists(),
            "compression_level": s.backup_compression_level,
            "backup_type": "scheduled",
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }

    /// 创建数据库备份 (tar.gz: uploads.db + .env + backup_metadata.json)
    async fn create_backup(&self) -> CreateResult {
        let filename = self.generate_backup_filename();
        let backup_path = path_join(&self.backup_dir, &filename);
        log::info!(target: LOG, "开始创建备份: {}", filename);

        let db_path = self.db_path.clone();
        let env_path = self.env_path.clone();
        let backup_dir = self.backup_dir.clone();
        let metadata = self.generate_backup_metadata();
        let level = config::get().backup_compression_level.clamp(0, 9) as u32;
        let bp = backup_path.clone();
        let partial_path = format!("{}.partial", backup_path);
        let partial_in_task = partial_path.clone();

        let r = tokio::task::spawn_blocking(move || -> Result<u64, String> {
            if !Path::new(&db_path).exists() {
                return Err(format!("数据库文件不存在: {}", db_path));
            }
            // 快照与元数据用唯一文件名；句柄后声明，清理最后执行
            let snapshot_path = path_join(&backup_dir, &format!("snapshot_{}.db", uuid::Uuid::new_v4()));
            let metadata_path = path_join(&backup_dir, &format!("metadata_{}.json", uuid::Uuid::new_v4()));
            let _cleanup = RemoveFiles {
                paths: vec![snapshot_path.clone(), metadata_path.clone(), partial_in_task.clone()],
            };

            if Path::new(&snapshot_path).exists() {
                std::fs::remove_file(&snapshot_path).map_err(|e| e.to_string())?;
            }
            // VACUUM INTO 读出已提交的一致快照，避免直接拷贝未 checkpoint 的 WAL
            let sql = format!("VACUUM INTO '{}'", snapshot_path.replace('\'', "''"));
            crate::db::with_conn(|conn| conn.execute_batch(&sql)).map_err(|e| e.to_string())?;

            let file = std::fs::File::create(&partial_in_task).map_err(|e| e.to_string())?;
            let enc = GzEncoder::new(file, Compression::new(level));
            let mut tar = tar::Builder::new(enc);
            tar.append_path_with_name(&snapshot_path, basename(&db_path)).map_err(|e| e.to_string())?;
            log::debug!(target: LOG, "已添加数据库文件: {}", db_path);
            if Path::new(&env_path).exists() {
                tar.append_path_with_name(&env_path, basename(&env_path)).map_err(|e| e.to_string())?;
                log::debug!(target: LOG, "已添加配置文件: {}", env_path);
            }
            std::fs::write(&metadata_path, metadata.as_bytes()).map_err(|e| e.to_string())?;
            tar.append_path_with_name(&metadata_path, "backup_metadata.json").map_err(|e| e.to_string())?;
            let enc = tar.into_inner().map_err(|e| e.to_string())?;
            enc.finish().map_err(|e| e.to_string())?;

            let size = std::fs::metadata(&partial_in_task).map_err(|e| e.to_string())?.len();
            if size < 1024 {
                return Err("备份文件大小异常，可能创建失败".to_string());
            }
            std::fs::rename(&partial_in_task, &bp).map_err(|e| e.to_string())?;
            Ok(size)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));

        match r {
            Ok(size) => {
                log::info!(target: LOG, "备份创建成功: {} ({} bytes)", filename, size);
                CreateResult { success: true, filename, backup_path, file_size: size, error: None }
            }
            Err(e) => {
                let error_msg = format!("创建备份失败: {}", e);
                log::error!(target: LOG, "{}", error_msg);
                if Path::new(&partial_path).exists() {
                    let _ = std::fs::remove_file(&partial_path);
                }
                CreateResult { success: false, filename, backup_path, file_size: 0, error: Some(error_msg) }
            }
        }
    }

    /// 上传备份到WebDAV
    async fn upload_backup(&self, backup_path: &str) -> bool {
        let filename = basename(backup_path);
        let webdav_path = format!("backups/{}", filename);
        log::info!(target: LOG, "开始上传备份到WebDAV: {}", webdav_path);
        self.webdav_client.create_directory("backups").await;
        let r = self.webdav_client.upload_file(backup_path, &webdav_path).await;
        if r.get("success") == Some(&Value::Bool(true)) {
            log::info!(target: LOG, "备份上传成功: {}", webdav_path);
            true
        } else {
            log::error!(
                target: LOG,
                "备份上传失败: {}",
                r.get("error").and_then(|v| v.as_str()).unwrap_or("None")
            );
            false
        }
    }

    fn parse_backup_time(name: &str) -> Result<NaiveDateTime, String> {
        let ts = name.replace("backup_", "").replace(".tar.gz", "");
        NaiveDateTime::parse_from_str(&ts, "%Y%m%d_%H%M%S")
            .map_err(|_| format!("time data '{}' does not match format '%Y%m%d_%H%M%S'", ts))
    }

    /// 清理过期备份 (本地 + WebDAV), 返回删除数量
    async fn cleanup_old_backups(&self) -> i64 {
        log::info!(target: LOG, "开始清理过期备份");
        let mut deleted = 0i64;
        let cutoff = beijing_now_naive() - ChronoDuration::days(config::get().backup_retention_days);

        // 本地
        if let Ok(rd) = std::fs::read_dir(&self.backup_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !(name.starts_with("backup_") && name.ends_with(".tar.gz")) {
                    continue;
                }
                match Self::parse_backup_time(&name) {
                    Ok(t) => {
                        if t < cutoff {
                            if std::fs::remove_file(entry.path()).is_ok() {
                                deleted += 1;
                                log::debug!(target: LOG, "删除本地过期备份: {}", name);
                            }
                        }
                    }
                    Err(e) => log::warn!(target: LOG, "处理本地备份文件失败 {}: {}", name, e),
                }
            }
        }

        // WebDAV
        match self.webdav_client.list_files("backups").await {
            Ok(files) => {
                for f in files {
                    let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !name.starts_with("backup_") || !name.ends_with(".tar.gz") {
                        continue;
                    }
                    match Self::parse_backup_time(&name) {
                        Ok(t) => {
                            if t < cutoff {
                                let p = format!("backups/{}", name);
                                if self.webdav_client.delete_file(&p).await {
                                    deleted += 1;
                                    log::debug!(target: LOG, "删除WebDAV过期备份: {}", p);
                                } else {
                                    log::warn!(target: LOG, "删除WebDAV备份失败: {}", p);
                                }
                            }
                        }
                        Err(e) => log::warn!(target: LOG, "处理WebDAV备份文件失败 {}: {}", name, e),
                    }
                }
            }
            Err(e) => log::error!(target: LOG, "清理WebDAV备份失败: {}", e),
        }

        log::info!(target: LOG, "备份清理完成，删除了 {} 个过期文件", deleted);
        deleted
    }

    /// 执行完整备份流程
    pub async fn perform_backup(&self) -> Map<String, Value> {
        let _g = BACKUP_LOCK.lock().await;
        self.perform_backup_inner().await
    }

    /// 备份主体。调用方已持有 BACKUP_LOCK，此处不再加锁
    async fn perform_backup_inner(&self) -> Map<String, Value> {
        log::info!(target: LOG, "开始执行定时备份任务");
        let started_at = beijing_now_iso();
        let mut result = Map::new();
        result.insert("success".into(), json!(false));
        result.insert("backup_filename".into(), Value::Null);
        result.insert("file_size".into(), json!(0));
        result.insert("uploaded".into(), json!(false));
        result.insert("cleaned_count".into(), json!(0));
        result.insert("error".into(), Value::Null);
        result.insert("started_at".into(), json!(started_at));

        let b = self.create_backup().await;
        if !b.success {
            result.insert("error".into(), json!(b.error));
            return result;
        }
        result.insert("backup_filename".into(), json!(b.filename));
        result.insert("file_size".into(), json!(b.file_size));

        let uploaded = self.upload_backup(&b.backup_path).await;
        result.insert("uploaded".into(), json!(uploaded));
        if !uploaded {
            log::warn!(target: LOG, "备份上传失败，但本地备份已保存");
        }

        let cleaned = self.cleanup_old_backups().await;
        result.insert("cleaned_count".into(), json!(cleaned));

        self.log_backup_result(&b.filename).await;

        if uploaded && Path::new(&b.backup_path).exists() {
            let _ = std::fs::remove_file(&b.backup_path);
            log::debug!(target: LOG, "已删除本地临时备份文件");
        }

        result.insert("success".into(), json!(true));
        log::info!(target: LOG, "定时备份任务执行成功");
        result.insert("completed_at".into(), json!(beijing_now_iso()));
        result
    }

    /// 记录备份结果到数据库 (保持原实现的失败表现, 见模块说明)
    async fn log_backup_result(&self, _filename: &str) {
        log::error!(target: LOG, "记录备份日志失败: {}", CONTEXT_MANAGER_ERROR);
    }

    /// 获取备份状态信息 (保持原实现的失败表现, 见模块说明)
    pub async fn get_backup_status(&self) -> Map<String, Value> {
        log::error!(target: LOG, "获取备份状态失败: {}", CONTEXT_MANAGER_ERROR);
        let mut m = Map::new();
        m.insert("error".into(), json!(CONTEXT_MANAGER_ERROR));
        m.insert("last_backup".into(), Value::Null);
        m.insert("backup_count".into(), json!(0));
        m.insert("total_size".into(), json!(0));
        m
    }

    #[allow(dead_code)]
    fn next_backup_time(&self) -> String {
        let now = beijing_now_naive();
        let mut next = now.date().and_hms_opt(0, 0, 0).unwrap();
        if now >= next {
            next += ChronoDuration::days(1);
        }
        isoformat(&next)
    }

    /// 手动触发备份。自行持锁后走主体，避免再调用 perform_backup 造成死锁
    pub async fn manual_backup(&self) -> Map<String, Value> {
        let _g = BACKUP_LOCK.lock().await;
        log::info!(target: LOG, "执行手动备份");
        self.perform_backup_inner().await
    }
}

/// 离开作用域时删除临时文件（成功、失败或 panic）
struct RemoveFiles {
    paths: Vec<String>,
}

impl Drop for RemoveFiles {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}
