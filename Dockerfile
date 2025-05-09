FROM rust:1.76-slim as builder

WORKDIR /usr/src/app

# 安装构建依赖
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev build-essential && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# 复制项目文件
COPY . .

# 构建项目（发布模式）
RUN cargo build --release

# 使用较小的镜像运行
FROM debian:bookworm-slim

WORKDIR /app

# 安装运行时依赖
RUN apt-get update && \
    apt-get install -y ca-certificates libssl-dev && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# 从构建阶段复制二进制文件和必要资源
COPY --from=builder /usr/src/app/target/release/copy-bot /app/
COPY --from=builder /usr/src/app/config.toml /app/
COPY --from=builder /usr/src/app/idls /app/idls/
COPY --from=builder /usr/src/app/parsers /app/parsers/

# 创建日志目录
RUN mkdir -p /app/logs

# 设置环境变量
ENV RUST_LOG=info

# 设置工作目录
WORKDIR /app

# 运行程序
CMD ["./copy-bot"] 