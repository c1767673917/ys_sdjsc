//! 定时任务调度器 (等价 app/scheduler.py, APScheduler: coalesce=True, max_instances=1, 时区 Asia/Shanghai)

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::backup::BackupService;
use crate::config;
use crate::db;
use crate::delivery_sync;
use crate::file_manager::FileManager;
use crate::pyutil::beijing_now_naive;
use crate::retry_service;
use crate::webdav::WebDavClient;
use crate::yonyou::YonYouClient;

const LOG: &str = "app.scheduler";
const APS_LOG: &str = "apscheduler.executors.default";

/// 调度器持有的单例 (get_file_manager / get_backup_service)
pub struct Services {
    pub file_manager: Arc<FileManager>,
    backup: tokio::sync::OnceCell<Arc<BackupService>>,
}

impl Services {
    async fn backup(&self) -> Arc<BackupService> {
        self.backup.get_or_init(|| async { Arc::new(BackupService::new()) }).await.clone()
    }
}

// ========== 任务函数 ==========

async fn webdav_health_check_task(svc: Arc<Services>) {
    log::debug!(target: LOG, "执行WebDAV健康检查任务");
    if svc.file_manager.check_webdav_health().await {
        log::debug!(target: LOG, "WebDAV健康检查通过");
    } else {
        log::warn!(target: LOG, "WebDAV健康检查失败，服务不可用");
    }
}

async fn cleanup_cache_task(svc: Arc<Services>) {
    log::info!(target: LOG, "执行缓存清理任务");
    let r = svc.file_manager.cleanup_cache().await;
    if let Some(e) = r.get("error") {
        log::error!(target: LOG, "缓存清理失败: {}", e.as_str().unwrap_or(""));
    } else {
        let deleted = r.get("deleted_files").and_then(|v| v.as_i64()).unwrap_or(0);
        let freed = r.get("freed_space").and_then(|v| v.as_f64()).unwrap_or(0.0);
        log::info!(
            target: LOG,
            "缓存清理完成: 删除{}个文件，释放{:.2}MB空间",
            deleted,
            freed / 1024.0 / 1024.0
        );
    }
}

async fn database_backup_task(svc: Arc<Services>) {
    log::info!(target: LOG, "执行数据库备份任务");
    if !config::get().backup_enabled {
        log::info!(target: LOG, "备份功能已禁用，跳过备份任务");
        return;
    }
    let r = svc.backup().await.perform_backup().await;
    if r.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        log::info!(
            target: LOG,
            "备份任务完成: {} ({} bytes), 上传{}, 清理{}个过期文件",
            r.get("backup_filename").and_then(|v| v.as_str()).unwrap_or("unknown"),
            r.get("file_size").and_then(|v| v.as_i64()).unwrap_or(0),
            if r.get("uploaded").and_then(|v| v.as_bool()).unwrap_or(false) { "成功" } else { "失败" },
            r.get("cleaned_count").and_then(|v| v.as_i64()).unwrap_or(0)
        );
    } else {
        log::error!(
            target: LOG,
            "备份任务失败: {}",
            r.get("error").and_then(|v| v.as_str()).unwrap_or("未知错误")
        );
    }
}

async fn sync_pending_files_task(svc: Arc<Services>) {
    log::debug!(target: LOG, "执行待同步文件检查任务");
    let pending = svc.file_manager.get_pending_sync_count().await;
    if pending > 0 {
        log::info!(target: LOG, "发现{}个待同步文件，开始同步", pending);
        let r = svc.file_manager.sync_pending_files().await;
        if r.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
            log::info!(
                target: LOG,
                "同步完成: 成功{}个，失败{}个",
                r.get("synced_count").and_then(|v| v.as_i64()).unwrap_or(0),
                r.get("failed_count").and_then(|v| v.as_i64()).unwrap_or(0)
            );
        } else {
            log::warn!(
                target: LOG,
                "同步失败: {}",
                r.get("error").and_then(|v| v.as_str()).unwrap_or("未知错误")
            );
        }
    } else {
        log::debug!(target: LOG, "没有待同步文件");
    }
}

async fn retry_yonyou_uploads_task(svc: Arc<Services>) {
    if !config::get().yonyou_retry_enabled {
        log::debug!(target: LOG, "用友云上传失败重试任务已禁用，跳过");
        return;
    }
    let yc = YonYouClient::new();
    let r = retry_service::retry_failed_yonyou_uploads(&svc.file_manager, &yc).await;
    let get = |k: &str| r.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    if get("scanned") > 0 {
        log::info!(
            target: LOG,
            "用友云上传失败重试完成: 扫描{}条, 成功{}, 仍失败{}, 跳过{}",
            get("scanned"),
            get("succeeded"),
            get("failed"),
            get("skipped")
        );
    } else {
        log::debug!(target: LOG, "用友云上传失败重试: 无待重试记录");
    }
}

async fn delivery_sync_task(_svc: Arc<Services>) {
    if !config::get().delivery_sync_enabled {
        log::debug!(target: LOG, "发货单快照同步任务已禁用，跳过");
        return;
    }
    let r = delivery_sync::sync_delivery_snapshot("scheduled").await;
    let err = r.get("error").map(|v| v.as_str().unwrap_or("None").to_string()).unwrap_or_else(|| "None".into());
    if r.get("skipped").map(|v| !v.is_null()).unwrap_or(false) {
        log::info!(target: LOG, "发货单快照同步跳过: {}", err);
    } else if !r.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        log::warn!(target: LOG, "发货单快照同步失败: {}", err);
    }
}

/// WebDAV文件完整性巡检: 抽样最近N条成功记录, GET回读比对大小, 仅记录日志
async fn webdav_integrity_check_task(svc: Arc<Services>) {
    let max_records: i64 = 200;
    log::info!(target: LOG, "执行WebDAV文件完整性检查任务");
    if !svc.file_manager.check_webdav_health().await {
        log::warn!(target: LOG, "WebDAV不可用,跳过完整性检查任务");
        return;
    }
    let client = WebDavClient::new();
    let rows = match db::run(move |c| {
        db::query_rows(
            c,
            r#"
                SELECT id, file_name, file_size, webdav_path
                FROM upload_history
                WHERE status = 'success'
                  AND webdav_path IS NOT NULL
                  AND file_size IS NOT NULL
                  AND file_size > 0
                ORDER BY id DESC
                LIMIT ?
                "#,
            &[rusqlite::types::Value::Integer(max_records)],
        )
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            log::error!(target: LOG, "WebDAV文件完整性检查任务异常: {}", e);
            return;
        }
    };
    if rows.is_empty() {
        log::info!(target: LOG, "WebDAV完整性检查: 无需检查的记录");
        return;
    }
    let mut problems = 0;
    for r in &rows {
        let id = db::opt_text(&r[0]).unwrap_or_default();
        let name = db::opt_text(&r[1]).unwrap_or_else(|| "None".into());
        let size = db::opt_text(&r[2]).unwrap_or_default();
        let wp = db::opt_text(&r[3]).unwrap_or_default();
        match client.get_remote_actual_size(&wp).await {
            Ok(None) => {
                problems += 1;
                log::error!(
                    target: LOG,
                    "[WebDAV完整性问题] id={}, 文件={}, 远端文件不存在, webdav_path={}",
                    id,
                    name,
                    wp
                );
            }
            Ok(Some(remote)) => {
                if remote == 0 || remote.to_string() != size {
                    problems += 1;
                    log::error!(
                        target: LOG,
                        "[WebDAV完整性问题] id={}, 文件={}, 本地file_size={}, 远端size={}, webdav_path={}",
                        id,
                        name,
                        size,
                        remote,
                        wp
                    );
                }
            }
            Err(e) => {
                log::error!(target: LOG, "[WebDAV完整性检查异常] id={}, 文件={}, 错误={}", id, name, e);
            }
        }
    }
    log::info!(
        target: LOG,
        "WebDAV文件完整性检查完成: 共检查{}条记录, 发现{}条疑似异常",
        rows.len(),
        problems
    );
}

// ========== 调度 ==========

type JobFn = fn(Arc<Services>) -> Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone, Copy)]
enum Trigger {
    /// 固定间隔(秒); first_delay 为首次运行延迟(秒), None 表示一个间隔后
    Interval { seconds: i64, first_delay: Option<i64> },
    /// 每日 hour:00 (北京时间)
    Daily { hour: u32 },
}

struct Job {
    id: &'static str,
    name: &'static str,
    trigger: Trigger,
    func: JobFn,
}

fn next_daily(now: NaiveDateTime, hour: u32) -> NaiveDateTime {
    let today = now.date().and_hms_opt(hour, 0, 0).unwrap();
    if today > now {
        today
    } else {
        today + ChronoDuration::days(1)
    }
}

fn trigger_str(t: &Trigger) -> String {
    match t {
        Trigger::Interval { seconds, .. } => {
            let h = seconds / 3600;
            let m = (seconds % 3600) / 60;
            let s = seconds % 60;
            format!("interval[{}:{:02}:{:02}]", h, m, s)
        }
        Trigger::Daily { hour } => format!("cron[hour='{}', minute='0']", hour),
    }
}

fn fmt_aware(dt: &NaiveDateTime) -> String {
    format!("{} CST", dt.format("%Y-%m-%d %H:%M:%S"))
}

pub struct Scheduler {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

macro_rules! job_fn {
    ($f:ident) => {
        |svc: Arc<Services>| -> Pin<Box<dyn Future<Output = ()> + Send>> { Box::pin($f(svc)) }
    };
}

impl Scheduler {
    pub async fn start(file_manager: Arc<FileManager>) -> Scheduler {
        let s = config::get();
        let svc = Arc::new(Services { file_manager, backup: tokio::sync::OnceCell::new() });
        log::info!(target: LOG, "开始设置定时任务");

        let mut jobs: Vec<Job> = Vec::new();
        jobs.push(Job {
            id: "webdav_health_check",
            name: "WebDAV健康检查",
            trigger: Trigger::Interval { seconds: s.health_check_interval, first_delay: None },
            func: job_fn!(webdav_health_check_task),
        });
        log::info!(target: LOG, "已设置WebDAV健康检查任务，间隔{}秒", s.health_check_interval);
        jobs.push(Job {
            id: "cache_cleanup",
            name: "缓存清理",
            trigger: Trigger::Daily { hour: 2 },
            func: job_fn!(cleanup_cache_task),
        });
        log::info!(target: LOG, "已设置缓存清理任务，每日凌晨2点执行");
        jobs.push(Job {
            id: "database_backup",
            name: "数据库备份",
            trigger: Trigger::Daily { hour: 0 },
            func: job_fn!(database_backup_task),
        });
        log::info!(target: LOG, "已设置数据库备份任务，每日凌晨0点执行");
        jobs.push(Job {
            id: "sync_pending_files",
            name: "待同步文件检查",
            trigger: Trigger::Interval { seconds: s.sync_retry_interval, first_delay: None },
            func: job_fn!(sync_pending_files_task),
        });
        log::info!(target: LOG, "已设置待同步文件检查任务，间隔{}秒", s.sync_retry_interval);
        jobs.push(Job {
            id: "webdav_integrity_check",
            name: "WebDAV文件完整性检查",
            trigger: Trigger::Daily { hour: 3 },
            func: job_fn!(webdav_integrity_check_task),
        });
        log::info!(target: LOG, "已设置WebDAV文件完整性检查任务，每日凌晨3点执行");

        if s.yonyou_retry_enabled {
            jobs.push(Job {
                id: "retry_yonyou_uploads",
                name: "用友云上传失败重试",
                trigger: Trigger::Interval { seconds: s.yonyou_retry_interval_hours * 3600, first_delay: None },
                func: job_fn!(retry_yonyou_uploads_task),
            });
            log::info!(target: LOG, "已设置用友云上传失败重试任务，间隔{}小时", s.yonyou_retry_interval_hours);
        } else {
            log::info!(target: LOG, "用友云上传失败重试任务已禁用(YONYOU_RETRY_ENABLED=False)");
        }

        if s.delivery_sync_enabled {
            jobs.push(Job {
                id: "delivery_sync",
                name: "发货单快照同步",
                // 启动15秒后先补一轮, 避免应用重启后快照为空的冷启动窗口
                trigger: Trigger::Interval { seconds: s.delivery_sync_interval_minutes * 60, first_delay: Some(15) },
                func: job_fn!(delivery_sync_task),
            });
            log::info!(target: LOG, "已设置发货单快照同步任务，间隔{}分钟", s.delivery_sync_interval_minutes);
        } else {
            log::info!(target: LOG, "发货单快照同步任务已禁用(DELIVERY_SYNC_ENABLED=False)");
        }

        log::info!(target: LOG, "定时任务调度器已启动");
        log::info!(target: LOG, "已加载 {} 个定时任务:", jobs.len());
        let start = beijing_now_naive();
        for j in &jobs {
            let next = match j.trigger {
                Trigger::Interval { seconds, first_delay } => {
                    start + ChronoDuration::seconds(first_delay.unwrap_or(seconds))
                }
                Trigger::Daily { hour } => next_daily(start, hour),
            };
            log::info!(
                target: LOG,
                "  - {}: {} (下次运行: {})",
                j.id,
                j.name,
                next.format("%Y-%m-%d %H:%M:%S")
            );
        }

        let handles = jobs.into_iter().map(|j| tokio::spawn(run_job(j, svc.clone(), start))).collect();
        Scheduler { handles }
    }

    pub fn shutdown(self) {
        for h in self.handles {
            h.abort();
        }
        log::info!(target: LOG, "定时任务调度器已关闭");
    }
}

async fn sleep_until_naive(target: NaiveDateTime) {
    let now = beijing_now_naive();
    if target > now {
        let d = (target - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(d).await;
    }
}

async fn run_job(job: Job, svc: Arc<Services>, start: NaiveDateTime) {
    let running = Arc::new(AtomicBool::new(false));
    let mut n: i64 = 0;
    let mut next = match job.trigger {
        Trigger::Interval { seconds, first_delay } => start + ChronoDuration::seconds(first_delay.unwrap_or(seconds)),
        Trigger::Daily { hour } => next_daily(start, hour),
    };
    loop {
        sleep_until_naive(next).await;
        let scheduled = next;

        // 计算下次触发时间 (错过的触发合并为一次)
        let now = beijing_now_naive();
        next = match job.trigger {
            Trigger::Interval { seconds, .. } => {
                let interval = seconds.max(1);
                loop {
                    n += 1;
                    let t = start + ChronoDuration::seconds(interval * n);
                    if t > now {
                        break t;
                    }
                }
            }
            Trigger::Daily { hour } => next_daily(now, hour),
        };

        // misfire_grace_time = 30 秒
        if (now - scheduled).num_seconds() > 30 {
            log::warn!(
                target: APS_LOG,
                "Run time of job \"{} (trigger: {}, next run at: {})\" was missed by {}",
                job.name,
                trigger_str(&job.trigger),
                fmt_aware(&next),
                now - scheduled
            );
            continue;
        }

        if running.swap(true, Ordering::SeqCst) {
            log::warn!(
                target: APS_LOG,
                "Execution of job \"{} (trigger: {}, next run at: {})\" skipped: maximum number of running instances reached (1)",
                job.name,
                trigger_str(&job.trigger),
                fmt_aware(&next)
            );
            continue;
        }

        log::info!(
            target: APS_LOG,
            "Running job \"{} (trigger: {}, next run at: {})\" (scheduled at {})",
            job.name,
            trigger_str(&job.trigger),
            fmt_aware(&next),
            scheduled.format("%Y-%m-%d %H:%M:%S%.6f+08:00")
        );
        let f = job.func;
        let svc2 = svc.clone();
        let flag = running.clone();
        let func_name = job.name;
        let trig = trigger_str(&job.trigger);
        let next_s = fmt_aware(&next);
        tokio::spawn(async move {
            let h = tokio::spawn(f(svc2));
            if let Err(e) = h.await {
                log::error!(target: LOG, "{} 异常: {}", func_name, e);
            }
            flag.store(false, Ordering::SeqCst);
            log::info!(
                target: APS_LOG,
                "Job \"{} (trigger: {}, next run at: {})\" executed successfully",
                func_name,
                trig,
                next_s
            );
        });
    }
}
