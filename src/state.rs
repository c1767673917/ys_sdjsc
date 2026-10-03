//! 应用共享状态
//!
//! 原实现中各模块在导入时各自创建 FileManager / WebDAVClient / YonYouClient 实例,
//! 每个实例各自缓存 WebDAV 健康状态与用友 token; 这里保持相同的实例划分。

use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

use crate::backup::BackupService;
use crate::file_manager::FileManager;
use crate::webdav::WebDavClient;
use crate::yonyou::YonYouClient;

pub struct AppState {
    /// app/api/upload.py 的 file_manager
    pub upload_fm: Arc<FileManager>,
    /// app/api/upload.py 的 yonyou_client
    pub upload_yc: Arc<YonYouClient>,
    /// app/api/webdav.py 的 file_manager / webdav_client / backup_service
    pub webdav_fm: Arc<FileManager>,
    pub webdav_client: Arc<WebDavClient>,
    pub backup: Arc<BackupService>,
    /// app/api/migration.py 的 webdav_client 与内存中的迁移任务
    pub migration_client: Arc<WebDavClient>,
    pub migrations: Mutex<Map<String, Value>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        // 与原实现模块导入顺序一致: main -> upload -> admin -> migration -> webdav
        let _main_fm = FileManager::new();
        let upload_yc = Arc::new(YonYouClient::new());
        let upload_fm = Arc::new(FileManager::new());
        let migration_client = Arc::new(WebDavClient::new());
        let webdav_client = Arc::new(WebDavClient::new());
        let webdav_fm = Arc::new(FileManager::new());
        let backup = Arc::new(BackupService::new());
        Arc::new(AppState {
            upload_fm,
            upload_yc,
            webdav_fm,
            webdav_client,
            backup,
            migration_client,
            migrations: Mutex::new(Map::new()),
        })
    }
}
