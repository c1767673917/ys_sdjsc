//! 日志配置 (等价 app/core/logging_config.py)
//!
//! - 控制台: 彩色格式 `[时间] LEVEL    name:lineno - message`
//! - 文件: logs/app.log, 格式 `asctime - name - levelname - message`, 10MB × 5 轮转
//! - 两路输出均经过敏感信息过滤

use log::{Level, LevelFilter, Log, Metadata, Record};
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::pyutil::{beijing_now_aware_iso, beijing_now_naive};

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const BACKUP_COUNT: usize = 5;

static SENSITIVE_PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    let pats: Vec<(&str, &str)> = vec![
        // 密码相关
        (r#"password["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"password":"***""#),
        (r#"pwd["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"pwd":"***""#),
        (r#"pass["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"pass":"***""#),
        // Token和密钥
        (r#"token["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"token":"***""#),
        (r#"key["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"key":"***""#),
        (r#"secret["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"secret":"***""#),
        (r#"authorization["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"authorization":"***""#),
        // 基础认证
        (r"basic\s+([a-zA-Z0-9+/=]+)", "basic ***"),
        (r"Bearer\s+([a-zA-Z0-9\-._~+/]+=*)", "Bearer ***"),
        // WebDAV相关
        (r#"webdav_password["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"webdav_password":"***""#),
        (r#"webdav_username["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"webdav_username":"***""#),
        // 用友云相关
        (r#"app_secret["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"app_secret":"***""#),
        (r#"app_key["\s]*[:=]["\s]*([^"\s,}]+)"#, r#"app_key":"***""#),
        // URL中的敏感参数
        (r"([&?])password=([^&]*)", "${1}password=***"),
        (r"([&?])token=([^&]*)", "${1}token=***"),
        (r"([&?])key=([^&]*)", "${1}key=***"),
    ];
    pats.into_iter()
        .map(|(p, r)| (Regex::new(&format!("(?i){}", p)).unwrap(), r))
        .collect()
});

pub fn filter_sensitive(text: &str) -> String {
    let mut out = text.to_string();
    for (re, rep) in SENSITIVE_PATTERNS.iter() {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARNING",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "DEBUG",
    }
}

fn level_color(level: Level) -> &'static str {
    match level {
        Level::Error => "\x1b[31m",
        Level::Warn => "\x1b[33m",
        Level::Info => "\x1b[32m",
        Level::Debug | Level::Trace => "\x1b[36m",
    }
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    fn new(path: PathBuf) -> Self {
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        RotatingFile { path, file, size }
    }

    fn rollover(&mut self) {
        self.file = None;
        let p = self.path.to_string_lossy().to_string();
        for i in (1..BACKUP_COUNT).rev() {
            let src = format!("{}.{}", p, i);
            let dst = format!("{}.{}", p, i + 1);
            if std::path::Path::new(&src).exists() {
                let _ = std::fs::remove_file(&dst);
                let _ = std::fs::rename(&src, &dst);
            }
        }
        let dst = format!("{}.1", p);
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::rename(&self.path, &dst);
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.size = 0;
    }

    fn write_line(&mut self, line: &str) {
        let bytes = line.as_bytes();
        if self.size > 0 && self.size + bytes.len() as u64 >= MAX_BYTES {
            self.rollover();
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(bytes).is_ok() {
                self.size += bytes.len() as u64;
            }
        }
    }
}

struct AppLogger {
    level: LevelFilter,
    console: bool,
    file: Option<Mutex<RotatingFile>>,
    filter_sensitive: bool,
}

impl Log for AppLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let raw = record.args().to_string();
        let msg = if self.filter_sensitive { filter_sensitive(&raw) } else { raw };
        let name = record.target();
        let lvl = record.level();

        if self.console {
            let ts = beijing_now_naive().format("%Y-%m-%d %H:%M:%S");
            let line = format!(
                "{}[{}] {:<8} {}:{} - {}\x1b[0m\n",
                level_color(lvl),
                ts,
                level_name(lvl),
                name,
                record.line().unwrap_or(0),
                msg
            );
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        if let Some(f) = &self.file {
            let line = format!(
                "{} - {} - {} - {}\n",
                beijing_now_aware_iso(),
                name,
                level_name(lvl),
                msg
            );
            if let Ok(mut g) = f.lock() {
                g.write_line(&line);
            }
        }
    }

    fn flush(&self) {}
}

/// setup_logging(level, enable_console, enable_file, log_file, structured=False, filter_sensitive)
pub fn setup_logging(level: &str, enable_console: bool, enable_file: bool, log_file: &str, filter_sensitive: bool) {
    let filter = match level.to_uppercase().as_str() {
        "DEBUG" => LevelFilter::Debug,
        "WARNING" => LevelFilter::Warn,
        "ERROR" | "CRITICAL" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };
    let file = if enable_file {
        let p = PathBuf::from(log_file);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Some(Mutex::new(RotatingFile::new(p)))
    } else {
        None
    };
    let logger = AppLogger { level: filter, console: enable_console, file, filter_sensitive };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(filter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_tokens() {
        // 期望值取自原 Python SensitiveDataFilter 的实际输出
        assert_eq!(
            filter_sensitive("GET https://x/a?access_token=abc&id=1"),
            "GET https://x/a?access_token\":\"***\""
        );
        assert_eq!(filter_sensitive("Authorization: Basic YWJj"), "authorization\":\"***\" YWJj");
        assert_eq!(
            filter_sensitive("HTTP Request: POST https://c4/x?access_token=a%2Fb&businessType=y \"HTTP/1.1 200 OK\""),
            "HTTP Request: POST https://c4/x?access_token\":\"***\" \"HTTP/1.1 200 OK\""
        );
    }
}
