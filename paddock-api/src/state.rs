//! 共享应用状态：Postgres 连接池 + 两个 bot 发送队列句柄。
//! main() 启动时经 `with_bot_tx` 注入发送通道；测试/禁用 bot 用 `new`（无队列）。
//!
//! ⚠️ **两个平台各自独立队列**（用户 2026-11-03 定案）：
//!   - QQ：未认证 30 条/分钟 ⇒ worker 串行 2 秒间隔
//!   - Telegram：单聊约 1 条/秒、单群 20 条/分钟 ⇒ 100ms 间隔足够
//! 共用队列会让 TG 的回复延迟被 QQ 的 2 秒节流拖垮；独立队列同时保证
//! 一个平台被限流时另一个完全不受影响。

use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::qq_bot::SendJob;
use crate::telegram_bot::TgSendJob;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub bot: Arc<BotHandle>,
}

/// bot 发送通道。tx = None：该平台队列未启用（静默，仅验签）。
pub struct BotHandle {
    pub tx: Option<mpsc::Sender<SendJob>>,
    pub tg_tx: Option<mpsc::Sender<TgSendJob>>,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            bot: Arc::new(BotHandle { tx: None, tg_tx: None }),
        }
    }

    /// 装上 QQ 发送队列（main 启动时调用）。
    pub fn with_bot_tx(mut self, tx: mpsc::Sender<SendJob>) -> Self {
        self.bot = Arc::new(BotHandle {
            tx: Some(tx),
            tg_tx: self.bot.tg_tx.clone(),
        });
        self
    }

    /// 装上 Telegram 发送队列（main 启动时调用）。
    pub fn with_tg_tx(mut self, tx: mpsc::Sender<TgSendJob>) -> Self {
        self.bot = Arc::new(BotHandle {
            tx: self.bot.tx.clone(),
            tg_tx: Some(tx),
        });
        self
    }
}
