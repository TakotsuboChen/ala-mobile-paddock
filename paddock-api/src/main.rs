//! Ala Mobile 围场（Paddock）私服后端。
//! 架构：axum 单二进制 = /v1/* 模块 API + /admin/* 管理页 + /qq/webhook（S4）。
//! 契约源：ala-mobile-tool 仓库 docs/PADDOCK_PLAN.md（两边共用，勿单侧演化）。

mod admin;
mod api;
mod auth;
mod qq_bot;
mod state;

use anyhow::Context;
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "paddock_api=info,tower_http=warn".into()),
        )
        .init();

    let db_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL 未设置（docker-compose 内由 app 容器环境变量提供）")?;
    let pool = PgPoolOptions::new()
        .max_connections(8) // 轻量 VPS：小连接池即可
        .connect(&db_url)
        .await
        .context("连接 Postgres 失败")?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("执行数据库迁移失败")?;

    // bot 发送队列：凭据由管理端设置页动态配置，worker 出队时实时读取，无需重启
    let (bot_tx, bot_rx) = tokio::sync::mpsc::channel(64);
    qq_bot::run_sender(pool.clone(), bot_rx);
    let app_state = state::AppState::new(pool).with_bot_tx(bot_tx);
    let app = Router::new()
        .nest("/v1", api::router())
        .nest("/admin", admin::router())
        .nest("/qq", qq_bot::router())
        .layer(TraceLayer::new_for_http())
        .with_state(app_state);

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("paddock-api listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
