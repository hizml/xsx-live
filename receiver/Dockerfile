# syntax=docker/dockerfile:1
# 在仓库根目录构建：docker build -f receiver/Dockerfile -t xsx-live/receiver .
# 构建上下文必须是整个仓库：管理页（dashboard/）和播放页（www/）的文件在编译时编进程序里
FROM rust@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS build
WORKDIR /src
# 第一步只编译依赖：先放一份只有一个空 main.rs 的源码。依赖清单没变时这一层直接用缓存，
# 之后源码怎么改都不用重新编译依赖
COPY receiver/Cargo.toml receiver/Cargo.lock receiver/
RUN mkdir -p receiver/src && echo 'fn main() {}' > receiver/src/main.rs \
    && cargo build --release --locked --manifest-path receiver/Cargo.toml
COPY dashboard/ dashboard/
COPY www/ www/
COPY receiver/ receiver/
# 拷进来的源码，修改时间可能早于上一步的编译结果，碰一下让 cargo 认得出内容变了
RUN touch receiver/src/main.rs receiver/src/lib.rs \
    && cargo build --release --locked --manifest-path receiver/Cargo.toml

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates ffmpeg && apt-get clean && find /var/lib/apt/lists -type f -delete
COPY --from=build /src/receiver/target/release/xsx-native /usr/local/bin/xsx-native
# 镜像标签里的版本号、提交号和仓库地址由构建方传入，GitHub Actions 会自动填
ARG VERSION=unknown
ARG REVISION=unknown
ARG SOURCE=https://github.com/EBOLABOY/xsx-live
LABEL org.opencontainers.image.title="xsx-live-receiver" \
      org.opencontainers.image.source=$SOURCE \
      org.opencontainers.image.version=$VERSION \
      org.opencontainers.image.revision=$REVISION
ENV XBOX_AUTH_FILE=/files/auth/session.json STATE_DIR=/files/state WWW_DIR=/files/www
ENTRYPOINT ["/usr/local/bin/xsx-native"]
