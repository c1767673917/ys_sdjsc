//! 应用配置 (等价 app/core/config.py 的 Settings)
//!
//! 读取顺序与 pydantic-settings 一致: 进程环境变量优先, 其次 .env 文件, 最后默认值。
//! 变量名大小写敏感, 未定义的额外变量忽略。

use once_cell::sync::OnceCell;
use regex::Regex;
use std::collections::HashMap;

use crate::pyutil::py_strip;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Settings {
    // 应用配置
    pub app_name: String,
    pub app_version: String,
    pub host: String,
    pub port: u16,
    pub debug: bool,

    // 管理后台鉴权
    pub admin_token: String,
    pub admin_cookie_secure: bool,

    // 用友云配置
    pub yonyou_app_key: String,
    pub yonyou_app_secret: String,
    pub yonyou_business_type: String,
    pub yonyou_auth_url: String,
    pub yonyou_upload_url: String,

    // 上传配置
    pub max_file_size: i64,
    pub max_files_per_request: i64,
    pub allowed_extensions: Vec<String>,

    // 重试配置
    pub max_retry_count: i64,
    pub retry_delay: i64,
    pub request_timeout: i64,

    // 用友云上传失败自动重试
    pub yonyou_retry_enabled: bool,
    pub yonyou_retry_interval_hours: i64,
    pub yonyou_retry_lookback_hours: i64,
    pub yonyou_retry_max_records: i64,

    // 并发控制
    pub max_concurrent_uploads: i64,

    // 发货单快照同步
    pub delivery_sync_enabled: bool,
    pub delivery_sync_interval_minutes: i64,
    pub delivery_sync_lookback_days: i64,
    pub delivery_sync_page_size: i64,
    pub delivery_sync_max_pages: i64,
    pub delivery_sync_min_freight: f64,
    pub delivery_sync_page_interval_seconds: f64,
    pub delivery_sync_manual_cooldown_seconds: i64,

    // 数据库
    pub database_url: String,

    // 本地文件存储
    pub local_storage_path: String,

    // WebDAV
    pub webdav_url: String,
    pub webdav_username: String,
    pub webdav_password: String,
    pub webdav_base_path: String,
    pub webdav_timeout: i64,
    pub webdav_retry_count: i64,
    pub webdav_retry_delay: i64,

    // 缓存
    pub cache_dir: String,
    pub cache_days: i64,
    pub temp_storage_dir: String,

    // 备份
    pub backup_retention_days: i64,
    pub backup_compression_level: i64,
    pub backup_enabled: bool,

    // 健康检查
    pub health_check_interval: i64,
    pub sync_retry_interval: i64,

    // Token缓存
    pub token_cache_duration: i64,

    // 调试
    pub webdav_debug: bool,
    pub backup_debug: bool,
}

struct Source {
    vars: HashMap<String, String>,
}

impl Source {
    fn load() -> Self {
        let mut vars: HashMap<String, String> = HashMap::new();
        // .env 文件 (优先级低于进程环境变量)
        if let Ok(iter) = dotenvy::from_filename_iter(".env") {
            for (k, v) in iter.flatten() {
                vars.insert(k, v);
            }
        }
        // 用 vars_os 并跳过非 UTF-8 项: std::env::vars() 遇到非 UTF-8 值会直接 panic
        // (例如 zsh 在中文路径下导出的 `_` 变量是 metafied 字节)
        for (k, v) in std::env::vars_os() {
            if let (Ok(k), Ok(v)) = (k.into_string(), v.into_string()) {
                vars.insert(k, v);
            }
        }
        Source { vars }
    }

    fn raw(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    fn string(&self, key: &str, default: &str) -> String {
        self.raw(key).cloned().unwrap_or_else(|| default.to_string())
    }

    fn opt_string(&self, key: &str) -> Option<String> {
        self.raw(key).cloned()
    }

    fn int(&self, key: &str, default: i64) -> Result<i64, String> {
        match self.raw(key) {
            None => Ok(default),
            Some(v) => parse_int(v).ok_or_else(|| {
                validation_error(
                    key,
                    "Input should be a valid integer, unable to parse string as an integer",
                    "int_parsing",
                    v,
                )
            }),
        }
    }

    fn float(&self, key: &str, default: f64) -> Result<f64, String> {
        match self.raw(key) {
            None => Ok(default),
            Some(v) => crate::pyutil::py_parse_float(v).ok_or_else(|| {
                validation_error(
                    key,
                    "Input should be a valid number, unable to parse string as a number",
                    "float_parsing",
                    v,
                )
            }),
        }
    }

    fn bool(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.raw(key) {
            None => Ok(default),
            Some(v) => parse_bool(v).ok_or_else(|| {
                validation_error(
                    key,
                    "Input should be a valid boolean, unable to interpret input",
                    "bool_parsing",
                    v,
                )
            }),
        }
    }
}

fn validation_error(key: &str, msg: &str, kind: &str, value: &str) -> String {
    format!(
        "1 validation error for Settings\n{}\n  {} [type={}, input_value='{}', input_type=str]",
        key, msg, kind, value
    )
}

/// pydantic 宽松模式的 str -> int: 允许首尾空白、小数部分为0的写法(如 "30.0")
pub fn parse_int(v: &str) -> Option<i64> {
    let t = py_strip(v);
    if let Ok(i) = t.replace('_', "").parse::<i64>() {
        if !t.starts_with('_') && !t.ends_with('_') && !t.contains("__") {
            return Some(i);
        }
    }
    if let Some((int_part, frac)) = t.split_once('.') {
        if !frac.is_empty() && frac.chars().all(|c| c == '0') {
            if let Ok(i) = int_part.parse::<i64>() {
                return Some(i);
            }
        }
    }
    None
}

/// pydantic 宽松模式的 str -> bool
pub fn parse_bool(v: &str) -> Option<bool> {
    match py_strip(v).to_lowercase().as_str() {
        "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
        "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
        _ => None,
    }
}

static SETTINGS: OnceCell<Settings> = OnceCell::new();

/// get_settings(): 进程内单例
pub fn get() -> &'static Settings {
    SETTINGS.get().expect("settings not initialized")
}

pub fn init() -> Result<&'static Settings, String> {
    let s = Settings::load()?;
    Ok(SETTINGS.get_or_init(|| s))
}

impl Settings {
    pub fn load() -> Result<Settings, String> {
        let src = Source::load();

        let allowed_extensions = match src.raw("ALLOWED_EXTENSIONS") {
            None => vec![".jpg".into(), ".jpeg".into(), ".png".into(), ".gif".into()],
            Some(v) => {
                let parsed: Result<Vec<String>, _> = serde_json::from_str(v);
                match parsed {
                    Ok(list) => {
                        let mut out: Vec<String> = Vec::new();
                        for e in list {
                            if !out.contains(&e) {
                                out.push(e);
                            }
                        }
                        out
                    }
                    Err(_) => {
                        return Err(format!(
                            "error parsing value for field \"ALLOWED_EXTENSIONS\" from source \"EnvSettingsSource\""
                        ))
                    }
                }
            }
        };

        let port = src.int("PORT", 10000)?;
        let mut s = Settings {
            app_name: src.string("APP_NAME", "单据上传管理系统"),
            app_version: src.string("APP_VERSION", "1.0.0"),
            host: src.string("HOST", "0.0.0.0"),
            port: u16::try_from(port).map_err(|_| format!("PORT无效: {}", port))?,
            debug: src.bool("DEBUG", false)?,

            admin_token: src.opt_string("ADMIN_TOKEN").unwrap_or_default(),
            admin_cookie_secure: src.bool("ADMIN_COOKIE_SECURE", false)?,

            yonyou_app_key: src.opt_string("YONYOU_APP_KEY").unwrap_or_default(),
            yonyou_app_secret: src.opt_string("YONYOU_APP_SECRET").unwrap_or_default(),
            yonyou_business_type: src.string("YONYOU_BUSINESS_TYPE", "yonbip-scm-scmsa"),
            yonyou_auth_url: src.string(
                "YONYOU_AUTH_URL",
                "https://c4.yonyoucloud.com/iuap-api-auth/open-auth/selfAppAuth/base/v1/getAccessToken",
            ),
            yonyou_upload_url: src.string(
                "YONYOU_UPLOAD_URL",
                "https://c4.yonyoucloud.com/iuap-api-gateway/yonbip/uspace/iuap-apcom-file/rest/v1/file",
            ),

            max_file_size: src.int("MAX_FILE_SIZE", 10 * 1024 * 1024)?,
            max_files_per_request: src.int("MAX_FILES_PER_REQUEST", 10)?,
            allowed_extensions,

            max_retry_count: src.int("MAX_RETRY_COUNT", 3)?,
            retry_delay: src.int("RETRY_DELAY", 2)?,
            request_timeout: src.int("REQUEST_TIMEOUT", 30)?,

            yonyou_retry_enabled: src.bool("YONYOU_RETRY_ENABLED", true)?,
            yonyou_retry_interval_hours: src.int("YONYOU_RETRY_INTERVAL_HOURS", 1)?,
            yonyou_retry_lookback_hours: src.int("YONYOU_RETRY_LOOKBACK_HOURS", 24)?,
            yonyou_retry_max_records: src.int("YONYOU_RETRY_MAX_RECORDS", 50)?,

            max_concurrent_uploads: src.int("MAX_CONCURRENT_UPLOADS", 3)?,

            delivery_sync_enabled: src.bool("DELIVERY_SYNC_ENABLED", true)?,
            delivery_sync_interval_minutes: src.int("DELIVERY_SYNC_INTERVAL_MINUTES", 30)?,
            delivery_sync_lookback_days: src.int("DELIVERY_SYNC_LOOKBACK_DAYS", 60)?,
            delivery_sync_page_size: src.int("DELIVERY_SYNC_PAGE_SIZE", 200)?,
            delivery_sync_max_pages: src.int("DELIVERY_SYNC_MAX_PAGES", 50)?,
            delivery_sync_min_freight: src.float("DELIVERY_SYNC_MIN_FREIGHT", 100.0)?,
            delivery_sync_page_interval_seconds: src.float("DELIVERY_SYNC_PAGE_INTERVAL_SECONDS", 1.6)?,
            delivery_sync_manual_cooldown_seconds: src.int("DELIVERY_SYNC_MANUAL_COOLDOWN_SECONDS", 300)?,

            database_url: src.string("DATABASE_URL", "sqlite:///data/uploads.db"),
            local_storage_path: src.string("LOCAL_STORAGE_PATH", "data/uploaded_files"),

            webdav_url: src.string("WEBDAV_URL", "http://localhost:10100/dav/"),
            webdav_username: src.string("WEBDAV_USERNAME", "admin"),
            webdav_password: src.string("WEBDAV_PASSWORD", "adminlcs"),
            webdav_base_path: src.string("WEBDAV_BASE_PATH", "onedrive_lcs"),
            webdav_timeout: src.int("WEBDAV_TIMEOUT", 30)?,
            webdav_retry_count: src.int("WEBDAV_RETRY_COUNT", 3)?,
            webdav_retry_delay: src.int("WEBDAV_RETRY_DELAY", 5)?,

            cache_dir: src.string("CACHE_DIR", "./cache"),
            cache_days: src.int("CACHE_DAYS", 7)?,
            temp_storage_dir: src.string("TEMP_STORAGE_DIR", "./temp_storage"),

            backup_retention_days: src.int("BACKUP_RETENTION_DAYS", 30)?,
            backup_compression_level: src.int("BACKUP_COMPRESSION_LEVEL", 6)?,
            backup_enabled: src.bool("BACKUP_ENABLED", true)?,

            health_check_interval: src.int("HEALTH_CHECK_INTERVAL", 60)?,
            sync_retry_interval: src.int("SYNC_RETRY_INTERVAL", 300)?,

            token_cache_duration: src.int("TOKEN_CACHE_DURATION", 3600)?,

            webdav_debug: src.bool("WEBDAV_DEBUG", false)?,
            backup_debug: src.bool("BACKUP_DEBUG", false)?,
        };

        s.validate()?;
        Ok(s)
    }

    fn validate(&mut self) -> Result<(), String> {
        // 验证WebDAV配置
        self.webdav_url = validate_webdav_url(&self.webdav_url)?;
        if self.webdav_username.is_empty() {
            return Err("WebDAV用户名不能为空".into());
        }
        if self.webdav_password.is_empty() {
            return Err("WebDAV密码不能为空".into());
        }
        if self.webdav_username.chars().count() > 100 {
            return Err("WebDAV用户名长度不能超过100个字符".into());
        }
        if self.webdav_password.chars().count() > 200 {
            return Err("WebDAV密码长度不能超过200个字符".into());
        }
        self.webdav_base_path = validate_webdav_base_path(&self.webdav_base_path)?;

        if self.webdav_timeout <= 0 {
            return Err("WebDAV超时时间必须大于0".into());
        }
        if self.webdav_timeout > 300 {
            return Err("WebDAV超时时间不能超过300秒".into());
        }
        if self.webdav_retry_count < 0 {
            return Err("WebDAV重试次数不能为负数".into());
        }
        if self.webdav_retry_count > 10 {
            return Err("WebDAV重试次数不能超过10次".into());
        }
        if self.webdav_retry_delay < 0 {
            return Err("WebDAV重试延迟不能为负数".into());
        }
        if self.webdav_retry_delay > 60 {
            return Err("WebDAV重试延迟不能超过60秒".into());
        }

        // 用友云上传失败重试配置
        if self.yonyou_retry_interval_hours <= 0 {
            return Err("YONYOU_RETRY_INTERVAL_HOURS必须大于0".into());
        }
        if self.yonyou_retry_lookback_hours <= 0 {
            return Err("YONYOU_RETRY_LOOKBACK_HOURS必须大于0".into());
        }
        if self.yonyou_retry_max_records <= 0 {
            return Err("YONYOU_RETRY_MAX_RECORDS必须大于0".into());
        }
        if self.yonyou_retry_max_records > 500 {
            return Err("YONYOU_RETRY_MAX_RECORDS不能超过500".into());
        }

        // 发货单快照同步配置
        if self.delivery_sync_interval_minutes <= 0 {
            return Err("DELIVERY_SYNC_INTERVAL_MINUTES必须大于0".into());
        }
        if self.delivery_sync_lookback_days <= 0 {
            return Err("DELIVERY_SYNC_LOOKBACK_DAYS必须大于0".into());
        }
        if !(1..=500).contains(&self.delivery_sync_page_size) {
            return Err("DELIVERY_SYNC_PAGE_SIZE必须在1-500之间".into());
        }
        if self.delivery_sync_max_pages <= 0 {
            return Err("DELIVERY_SYNC_MAX_PAGES必须大于0".into());
        }
        if self.delivery_sync_page_interval_seconds < 0.0 {
            return Err("DELIVERY_SYNC_PAGE_INTERVAL_SECONDS不能为负数".into());
        }
        if self.delivery_sync_manual_cooldown_seconds < 0 {
            return Err("DELIVERY_SYNC_MANUAL_COOLDOWN_SECONDS不能为负数".into());
        }

        // 必需的用友云配置
        if self.yonyou_app_key.is_empty() {
            return Err(
                "缺少必需的环境变量: YONYOU_APP_KEY\n请在 .env 文件中设置: YONYOU_APP_KEY=your_app_key".into(),
            );
        }
        if self.yonyou_app_secret.is_empty() {
            return Err(
                "缺少必需的环境变量: YONYOU_APP_SECRET\n请在 .env 文件中设置: YONYOU_APP_SECRET=your_app_secret"
                    .into(),
            );
        }

        // 管理后台token
        if self.admin_token.is_empty() {
            return Err("缺少必需的环境变量: ADMIN_TOKEN\n请在 .env 文件中设置随机token(管理页访问路径), 生成方式:\n  python -c \"import secrets; print(secrets.token_urlsafe(16))\"".into());
        }
        let weak = [
            "change-me-to-random-token",
            "changeme",
            "admin",
            "password",
            "secret",
            "your-token-here",
        ];
        if weak.contains(&self.admin_token.to_lowercase().as_str()) {
            return Err("ADMIN_TOKEN不能使用示例/占位弱值, 请生成随机token:\n  python -c \"import secrets; print(secrets.token_urlsafe(16))\"".into());
        }
        let re = Regex::new(r"^[A-Za-z0-9_-]{12,}$").unwrap();
        if !re.is_match(&self.admin_token) {
            return Err("ADMIN_TOKEN格式无效: 至少12位, 仅允许字母/数字/下划线/连字符".into());
        }
        Ok(())
    }

    /// validate_webdav_health(): 返回 (valid, errors, warnings)
    pub fn validate_webdav_health(&self) -> (bool, Vec<String>, Vec<String>) {
        let mut valid = true;
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        if self.webdav_url.is_empty() {
            errors.push("WebDAV URL未配置".to_string());
            valid = false;
        }
        if self.webdav_username.is_empty() {
            errors.push("WebDAV用户名未配置".to_string());
            valid = false;
        }
        if self.webdav_password.is_empty() {
            errors.push("WebDAV密码未配置".to_string());
            valid = false;
        }
        if !self.webdav_base_path.is_empty() && self.webdav_base_path.chars().count() > 200 {
            warnings.push("WebDAV基础路径较长，可能影响性能".to_string());
        }
        if self.webdav_debug {
            warnings.push("WebDAV调试模式已启用，生产环境建议关闭".to_string());
        }
        (valid, errors, warnings)
    }

    /// DATABASE_URL 去掉 sqlite:/// 前缀后的文件路径
    pub fn db_path(&self) -> String {
        self.database_url.replace("sqlite:///", "")
    }
}

fn validate_webdav_url(url: &str) -> Result<String, String> {
    if url.is_empty() {
        return Err("WebDAV URL不能为空".into());
    }
    let wrap = |e: &str| format!("WebDAV URL格式无效: {}", e);
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => ("".to_string(), ""),
    };
    if scheme != "http" && scheme != "https" {
        return Err(wrap("WebDAV URL必须使用http或https协议"));
    }
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    if netloc.is_empty() {
        return Err(wrap("WebDAV URL格式无效，缺少主机名"));
    }
    Ok(format!("{}/", url.trim_end_matches('/')))
}

fn validate_webdav_base_path(base_path: &str) -> Result<String, String> {
    if base_path.is_empty() {
        return Ok(String::new());
    }
    let clean = base_path.trim_matches('/');
    for ch in ['<', '>', ':', '"', '|', '?', '*'] {
        if clean.contains(ch) {
            return Err(format!("WebDAV基础路径包含无效字符: {}", ch));
        }
    }
    Ok(clean.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_bool_parsing() {
        assert_eq!(parse_int("30.0"), Some(30));
        assert_eq!(parse_int(" 7 "), Some(7));
        assert_eq!(parse_int("30.5"), None);
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("2"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn webdav_url() {
        assert_eq!(validate_webdav_url("http://h:1/dav").unwrap(), "http://h:1/dav/");
        assert!(validate_webdav_url("ftp://h").is_err());
        assert_eq!(validate_webdav_base_path("/a/b/").unwrap(), "a/b");
    }
}
