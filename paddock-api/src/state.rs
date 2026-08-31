//! 共享应用状态：Postgres 连接池。
//! S4 起逐步加入：bot 凭据、发送队列通道、Garage 客户端。

use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}
