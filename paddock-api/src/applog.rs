//! 统一业务事件日志（app_logs 表，管理端"日志"页数据源）。
//! 与 tracing 应用日志分离：这里只收"谁在何时做了什么"的业务事件。
//! 两条写入路径：
//!   * `log_event`：fire-and-forget（tokio::spawn），业务事件（登录/上传/bot）用这条——
//!     日志失败绝不影响主流程；
//!   * `log_event_tx`：事务内写入，管理端敏感操作用这条——审计与动作同生共死，
//!     防止"操作成功但审计丢失"。
//! 脱敏红线：调用方不得把密码/token/secret 放进 message/detail。

use sqlx::PgPool;

/// fire-and-forget 业务事件。池不可用或写入失败仅 tracing 记录，静默吞掉。
pub fn log_event(pool: &PgPool, level: &str, category: &str, event: &str, actor: &str, message: impl Into<String>, detail: serde_json::Value) {
    let pool = pool.clone();
    let level = level.to_string();
    let category = category.to_string();
    let event = event.to_string();
    let actor = actor.to_string();
    let message = message.into();
    tokio::spawn(async move {
        if let Err(e) = insert(&pool, &level, &category, &event, &actor, &message, &detail).await {
            tracing::warn!("[applog] 写日志失败 event={event}: {e}");
        }
    });
}

/// 事务内写入（管理端敏感操作与业务动作同事务提交）。
pub async fn log_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    level: &str,
    category: &str,
    event: &str,
    actor: &str,
    message: impl Into<String>,
    detail: serde_json::Value,
) {
    if let Err(e) = insert_tx(tx, level, category, event, actor, &message.into(), &detail).await {
        tracing::warn!("[applog] 事务内写日志失败 event={event}: {e}");
    }
}

/// 90 天保留：启动时清一次过期行（轻量 VPS 足够，无需定时任务）。
pub async fn purge_expired(pool: &PgPool) {
    match sqlx::query("DELETE FROM app_logs WHERE created_at < now() - interval '90 days'")
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!("[applog] 已清理 {} 条过期日志", r.rows_affected())
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("[applog] 过期清理失败: {e}"),
    }
}

async fn insert(
    pool: &PgPool,
    level: &str,
    category: &str,
    event: &str,
    actor: &str,
    message: &str,
    detail: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO app_logs (level, category, event, actor, message, detail) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(level)
        .bind(category)
        .bind(event)
        .bind(actor)
        .bind(message)
        .bind(detail)
        .execute(pool)
        .await
        .map(|_| ())
}

async fn insert_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    level: &str,
    category: &str,
    event: &str,
    actor: &str,
    message: &str,
    detail: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO app_logs (level, category, event, actor, message, detail) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(level)
        .bind(category)
        .bind(event)
        .bind(actor)
        .bind(message)
        .bind(detail)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}
