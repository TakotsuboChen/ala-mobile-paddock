//! 共享应用状态：Postgres 连接池 + bot 发送队列句柄。
//! main() 启动时经 `with_bot_tx` 注入发送通道；测试/禁用 bot 用 `new`（无队列）。

use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::qq_bot::SendJob;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub bot: Arc<BotHandle>,
}

/// bot 发送通道。tx = None：队列未启用（bot 静默，仅验签）。
pub struct BotHandle {
    pub tx: Option<mpsc::Sender<SendJob>>,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            bot: Arc::new(BotHandle { tx: None }),
        }
    }

    /// 装上 bot 发送队列（main 启动时调用）。
    pub fn with_bot_tx(mut self, tx: mpsc::Sender<SendJob>) -> Self {
        self.bot = Arc::new(BotHandle { tx: Some(tx) });
        self
    }
}