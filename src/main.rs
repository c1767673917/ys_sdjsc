//! 单据上传管理系统 (djgl-hk) Rust 版
//!
//! 由原 Python/FastAPI 实现重构而来, 对外接口、数据库结构、文件存储与定时任务行为保持一致。

mod api;
mod backup;
mod config;
mod db;
mod delivery_sync;
mod file_manager;
mod http;
mod image_overlay;
mod logging;
mod pyutil;
mod retry_service;
mod routes;
mod scheduler;
mod state;
mod upload_types;
mod webdav;
mod yonyou;

use std::net::SocketAddr;
use std::sync::Arc;

const LOG: &str = "app.main";

#[tokio::main]
async fn main() {
    // 配置校验失败时直接退出 (等价导入阶段 get_settings() 抛出 ValueError)
    let settings = match config::init() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("配置错误: {}", e);
            std::process::exit(1);
        }
    };

    // 各模块实例 (导入阶段即创建目录)
    let state = state::AppState::new();

    // ===== startup 事件 =====
    logging::setup_logging(
        if settings.debug { "DEBUG" } else { "INFO" },
        true,
        true,
        "logs/app.log",
        true,
    );
    log::info!(target: LOG, "启动应用: {} v{}", settings.app_name, settings.app_version);

    if let Err(e) = db::init_database() {
        log::error!(target: LOG, "数据库初始化失败: {}", e);
        std::process::exit(3);
    }
    log::info!(target: LOG, "数据库初始化完成");

    if let Err(e) = db::verify_database_schema() {
        log::error!(target: LOG, "应用启动失败 - 数据库Schema不完整");
        eprintln!("{}", e);
        std::process::exit(3);
    }

    // 调度器与 HTTP 监听之前，把重启中断的 uploading 记录恢复为可重试或成功
    match retry_service::recover_stale_uploading().await {
        Ok(n) if n > 0 => log::info!(target: LOG, "已恢复 {} 条中断的上传记录", n),
        Ok(_) => {}
        Err(e) => log::error!(target: LOG, "恢复中断的上传记录失败: {}", e),
    }

    let (valid, errors, warnings) = settings.validate_webdav_health();
    if !valid {
        let msg = format!("WebDAV配置验证失败: {}", errors.join(", "));
        log::error!(target: LOG, "{}", msg);
        if settings.debug {
            std::process::exit(3);
        }
    } else {
        for w in warnings {
            log::warn!(target: LOG, "WebDAV配置警告: {}", w);
        }
    }

    let scheduler = scheduler::Scheduler::start(Arc::new(file_manager::FileManager::new())).await;
    log::info!(target: LOG, "定时任务调度器启动成功");
    log::info!(target: LOG, "应用启动完成");

    let app = routes::build(state);
    let addr = format!("{}:{}", settings.host, settings.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ERROR:    [Errno] error while attempting to bind on address ('{}'): {}", addr, e);
            std::process::exit(1);
        }
    };
    println!("INFO:     Uvicorn running on http://{} (Press CTRL+C to quit)", addr);

    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal());
    if let Err(e) = server.await {
        log::error!(target: LOG, "服务器异常退出: {}", e);
    }

    // ===== shutdown 事件 =====
    scheduler.shutdown();
    log::info!(target: LOG, "定时任务调度器已关闭");
    log::info!(target: LOG, "应用已关闭");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("INFO:     Shutting down");
}
