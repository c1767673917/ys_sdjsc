# 单据上传管理系统 (djgl-hk) — Rust 版

由 `djgl-hk` 的 Python/FastAPI 实现（以 git HEAD `bed869b` 为准）重构而来。对外接口、数据库结构、文件存储布局、定时任务和配置项都与原实现保持一致，前端（`app/static`、`app/pages`）原样复用，未做修改。

## 运行

```bash
cp .env.example .env        # 配置项与原 Python 版完全相同
cargo build --release
./target/release/djgl-hk    # 在包含 app/、data/ 的目录下运行
```

- 启动时会校验配置；校验不通过直接退出（退出码 1），提示信息与原版相同。
- 数据库沿用原版的 `data/uploads.db`，可直接接管现有数据。新库需要先执行 `migrations/add_webdav_support.sql`，否则启动时 Schema 校验不通过（与原版一致）。
- Docker：`docker compose up -d --build`（多阶段构建，运行镜像内置文泉驿微米黑字体）。

## 模块对照

| Python | Rust |
|---|---|
| `app/core/config.py` | `src/config.rs` |
| `app/core/database.py` | `src/db.rs` |
| `app/core/webdav_client.py` / `exceptions.py` | `src/webdav.rs` |
| `app/core/file_manager.py` | `src/file_manager.rs` |
| `app/core/yonyou_client.py` | `src/yonyou.rs` |
| `app/core/yonyou_retry_service.py` | `src/retry_service.rs` |
| `app/core/delivery_sync_service.py` | `src/delivery_sync.rs` |
| `app/core/backup_service.py` | `src/backup.rs` |
| `app/core/image_overlay.py` | `src/image_overlay.rs` |
| `app/core/logging_config.py` | `src/logging.rs` |
| `app/core/auth.py`、`app/main.py` 页面路由 | `src/api/pages.rs`、`src/routes.rs` |
| `app/api/*.py` | `src/api/*.rs` |
| `app/scheduler.py` | `src/scheduler.rs` |
| FastAPI 错误格式 / CORS / StaticFiles | `src/http.rs` |

`scripts/` 下的运维脚本仍使用原 Python 版本，未移植。

## 一致性验证

用同一份数据库副本，分别启动原 Python 版和 Rust 版，后端接入同一套 YonYou/WebDAV 模拟服务，再逐个请求比对状态码、关键响应头和响应体。覆盖范围：

- 全部只读接口（记录分页、筛选、统计、历史、物流选项、导出、WebDAV/迁移/备份状态、门户），以及鉴权、404/405/307、静态文件、CORS 和各类 422/400 校验
- 上传（物流/仓库/业务失败/超大文件/WebDAV 不可用降级）及后台任务写库结果
- 预览/下载、勾选/备注/软删除、发货单快照同步（成功与失败）、物流链接与门户、token 重置
- 待同步文件恢复、备份打包上传、迁移任务、用友失败重试服务

数据库最终状态逐字段一致（随机文件名后缀除外）。导出的 Excel 单元格完全一致；叠加单号的图片布局一致，文字宽度与 Pillow 逐像素吻合，仅抗锯齿像素不同。

## 刻意保留的原版行为

以下行为来自原实现，为了"功能完全一致"而原样保留：

- **备份日志从不写入**：原版 `_log_backup_result` / `get_backup_status` 误用了上下文管理器，会抛错并被吞掉。所以 `backup_logs` 表从不写入，`/api/admin/webdav/backup/status` 总是返回空统计。
- **迁移任务 409 被包装成 500**：已有迁移任务在运行时，再次启动返回 500，`detail` 为 `启动迁移任务失败: 409: ...`。
- **`/uploaded_files/*` 是静态挂载**：原版 `main.py` 中同路径的"缓存优先、WebDAV 兜底"路由被静态挂载遮蔽，从未生效。
- **WebDAV `list_files` 结果包含目录自身**：返回的列表里会有被查询目录这一项。
- **超大文件时前面的记录卡在 pending**：一次上传多个文件、其中某个超过大小限制时，排在它前面的记录已入库，但后台任务不会执行，状态一直是 `pending`。
- **422 响应带 `url` 字段**：错误项包含 `url`（`https://errors.pydantic.dev/2.5/v/...`），与原版 Docker 镜像锁定的 FastAPI 0.104.1 + pydantic 2.5 一致。

## 已知的非功能性差异

- **网络错误文案**：常见情况（连接失败、DNS、超时）已映射为 httpx 的文案，例如 `All connection attempts failed`；其他少见底层错误的文字会不同。
- **待同步清单并发写入**：原版在同步期间新登记的条目可能被覆盖丢失（竞态），这里改为合并保存。
- **日志行号与调度器日志细节**：日志格式与原版相同，但行号指向 Rust 源码，APScheduler 风格的调度日志只做近似。
- **未找到中文字体时**：Pillow 会退回默认字体；Rust 版跳过叠加并返回原图（Docker 镜像已内置字体，不受影响）。
