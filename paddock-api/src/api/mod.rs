//! /v1 模块 API：auth + laps + leaderboard + driver + me。
//! Toast 判定在 POST /v1/laps 的服务端事务里完成（判定权归服务端，防模块榜单缓存不一致）。

pub mod account;
pub mod auth_handlers;
pub mod avatar;
pub mod driver;
pub mod laps;
pub mod leaderboard;

use axum::Router;

pub fn router() -> Router<crate::state::AppState> {
    Router::new()
        .merge(account::router())
        .merge(auth_handlers::router())
        .merge(avatar::router())
        .merge(laps::router())
        .merge(leaderboard::router())
        .merge(driver::router())
}
