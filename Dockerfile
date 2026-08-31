# 多阶段构建：rust 构建出零依赖小镜像
FROM rust:1.97-alpine AS builder
WORKDIR /build
RUN apk add --no-cache musl-dev
COPY Cargo.toml Cargo.lock ./
COPY paddock-api ./paddock-api
ENV SQLX_OFFLINE=true
RUN cargo build --release -p paddock-api

FROM alpine:3.21
RUN adduser -D -H paddock
COPY --from=builder /build/target/release/paddock-api /usr/local/bin/paddock-api
COPY paddock-api/migrations /app/migrations
USER paddock
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
CMD ["paddock-api"]