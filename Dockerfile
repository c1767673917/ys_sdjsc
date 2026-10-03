# ===== 构建阶段 =====
FROM rust:1.88-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release

# ===== 运行阶段 =====
FROM debian:bookworm-slim

WORKDIR /app

# 时区: 缓存过期等逻辑按本地时间与北京时间比较
ENV TZ=Asia/Shanghai

# 运行依赖: 文泉驿微米黑(导出图片叠加单号), 证书, curl(健康检查)
RUN apt-get update && apt-get install -y --no-install-recommends \
    fonts-wqy-microhei \
    ca-certificates \
    tzdata \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/djgl-hk /app/djgl-hk
COPY app ./app
COPY migrations ./migrations

# 创建数据目录和日志目录
RUN mkdir -p /app/data /app/logs && \
    chmod -R 755 /app/data /app/logs

EXPOSE 10000

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:10000/api/health || exit 1

CMD ["/app/djgl-hk"]
