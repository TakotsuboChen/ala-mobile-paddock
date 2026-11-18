# 多阶段构建：rust 构建出零依赖小镜像
FROM rust:1.97-alpine AS builder
WORKDIR /build
RUN apk add --no-cache musl-dev
COPY Cargo.toml Cargo.lock ./
COPY paddock-api ./paddock-api
ENV SQLX_OFFLINE=true
# ⚠️ cache mount 让容器内 target/ 独立于层链存活。
# 没有它：源码改一行 → COPY paddock-api 层失效 → RUN 层连带重跑 →
#   容器内从零编译 301 个依赖 + fat LTO（实测 3m42s）。
# 有了它：依赖的 .rlib 跨层复用，只需重编 paddock-api 自身（实测 2m00s）。
# 注意：它只影响「复用」，不改变任何编译参数 ⇒ 产物字节级不变。
#
# ⚠️ cache mount 的内容 **不会写回镜像层**，所以必须把产物拷到 cache 之外的
#   路径，否则下一阶段的 COPY --from=builder 会报 "not found"。
#
# ⚠️ `id=` 必须显式写：默认 id **就是 target 路径**（实测 `id "//build/target"`），
#   将来任何用同一挂载点的 Dockerfile 会静默共用同一份缓存，排查时表现为
#   「缓存被别的项目污染」。显式 id 让两件事解耦。
#
# ⚠️ 另挂 cargo registry：当前只挂 target 时，容器内 /usr/local/cargo/registry
#   是空的（实测）。改依赖版本才会触发重下 301 crate 的网络等待，
#   日常改代码不受影响（Cargo.toml/Cargo.lock 层命中缓存）。
RUN --mount=type=cache,target=/build/target,id=paddock-target \
    --mount=type=cache,target=/usr/local/cargo/registry,id=paddock-registry \
    cargo build --release -p paddock-api && \
    mkdir -p /build/out && \
    cp /build/target/release/paddock-api /build/out/paddock-api

FROM alpine:3.21
RUN adduser -D -H paddock
COPY --from=builder /build/out/paddock-api /usr/local/bin/paddock-api
COPY paddock-api/migrations /app/migrations
USER paddock
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
CMD ["paddock-api"]