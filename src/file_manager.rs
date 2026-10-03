//! 文件管理服务 (等价 app/core/file_manager.py)
//! 混合存储策略: WebDAV + 本地缓存, WebDAV 故障时降级到临时存储并登记待同步清单。

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::config;
use crate::pyutil::{beijing_from_timestamp, beijing_now_iso, beijing_now_naive, dirname, path_join};
use crate::webdav::WebDavClient;

const LOG: &str = "app.core.file_manager";

/// 待同步清单文件的进程内互斥 (多个 FileManager 实例共享同一个清单文件)
static PENDING_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

/// save_file 的返回结果 (对应原实现返回的 dict)
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct SaveResult {
    pub filename: String,
    pub webdav_path: Option<String>,
    pub file_size: Option<usize>,
    pub upload_time: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub local_cache_path: Option<String>,
    pub is_cached: Option<bool>,
    pub webdav_etag: Option<String>,
    pub is_synced: Option<bool>,
    pub storage_type: Option<String>,
}

impl SaveResult {
    pub fn is_cached_or_false(&self) -> bool {
        self.is_cached.unwrap_or(false)
    }
    pub fn is_synced_or_false(&self) -> bool {
        self.is_synced.unwrap_or(false)
    }
}

pub struct FileManager {
    pub webdav_client: WebDavClient,
    health: Mutex<(bool, Option<NaiveDateTime>)>,
    pending_sync_file: String,
}

impl FileManager {
    pub fn new() -> Self {
        let s = config::get();
        let fm = FileManager {
            webdav_client: WebDavClient::new(),
            health: Mutex::new((false, None)),
            pending_sync_file: path_join(&s.temp_storage_dir, "pending_sync.json"),
        };
        fm.ensure_directories();
        log::info!(target: LOG, "文件管理器初始化完成");
        log::info!(target: LOG, "缓存目录: {}", s.cache_dir);
        log::info!(target: LOG, "临时存储目录: {}", s.temp_storage_dir);
        log::info!(target: LOG, "缓存保留天数: {}", s.cache_days);
        fm
    }

    fn ensure_directories(&self) {
        let s = config::get();
        for d in [
            s.cache_dir.clone(),
            s.temp_storage_dir.clone(),
            path_join(&s.cache_dir, "files"),
            s.local_storage_path.clone(),
        ] {
            match std::fs::create_dir_all(&d) {
                Ok(_) => log::debug!(target: LOG, "确保目录存在: {}", d),
                Err(e) => {
                    log::error!(target: LOG, "创建目录失败 {}: {}", d, e);
                    panic!("创建目录失败 {}: {}", d, e);
                }
            }
        }
    }

    /// 获取本地缓存路径
    pub fn get_cache_path(&self, webdav_path: &str) -> String {
        let relative = match webdav_path.strip_prefix("files/") {
            Some(r) => r.to_string(),
            None => webdav_path.trim_start_matches('/').to_string(),
        };
        path_join(&config::get().cache_dir, &relative)
    }

    /// 获取临时存储路径: {时间戳}_{文件名}
    fn get_temp_path(&self, filename: &str) -> String {
        let ts = beijing_now_naive().format("%Y%m%d_%H%M%S");
        path_join(&config::get().temp_storage_dir, &format!("{}_{}", ts, filename))
    }

    /// 生成WebDAV文件路径: files/YYYY/MM/DD/filename
    pub fn generate_webdav_path(&self, filename: &str) -> String {
        format!("files/{}/{}", beijing_now_naive().format("%Y/%m/%d"), filename)
    }

    /// 检查缓存是否有效（CACHE_DAYS 天内）
    pub fn is_cache_valid(&self, cache_path: &str) -> bool {
        let p = Path::new(cache_path);
        if !p.exists() {
            return false;
        }
        match std::fs::metadata(p).and_then(|m| m.modified()) {
            Ok(mtime) => {
                let ts = mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let file_time = beijing_from_timestamp(ts);
                let expiry = file_time + ChronoDuration::days(config::get().cache_days);
                beijing_now_naive() < expiry
            }
            Err(e) => {
                log::error!(target: LOG, "检查缓存有效性失败 {}: {}", cache_path, e);
                false
            }
        }
    }

    fn load_pending_sync(&self) -> Value {
        load_pending_sync_file(&self.pending_sync_file)
    }

    fn save_pending_sync(&self, data: &Value) {
        let r = (|| -> Result<(), String> {
            let dir = dirname(&self.pending_sync_file);
            if !dir.is_empty() {
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            }
            let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
            atomic_write(&self.pending_sync_file, text.as_bytes()).map_err(|e| e.to_string())
        })();
        if let Err(e) = r {
            log::error!(target: LOG, "保存待同步清单失败: {}", e);
        }
    }

    /// 检查WebDAV健康状态 (HEALTH_CHECK_INTERVAL 秒内复用上次结果)
    pub async fn check_webdav_health(&self) -> bool {
        let now = beijing_now_naive();
        {
            let g = self.health.lock().unwrap();
            if let Some(last) = g.1 {
                let elapsed = (now - last).num_microseconds().unwrap_or(i64::MAX) as f64 / 1e6;
                if elapsed < config::get().health_check_interval as f64 {
                    return g.0;
                }
            }
        }
        let healthy = self.webdav_client.health_check().await;
        {
            let mut g = self.health.lock().unwrap();
            *g = (healthy, Some(now));
        }
        if healthy {
            log::debug!(target: LOG, "WebDAV服务可用");
        } else {
            log::warn!(target: LOG, "WebDAV服务不可用");
        }
        healthy
    }

    /// 保存文件（WebDAV + 本地缓存）
    pub async fn save_file(&self, file_content: &[u8], filename: &str) -> SaveResult {
        let webdav_path = self.generate_webdav_path(filename);
        let file_size = file_content.len();
        log::info!(target: LOG, "开始保存文件: {} ({} bytes)", filename, file_size);

        let webdav_available = self.check_webdav_health().await;

        let mut result = SaveResult {
            filename: filename.to_string(),
            webdav_path: Some(webdav_path.clone()),
            file_size: Some(file_size),
            upload_time: Some(beijing_now_iso()),
            success: false,
            error: None,
            ..Default::default()
        };

        if webdav_available {
            let temp_path = self.get_temp_path(filename);
            let inner: Result<(), String> = async {
                tokio::fs::write(&temp_path, file_content).await.map_err(|e| e.to_string())?;
                let upload_result = self.webdav_client.upload_file(&temp_path, &webdav_path).await;
                if upload_result.get("success") == Some(&Value::Bool(true)) {
                    let cache_path = self.get_cache_path(&webdav_path);
                    let cached = self.write_cache(&cache_path, file_content).await;
                    result.success = true;
                    if cached {
                        result.local_cache_path = Some(cache_path);
                        result.is_cached = Some(true);
                    } else {
                        // 缓存没落盘，不能把这条路径当成已缓存
                        result.local_cache_path = None;
                        result.is_cached = Some(false);
                    }
                    result.webdav_etag = upload_result
                        .get("etag")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    result.is_synced = Some(true);
                    log::info!(target: LOG, "文件保存成功: {}", webdav_path);
                } else {
                    let err = upload_result.get("error").and_then(|v| v.as_str()).unwrap_or("");
                    log::warn!(target: LOG, "WebDAV上传失败，降级到临时存储: {}", err);
                    self.save_to_temp_storage(file_content, filename, &webdav_path, &mut result).await;
                }
                // 上传用的临时文件无论成功与否都清理；删除失败只记日志，避免把已成功的结果降级
                if Path::new(&temp_path).exists() {
                    if let Err(e) = std::fs::remove_file(&temp_path) {
                        log::warn!(target: LOG, "删除临时文件失败 {}: {}", temp_path, e);
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = inner {
                log::error!(target: LOG, "文件保存过程中出错: {}", e);
                self.save_to_temp_storage(file_content, filename, &webdav_path, &mut result).await;
            }
        } else {
            log::warn!(target: LOG, "WebDAV不可用，文件降级到临时存储");
            self.save_to_temp_storage(file_content, filename, &webdav_path, &mut result).await;
        }
        result
    }

    /// 写入本地缓存 (失败不影响主流程)，返回是否写入成功
    async fn write_cache(&self, cache_path: &str, content: &[u8]) -> bool {
        let dir = dirname(cache_path);
        let path = cache_path.to_string();
        let data = content.to_vec();
        let r = async {
            if !dir.is_empty() {
                tokio::fs::create_dir_all(&dir).await?;
            }
            // sync_all 会等磁盘，放到阻塞线程，避免卡住运行时
            match tokio::task::spawn_blocking(move || atomic_write(&path, &data)).await {
                Ok(res) => res,
                Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
            }
        }
        .await;
        match r {
            Ok(_) => {
                log::debug!(target: LOG, "缓存文件写入成功: {}", cache_path);
                true
            }
            Err(e) => {
                log::error!(target: LOG, "写入缓存失败 {}: {}", cache_path, e);
                false
            }
        }
    }

    /// 保存到临时存储（降级处理）
    async fn save_to_temp_storage(
        &self,
        content: &[u8],
        filename: &str,
        webdav_path: &str,
        result: &mut SaveResult,
    ) {
        let temp_path = self.get_temp_path(filename);
        let r: Result<(), String> = async {
            tokio::fs::write(&temp_path, content).await.map_err(|e| e.to_string())?;
            let _g = PENDING_LOCK.lock().await;
            let mut pending = self.load_pending_sync();
            let entry = json!({
                "temp_path": temp_path,
                "filename": filename,
                "webdav_path": webdav_path,
                "created_time": beijing_now_iso(),
            });
            match pending.get_mut("files").and_then(|f| f.as_array_mut()) {
                Some(arr) => arr.push(entry),
                None => return Err("'files'".to_string()),
            }
            self.save_pending_sync(&pending);
            Ok(())
        }
        .await;

        match r {
            Ok(_) => {
                result.success = true; // 降级存储也认为是成功的
                result.local_cache_path = Some(temp_path.clone());
                result.is_cached = Some(false);
                result.is_synced = Some(false);
                result.storage_type = Some("temp".to_string());
                log::info!(target: LOG, "文件已降级到临时存储: {}", temp_path);
            }
            Err(e) => {
                log::error!(target: LOG, "临时存储保存失败: {}", e);
                result.error = Some(format!("临时存储保存失败: {}", e));
            }
        }
    }

    /// 获取文件内容（优先从缓存,支持WebDAV重试）
    pub async fn get_file(&self, webdav_path: &str) -> Result<Vec<u8>, String> {
        self.get_file_with_retries(webdav_path, 3).await
    }

    pub async fn get_file_with_retries(&self, webdav_path: &str, max_retries: u32) -> Result<Vec<u8>, String> {
        let r: Result<Vec<u8>, String> = async {
            log::debug!(target: LOG, "获取文件: {}", webdav_path);
            let cache_path = self.get_cache_path(webdav_path);
            if self.is_cache_valid(&cache_path) {
                log::debug!(target: LOG, "缓存命中: {}", cache_path);
                let content = tokio::fs::read(&cache_path).await.map_err(|e| e.to_string())?;
                // 更新访问时间
                touch(&cache_path).map_err(|e| e.to_string())?;
                return Ok(content);
            }

            log::debug!(target: LOG, "缓存未命中，从WebDAV下载: {}", webdav_path);
            let mut last_error = String::new();
            for attempt in 1..=max_retries {
                match self.webdav_client.download_file(webdav_path).await {
                    Ok(content) => {
                        self.write_cache(&cache_path, &content).await;
                        log::debug!(target: LOG, "文件已缓存: {}", cache_path);
                        if attempt > 1 {
                            log::info!(target: LOG, "WebDAV下载成功 (第{}次尝试): {}", attempt, webdav_path);
                        }
                        return Ok(content);
                    }
                    Err(e) => {
                        last_error = e.clone();
                        if attempt < max_retries {
                            log::warn!(
                                target: LOG,
                                "WebDAV下载失败 (第{}/{}次): {} - {}, 1秒后重试...",
                                attempt,
                                max_retries,
                                webdav_path,
                                e
                            );
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        } else {
                            log::error!(
                                target: LOG,
                                "WebDAV下载失败 (已重试{}次): {} - {}",
                                max_retries,
                                webdav_path,
                                e
                            );
                        }
                    }
                }
            }
            Err(format!("获取文件失败 (已重试{}次): {}", max_retries, last_error))
        }
        .await;

        r.map_err(|e| {
            let msg = format!("获取文件失败: {}", e);
            log::error!(target: LOG, "{}", msg);
            msg
        })
    }

    /// 清理过期缓存文件; 返回统计或 {"error": ...}
    pub async fn cleanup_cache(&self) -> Map<String, Value> {
        log::info!(target: LOG, "开始清理缓存文件");
        let s = config::get();
        let cache_dir = s.cache_dir.clone();
        let cache_days = s.cache_days;
        let res = tokio::task::spawn_blocking(move || {
            let mut stats = Map::new();
            stats.insert("total_files".into(), json!(0));
            stats.insert("deleted_files".into(), json!(0));
            stats.insert("freed_space".into(), json!(0));
            if !Path::new(&cache_dir).exists() {
                log::info!(target: LOG, "缓存目录不存在，跳过清理");
                return stats;
            }
            let expiry = beijing_now_naive() - ChronoDuration::days(cache_days);
            let (mut total, mut deleted, mut freed) = (0i64, 0i64, 0i64);
            for file in walk_files(Path::new(&cache_dir)) {
                total += 1;
                let r = (|| -> std::io::Result<()> {
                    let meta = std::fs::metadata(&file)?;
                    let mtime = meta.modified()?;
                    // 原实现用本机时区的 naive 时间与北京时间比较
                    let local: chrono::DateTime<chrono::Local> = mtime.into();
                    if local.naive_local() < expiry {
                        let size = meta.len() as i64;
                        std::fs::remove_file(&file)?;
                        deleted += 1;
                        freed += size;
                        log::debug!(target: LOG, "删除过期缓存文件: {}", file.display());
                    }
                    Ok(())
                })();
                if let Err(e) = r {
                    log::error!(target: LOG, "处理缓存文件失败 {}: {}", file.display(), e);
                }
            }
            stats.insert("total_files".into(), json!(total));
            stats.insert("deleted_files".into(), json!(deleted));
            stats.insert("freed_space".into(), json!(freed));
            log::info!(target: LOG, "缓存清理完成: 删除{}个文件，释放{}字节", deleted, freed);
            stats
        })
        .await;
        match res {
            Ok(m) => m,
            Err(e) => {
                log::error!(target: LOG, "缓存清理失败: {}", e);
                let mut m = Map::new();
                m.insert("error".into(), json!(e.to_string()));
                m
            }
        }
    }

    /// 同步待同步文件到WebDAV
    pub async fn sync_pending_files(&self) -> Map<String, Value> {
        log::info!(target: LOG, "开始同步待同步文件");
        let to_map = |v: Value| v.as_object().cloned().unwrap_or_default();

        if !self.check_webdav_health().await {
            return to_map(json!({
                "success": false,
                "error": "WebDAV服务不可用，跳过同步",
                "synced_count": 0,
                "failed_count": 0
            }));
        }

        let pending = {
            let _g = PENDING_LOCK.lock().await;
            self.load_pending_sync()
        };
        let files: Vec<Value> = pending
            .get("files")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();

        if files.is_empty() {
            return to_map(json!({
                "success": true,
                "message": "没有待同步文件",
                "synced_count": 0,
                "failed_count": 0
            }));
        }

        let mut synced = 0i64;
        let mut failed = 0i64;
        let mut errors: Vec<Value> = Vec::new();
        let mut remaining: Vec<Value> = Vec::new();

        for info in &files {
            let r: Result<(), String> = async {
                let temp_path = info.get("temp_path").and_then(|v| v.as_str()).ok_or("'temp_path'")?;
                let filename = info.get("filename").and_then(|v| v.as_str()).ok_or("'filename'")?;
                let webdav_path = info.get("webdav_path").and_then(|v| v.as_str()).ok_or("'webdav_path'")?;

                if !Path::new(temp_path).exists() {
                    log::warn!(target: LOG, "临时文件不存在，跳过: {}", temp_path);
                    return Ok(());
                }

                let upload_result = self.webdav_client.upload_file(temp_path, webdav_path).await;
                if upload_result.get("success") == Some(&Value::Bool(true)) {
                    std::fs::remove_file(temp_path).map_err(|e| e.to_string())?;
                    synced += 1;
                    log::info!(target: LOG, "文件同步成功: {}", webdav_path);
                } else {
                    let err = upload_result
                        .get("error")
                        .cloned()
                        .unwrap_or(Value::String("未知错误".into()));
                    remaining.push(info.clone());
                    failed += 1;
                    errors.push(json!({"filename": filename, "error": err.clone()}));
                    log::error!(
                        target: LOG,
                        "文件同步失败: {} - {}",
                        filename,
                        err.as_str().map(|s| s.to_string()).unwrap_or_else(|| err.to_string())
                    );
                }
                Ok(())
            }
            .await;
            if let Err(e) = r {
                remaining.push(info.clone());
                failed += 1;
                let filename = info.get("filename").cloned().unwrap_or(Value::String("unknown".into()));
                errors.push(json!({"filename": filename, "error": e}));
                log::error!(target: LOG, "同步文件异常: {}", e);
            }
        }

        // 更新待同步清单 (保留同步期间新登记的条目)
        {
            let _g = PENDING_LOCK.lock().await;
            let mut latest = self.load_pending_sync();
            let latest_files: Vec<Value> = latest
                .get("files")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            for f in latest_files {
                if !files.contains(&f) && !remaining.contains(&f) {
                    remaining.push(f);
                }
            }
            if let Some(obj) = latest.as_object_mut() {
                obj.insert("files".into(), Value::Array(remaining));
                obj.insert("last_sync".into(), Value::String(beijing_now_iso()));
            }
            self.save_pending_sync(&latest);
        }

        log::info!(target: LOG, "同步完成: 成功{}，失败{}", synced, failed);
        to_map(json!({
            "success": true,
            "synced_count": synced,
            "failed_count": failed,
            "errors": errors
        }))
    }

    /// 获取缓存统计信息
    pub async fn get_cache_stats(&self) -> Map<String, Value> {
        let cache_dir = crate::pyutil::path_str(&config::get().cache_dir);
        tokio::task::spawn_blocking(move || {
            let mut m = Map::new();
            if !Path::new(&cache_dir).exists() {
                m.insert("total_files".into(), json!(0));
                m.insert("total_size".into(), json!(0));
                m.insert("cache_dir".into(), json!(cache_dir));
                return m;
            }
            let mut total_files = 0i64;
            let mut total_size = 0i64;
            for f in walk_files(Path::new(&cache_dir)) {
                total_files += 1;
                total_size += std::fs::metadata(&f).map(|m| m.len() as i64).unwrap_or(0);
            }
            m.insert("total_files".into(), json!(total_files));
            m.insert("total_size".into(), json!(total_size));
            m.insert(
                "total_size_mb".into(),
                json!(crate::pyutil::py_round(total_size as f64 / 1024.0 / 1024.0, 2)),
            );
            m.insert("cache_dir".into(), json!(cache_dir));
            m
        })
        .await
        .unwrap_or_default()
    }

    /// 获取待同步文件数量
    pub async fn get_pending_sync_count(&self) -> usize {
        let _g = PENDING_LOCK.lock().await;
        self.load_pending_sync()
            .get("files")
            .and_then(|f| f.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    }
}

/// 读取待同步清单。文件不存在时返回空清单；存在但读不出或解析失败时改名保留再返回空清单。
fn load_pending_sync_file(path: &str) -> Value {
    if !Path::new(path).exists() {
        return json!({"files": [], "last_sync": null});
    }
    match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            log::error!(target: LOG, "加载待同步清单失败: {}", e);
            // 留下原文件，避免下一次保存把未解析的条目覆盖掉
            if Path::new(path).exists() {
                let dest = format!("{}.corrupt.{}", path, beijing_now_naive().format("%Y%m%d_%H%M%S%3f"));
                match std::fs::rename(path, &dest) {
                    Ok(()) => log::error!(target: LOG, "已保留损坏的待同步清单: {}", dest),
                    Err(re) => log::error!(target: LOG, "保留损坏的待同步清单失败 {}: {}", dest, re),
                }
            }
            json!({"files": [], "last_sync": null})
        }
    }
}

/// 同目录写临时文件，flush/sync_all 后 rename 覆盖。失败时删除临时文件。
fn atomic_write(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{}.tmp.{}", path, uuid::Uuid::new_v4());
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// os.utime(path): 将访问/修改时间更新为当前时间
fn touch(path: &str) -> std::io::Result<()> {
    let now = SystemTime::now();
    let f = std::fs::File::options().write(true).open(path)?;
    f.set_times(std::fs::FileTimes::new().set_accessed(now).set_modified(now))
}

/// 递归列出目录下所有文件 (Path.rglob('*') 中 is_file 的部分)
pub fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_target_and_drops_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending_sync.json");
        let path_str = path.to_str().unwrap();
        std::fs::write(path_str, b"old-partial").unwrap();

        let data = br#"{"files":[],"last_sync":null}"#;
        atomic_write(path_str, data).unwrap();
        assert_eq!(std::fs::read(path_str).unwrap(), data);
        assert!(tmp_names(dir.path()).is_empty());

        // 目标是目录时 rename 失败，临时文件应被删掉
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let err = atomic_write(blocked.to_str().unwrap(), b"data").unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(tmp_names(dir.path()).is_empty());
        assert!(blocked.is_dir());
    }

    #[test]
    fn corrupt_pending_sync_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending_sync.json");
        let path_str = path.to_str().unwrap();
        std::fs::write(path_str, b"{not json").unwrap();

        let v = load_pending_sync_file(path_str);
        assert_eq!(v, json!({"files": [], "last_sync": null}));
        assert!(!path.exists());

        let preserved: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(preserved.len(), 1);
        let name = preserved[0].file_name().to_string_lossy().into_owned();
        assert!(name.starts_with("pending_sync.json.corrupt."), "{}", name);
        assert_eq!(std::fs::read(preserved[0].path()).unwrap(), b"{not json");

        // 文件不存在时不产生 .corrupt 备份
        let missing = dir.path().join("missing.json");
        let missing_str = missing.to_str().unwrap();
        assert_eq!(load_pending_sync_file(missing_str), json!({"files": [], "last_sync": null}));
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().flatten().count(), 1);
    }

    fn tmp_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect()
    }
}
