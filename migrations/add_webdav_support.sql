-- 2025-10-27: 添加WebDAV支持
-- 执行顺序很重要，请按顺序执行

-- 1. 创建文件元数据表
CREATE TABLE IF NOT EXISTS file_metadata (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    filename TEXT NOT NULL,
    webdav_path TEXT NOT NULL,
    local_cache_path TEXT,
    upload_time DATETIME NOT NULL,
    file_size INTEGER NOT NULL,
    is_cached BOOLEAN DEFAULT 1,
    last_access_time DATETIME,
    webdav_etag TEXT,
    is_synced BOOLEAN DEFAULT 0,
    created_at DATETIME NOT NULL,
    updated_at DATETIME NOT NULL
);

-- 2. 创建索引
CREATE INDEX IF NOT EXISTS idx_file_metadata_upload_time ON file_metadata(upload_time);
CREATE INDEX IF NOT EXISTS idx_file_metadata_is_cached ON file_metadata(is_cached);
CREATE INDEX IF NOT EXISTS idx_file_metadata_is_synced ON file_metadata(is_synced);

-- 3. 扩展upload_history表
ALTER TABLE upload_history ADD COLUMN webdav_path TEXT;
ALTER TABLE upload_history ADD COLUMN is_cached BOOLEAN DEFAULT 1;
ALTER TABLE upload_history ADD COLUMN cache_expiry_time DATETIME;
ALTER TABLE upload_history ADD COLUMN backup_status TEXT DEFAULT 'pending';

-- 4. 创建备份日志表
CREATE TABLE IF NOT EXISTS backup_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    backup_filename TEXT NOT NULL,
    backup_time DATETIME NOT NULL,
    file_size INTEGER NOT NULL,
    status TEXT NOT NULL,
    error_message TEXT,
    created_at DATETIME NOT NULL
);

-- 5. 插入现有文件到file_metadata表（迁移现有数据）
INSERT INTO file_metadata (filename, webdav_path, local_cache_path, upload_time, file_size, is_synced, created_at, updated_at)
SELECT
    file_name,
    'files/' || strftime('%Y/%m/%d', upload_time) || '/' || file_name,
    local_file_path,
    upload_time,
    file_size,
    0, -- 标记为未同步到WebDAV
    upload_time,
    upload_time
FROM upload_history
WHERE status = 'success' AND local_file_path IS NOT NULL;

-- 6. 更新upload_history表的webdav_path
UPDATE upload_history
SET webdav_path = 'files/' || strftime('%Y/%m/%d', upload_time) || '/' || file_name
WHERE status = 'success';

-- 7. 创建迁移状态表（用于跟踪WebDAV迁移任务）
CREATE TABLE IF NOT EXISTS migration_status (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    migration_id TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'pending',
    total_files INTEGER NOT NULL DEFAULT 0,
    completed_files INTEGER NOT NULL DEFAULT 0,
    failed_files INTEGER NOT NULL DEFAULT 0,
    start_time DATETIME,
    end_time DATETIME,
    error_log TEXT,
    created_at DATETIME NOT NULL
);

-- 8. 创建索引
CREATE INDEX IF NOT EXISTS idx_migration_status_id ON migration_status(migration_id);
CREATE INDEX IF NOT EXISTS idx_backup_logs_time ON backup_logs(backup_time);
