//! 账号设置：头像/用户名 30 天冷却 + 屏蔽缺失配置的圈速记录。
//!
//! ## 端点
//!
//! - `GET  /v1/me/account` — 账号设置页所需的全部状态（冷却剩余 + 屏蔽资格/是否已屏蔽）
//! - `PUT  /v1/me/username` — 改用户名（登录名同步变化，30 天一次）
//! - `POST /v1/me/hide-missing` — 屏蔽缺失配置的记录（一次性、不可撤销）
//!
//! 头像上传本身仍在 `avatar.rs`（`POST /v1/me/avatar`），本模块只补冷却检查。
//!
//! ## 为什么屏蔽要重算 best_laps 而不是加过滤条件
//!
//! 积分/排名/金标全部以 `best_laps` 为数据源（`score.rs::scored_cte` 是唯一窗口
//! 定义）。在榜单 SQL 里加 `NOT EXISTS (hidden_missing_laps)` 意味着**每一处**
//! 用到 best_laps 的地方都要加一遍（版本榜/总榜/`/v1/me`/车手主页/管理端），
//! 漏一处就是"榜上看不到但积分还在"。删除行则让所有下游自动一致。
//!
//! ## 为什么重算要用"排除被屏蔽 lap_id"而不是简单删行
//!
//! 屏蔽后 `best_laps` 该维度被删，但该用户在**同一维度**可能还有别的圈
//! （正常圈或另一条缺失圈）。用户定案「屏蔽 = 这些成绩不存在」，故重算时
//! 必须把被屏蔽的圈排除后重新取最快——否则一条比被屏蔽圈更慢的正常圈
//! 永远上不了榜（维度被删后没有触发路径）。
//!
//! ⚠️ **records（全服最快圈）也要一并重算**：被屏蔽的圈若持有赛道纪录，
//!    删掉 best_laps 行不影响 records（它读 laps），会出现"纪录属于一个
//!    榜上不存在的成绩"。`records` 只服务纪录播报与车手主页的纪录数统计。

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{post, put},
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    api::{auth_handlers::ApiError, laps},
    state::AppState,
};

/// 冷却时长：30 天。头像与用户名各自独立计时。
const COOLDOWN_DAYS: i64 = 30;

// ---------- 状态查询 ----------

#[derive(Serialize)]
pub struct AccountState {
    /// 距离下次可换头像还剩多少秒（0 = 现在就能换；从未换过 = 0）
    pub avatar_cooldown_secs: i64,
    /// 距离下次可改用户名还剩多少秒
    pub username_cooldown_secs: i64,
    /// 本人是否已经执行过屏蔽（true → 客户端直接弹「已屏蔽过」Toast，不发请求）
    pub hidden_missing: bool,
    /// 当前是否**存在**可屏蔽的缺失配置记录（false → 弹「没有可屏蔽的记录」）
    ///
    /// 口径 = 该用户 `laps` 里**任意一条**五维有缺失的圈（2026-10-20 修正）。
    /// ⚠️ 早期口径是「榜上获胜圈缺失的维度数 > 0」——那会让「维度里最快的圈配置
    ///    完整、次快圈缺失」的用户被判成"无可屏蔽记录"，与屏蔽动作实际要处理的
    ///    范围（全部缺失圈）不一致。判据必须与 `hide_missing` 的选取集合同源。
    pub has_hidden_candidates: bool,
}

/// GET /v1/me/account（Bearer）
pub async fn account_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AccountState>, ApiError> {
    let user_id = laps::authenticate(&state, &headers).await?;
    let row: Option<(Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as(
            "SELECT avatar_updated_at, username_updated_at, hidden_missing_at \
             FROM users WHERE id = $1",
        )
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?;
    let Some((avatar_at, username_at, hidden_at)) = row else {
        return Err(ApiError::unauthorized("账号不存在，请重新登录"));
    };
    let candidates = count_missing_candidates(&state, user_id).await?;
    Ok(Json(AccountState {
        avatar_cooldown_secs: cooldown_remaining(avatar_at),
        username_cooldown_secs: cooldown_remaining(username_at),
        hidden_missing: hidden_at.is_some(),
        has_hidden_candidates: candidates > 0,
    }))
}

/// 冷却剩余秒数（向上取整，已过期/从未操作 = 0）。
///
/// ⚠️ **未来时间戳按「无冷却」处理（返回 0），不能让它把冷却延长**：
///    `avatar_updated_at` 只由服务端 `now()` 写入，出现未来值只能是时钟回拨
///    或数据异常。此时若按"现在 - 未来时间"算，剩余会**超过** 30 天，
///    用户被锁死远超规格。返回 0 还能自我修复——用户下次操作会把时间戳
///    重写为当前时刻。
pub fn cooldown_remaining(at: Option<chrono::DateTime<chrono::Utc>>) -> i64 {
    let Some(at) = at else { return 0 };
    let elapsed = chrono::Utc::now().signed_duration_since(at);
    // 未来时间戳 → elapsed 为负 → 下面 remain > 30 天。显式挡掉。
    if elapsed < chrono::Duration::zero() {
        return 0;
    }
    let remain = chrono::Duration::days(COOLDOWN_DAYS) - elapsed;
    if remain <= chrono::Duration::zero() {
        0
    } else {
        // 向上取整：剩 0.4 秒要报 1 秒，否则客户端显示"0 秒后"却仍被服务端拒绝。
        remain.num_seconds() + if remain.subsec_nanos() > 0 { 1 } else { 0 }
    }
}

/// 可屏蔽记录数 = 该用户配置缺失的圈数。
///
/// ⚠️ **判据必须与 [`hide_missing`] 的选取集合同源**（2026-10-20 修正）：
///    早期这里算的是「榜上获胜圈缺失的维度数」，而 `hide_missing` 也只屏蔽获胜圈，
///    两者一致但**范围都错了**——用户点「屏蔽」时看到 65 条缺失圈里只标了 7 条，
///    被屏蔽的获胜圈由下一条同样缺失的圈接棒，于是「缺失」与「最佳」同挂一条。
///    现在两处统一为「该用户全部缺失圈」。
async fn count_missing_candidates(state: &AppState, user_id: Uuid) -> Result<i64, ApiError> {
    // 判据只有一份定义（`laps::ASSIST_MISSING_SQL`），这里把前缀换成 l.
    let sql = format!(
        "SELECT count(*)::bigint FROM laps l WHERE l.user_id = $1 AND {}",
        laps::ASSIST_MISSING_SQL
    );
    let n: i64 = sqlx::query_scalar(&sql)
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;
    Ok(n)
}

// ---------- 改用户名 ----------

#[derive(serde::Deserialize)]
pub struct ChangeUsernameReq {
    pub username: String,
}

#[derive(Serialize)]
pub struct ChangeUsernameResp {
    pub username: String,
    /// 距下次可改名的剩余秒数（客户端据此刷新页面状态）
    pub cooldown_secs: i64,
}

/// PUT /v1/me/username（Bearer）
///
/// ⚠️ 用户名是**登录名**（用户定案 2026-10-20）：改完必须用新名字登录。
///    旧名字因 UNIQUE 约束被释放，可被他人注册。
pub async fn change_username(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChangeUsernameReq>,
) -> Result<Json<ChangeUsernameResp>, ApiError> {
    let user_id = laps::authenticate(&state, &headers).await?;
    let new_name = req.username.trim().to_string();
    crate::auth::validate_username(&new_name).map_err(ApiError::bad_request)?;

    let mut tx = state.pool.begin().await?;
    // 行锁：两个并发改名请求必须串行，否则都会通过冷却检查。
    let current: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT username, username_updated_at FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((old_name, updated_at)) = current else {
        return Err(ApiError::unauthorized("账号不存在，请重新登录"));
    };
    if cooldown_remaining(updated_at) > 0 {
        return Err(ApiError::conflict("用户名 30 天内只能修改一次"));
    }
    if old_name == new_name {
        return Err(ApiError::bad_request("新用户名与当前用户名相同"));
    }
    // 唯一性：UNIQUE 约束兜底，但先查一次给出可读文案（约束冲突的报错是英文）。
    let taken: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM users WHERE username = $1")
        .bind(&new_name)
        .fetch_one(&mut *tx)
        .await?;
    if taken > 0 {
        return Err(ApiError::conflict("该用户名已被占用"));
    }
    sqlx::query("UPDATE users SET username = $2, username_updated_at = now() WHERE id = $1")
        .bind(user_id)
        .bind(&new_name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    tracing::info!("user {user_id} renamed: {old_name} -> {new_name}");
    Ok(Json(ChangeUsernameResp {
        username: new_name,
        cooldown_secs: COOLDOWN_DAYS * 86400,
    }))
}

// ---------- 屏蔽缺失配置的记录 ----------

#[derive(Serialize)]
pub struct HideMissingResp {
    /// 本次被移出榜单的维度数
    pub hidden: i64,
}

/// POST /v1/me/hide-missing（Bearer）
///
/// 一次性、不可撤销（用户定案 2026-10-20）。语义 = 「这些成绩不存在」：
/// 1. 把该用户**全部**配置缺失的圈记进 `hidden_missing_laps`（按 lap_id，快照语义）；
/// 2. 对这些圈涉及的每个 (赛道, 版本) 维度重算 `best_laps`（排除已屏蔽的圈）；
/// 3. 重算受影响赛道的 `records`。
///
/// ⚠️ **范围 = 全部缺失圈，不是「每维度获胜圈」**（2026-10-20 修正）：
///    首版用 `DISTINCT ON (gp_index, version_code)` 只取每维度最快的圈，结果
///    每维度最多屏蔽 1 条；被屏蔽的获胜圈移出 `best_laps` 后，回放会挑**下一条
///    最快的圈**顶上——而它往往同样缺失配置，于是「缺失」与「最佳」同挂一条记录，
///    用户看到「只有少部分显示已屏蔽，还有一堆没屏蔽，缺失配置的还显示最佳」。
///    实机数据：飞行雪绒 65 条缺失圈只标了 7 条（= 7 个维度的获胜圈）。
///
/// ⚠️ 第 2 步对**受影响维度无条件重算**，不判「获胜圈是否被屏蔽」：
///    回放口径与获胜圈口径一致（`lap_ms ASC, created_at ASC LIMIT 1`），若获胜圈
///    未被屏蔽，回放会选出同一行，结果不变。少一个分支就少一处口径分叉。
pub async fn hide_missing(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<HideMissingResp>, ApiError> {
    let user_id = laps::authenticate(&state, &headers).await?;

    let mut tx = state.pool.begin().await?;
    // 行锁串行化并发请求：否则两个请求会各自通过「未屏蔽」检查。
    let hidden_at: Option<Option<chrono::DateTime<chrono::Utc>>> =
        sqlx::query_scalar("SELECT hidden_missing_at FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    match hidden_at {
        None => return Err(ApiError::unauthorized("账号不存在，请重新登录")),
        Some(Some(_)) => {
            return Err(ApiError::conflict(
                "您已经屏蔽过缺失配置的记录，该操作无法撤销",
            ))
        }
        Some(None) => {}
    }

    // 1) 该用户全部配置缺失的圈。判据走 `laps::ASSIST_MISSING_SQL`（单一事实源），
    //    与 `count_missing_candidates` 同源——预判"有没有可屏蔽的"与实际屏蔽的行集
    //    必须一致，否则用户会看到「有候选」→ 点了却报「没有可屏蔽的记录」。
    #[derive(sqlx::FromRow)]
    struct MissingLap {
        id: Uuid,
        gp_index: i16,
        version_code: i32,
    }
    let missing: Vec<MissingLap> = sqlx::query_as(&format!(
        "SELECT l.id, l.gp_index, l.version_code FROM laps l \
         WHERE l.user_id = $1 AND {}",
        laps::ASSIST_MISSING_SQL
    ))
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;

    if missing.is_empty() {
        return Err(ApiError::bad_request("您没有可以屏蔽的圈速记录"));
    }

    // 2) 记快照。ON CONFLICT DO NOTHING 防御性写法：正常路径下这些行不可能已存在
    //    （屏蔽过就早返回了），但**重复执行本函数的修复脚本**需要它幂等。
    for m in &missing {
        sqlx::query(
            "INSERT INTO hidden_missing_laps (user_id, lap_id) VALUES ($1, $2) \
             ON CONFLICT (user_id, lap_id) DO NOTHING",
        )
        .bind(user_id)
        .bind(m.id)
        .execute(&mut *tx)
        .await?;
    }

    // 3) 受影响维度集合。同一维度可能有多条缺失圈，必须去重——否则会对同一
    //    维度反复删/插 best_laps（结果相同但白跑 N 次查询）。
    let mut dims: Vec<(i16, i32)> = missing.iter().map(|m| (m.gp_index, m.version_code)).collect();
    dims.sort_unstable();
    dims.dedup();

    // 4) 从剩余圈回放 best_laps：排除已屏蔽的 lap_id，取最快的一条。
    //    ⚠️ 排除条件必须用 hidden_missing_laps 而不是"配置完整"——屏蔽是快照，
    //       屏蔽后新上传的缺失圈照常进榜（用户定案），不能拿配置判据当过滤器。
    //    ⚠️ 先 DELETE 再回放（而不是直接 upsert）：若该维度屏蔽后已无可用圈，
    //       必须让 best_laps 行消失。回放命中时再 upsert 回来。
    for (gp, ver) in &dims {
        sqlx::query("DELETE FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3")
            .bind(user_id)
            .bind(gp)
            .bind(ver)
            .execute(&mut *tx)
            .await?;

        let next: Option<(i32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as(
                "SELECT l.lap_ms, l.pedal_mode, l.tc_mode, l.tc_strength, l.abs_mode, l.abs_strength \
                 FROM laps l \
                 WHERE l.user_id=$1 AND l.gp_index=$2 AND l.version_code=$3 \
                   AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h \
                                   WHERE h.user_id=l.user_id AND h.lap_id=l.id) \
                 ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
            )
            .bind(user_id)
            .bind(gp)
            .bind(ver)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((ms, pedal, tcm, tcs, absm, abss)) = next {
            let f = laps::flags_of(
                pedal.as_deref(),
                tcm.as_deref(),
                tcs.as_deref(),
                absm.as_deref(),
                abss.as_deref(),
            );
            sqlx::query(
                "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms, \
                                        pedal_linear, tc_off, abs_off, abs_low) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            )
            .bind(user_id)
            .bind(gp)
            .bind(ver)
            .bind(ms)
            .bind(f.pedal_linear)
            .bind(f.tc_off)
            .bind(f.abs_off)
            .bind(f.abs_low)
            .execute(&mut *tx)
            .await?;
        }
        // next 为 None = 该维度屏蔽后再无可用圈 → best_laps 保持已删除状态。
    }

    // 5) 重算 records：被屏蔽的圈可能持有全服纪录，必须让位给下一个最快圈。
    //    口径与 admin::recalc_dims 相同，额外排除所有被屏蔽的 lap_id。
    let mut affected_gps: Vec<i16> = dims.iter().map(|(gp, _)| *gp).collect();
    affected_gps.sort_unstable();
    affected_gps.dedup();

    for gp in &affected_gps {
        let va: Option<(i32, Uuid)> = sqlx::query_as(
            "SELECT l.lap_ms, l.user_id FROM laps l WHERE l.gp_index=$1 \
               AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h WHERE h.lap_id=l.id) \
             ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
        )
        .bind(gp)
        .fetch_optional(&mut *tx)
        .await?;
        apply_record(&mut tx, *gp, "alltime", 0, va).await?;

        // 版本维度：该赛道下所有有圈的版本都要重算（被屏蔽圈的版本不一定是唯一）。
        let versions: Vec<i32> = sqlx::query_scalar(
            "SELECT DISTINCT version_code FROM laps WHERE gp_index=$1",
        )
        .bind(gp)
        .fetch_all(&mut *tx)
        .await?;
        for ver in versions {
            let vb: Option<(i32, Uuid)> = sqlx::query_as(
                "SELECT l.lap_ms, l.user_id FROM laps l \
                 WHERE l.gp_index=$1 AND l.version_code=$2 \
                   AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h WHERE h.lap_id=l.id) \
                 ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
            )
            .bind(gp)
            .bind(ver)
            .fetch_optional(&mut *tx)
            .await?;
            apply_record(&mut tx, *gp, "version", ver, vb).await?;
        }
    }

    // 6) 落标记（决定后续请求直接返回"已屏蔽过"）
    sqlx::query("UPDATE users SET hidden_missing_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    tracing::info!(
        "user {user_id} hid {} missing-config laps across {} dimensions",
        missing.len(),
        dims.len()
    );
    Ok(Json(HideMissingResp {
        hidden: missing.len() as i64,
    }))
}

/// 与 `admin::apply_record` 同语义（那边是私有的，跨模块无法复用）。
///
/// ⚠️ 两处必须同步：records 的 upsert/delete 语义一旦分叉，管理端重算与
///    用户屏蔽会产出不同的全服纪录表。
async fn apply_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    gp: i16,
    kind: &str,
    vc: i32,
    best: Option<(i32, Uuid)>,
) -> Result<(), ApiError> {
    match best {
        None => {
            sqlx::query("DELETE FROM records WHERE gp_index=$1 AND kind=$2 AND version_code=$3")
                .bind(gp)
                .bind(kind)
                .bind(vc)
                .execute(&mut **tx)
                .await?;
        }
        Some((ms, uid)) => {
            sqlx::query(
                "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,now()) \
                 ON CONFLICT (gp_index, kind, version_code) \
                 DO UPDATE SET lap_ms=EXCLUDED.lap_ms, user_id=EXCLUDED.user_id, updated_at=now()",
            )
            .bind(gp)
            .bind(kind)
            .bind(vc)
            .bind(ms)
            .bind(uid)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me/account", axum::routing::get(account_state))
        .route("/me/username", put(change_username))
        .route("/me/hide-missing", post(hide_missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(days_ago: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        Some(chrono::Utc::now() - chrono::Duration::days(days_ago))
    }

    /// 从未操作 → 0（不是"剩余 30 天"）：NULL 表示从没换过，必须立刻可换。
    #[test]
    fn cooldown_none_is_zero() {
        assert_eq!(cooldown_remaining(None), 0);
    }

    /// 刚好 30 天前 → 0（边界是"满 30 天即可再操作"，不是"超过 30 天"）。
    #[test]
    fn cooldown_exactly_30_days_is_zero() {
        assert_eq!(cooldown_remaining(t(30)), 0);
    }

    /// 29 天前 → 约 1 天剩余，且**向上取整**：剩 0.4 秒要报 1 秒，
    /// 否则客户端显示"0 秒后"却仍被服务端拒绝。
    #[test]
    fn cooldown_rounds_up() {
        let r = cooldown_remaining(t(29));
        assert!(r > 0 && r <= 86400, "29 天前应剩 ≤1 天，实得 {r}");
        // 剩余 0.5 秒（30 天差 0.5 秒）→ 必须报 1，不能报 0
        let almost = Some(chrono::Utc::now()
            - chrono::Duration::days(COOLDOWN_DAYS)
            + chrono::Duration::milliseconds(500));
        assert_eq!(cooldown_remaining(almost), 1, "亚秒级剩余必须向上取整");
    }

    /// 未来时间戳（时钟回拨/数据异常）→ 0，不能让用户被永久锁死。
    #[test]
    fn cooldown_future_timestamp_is_zero() {
        assert_eq!(cooldown_remaining(t(-5)), 0);
    }
}
