//! 认证端点：注册申请 / 登录 / 一次性码重置。
//! 注册闭环（2026-09-01 定案 v2：bot 校验即建号）：模块申请（username+password，服务端
//! 哈希密码存 pending 会话）→ 用户在 CAMDA 群发"申请围场通行证#XXXXXXXX"
//! → bot 用码定位会话绑定 member_openid **并直接建号**（回复车手 ID）→ 用户回模块
//! 用同一账号密码直接登录。旧 register-verify 端点已删（bot 建号后无 verify 步骤）。
//! 2026-09-04 v3：车手 ID 改为建号时才发放（申请时发号会因 pending 作废烧号）；
//! register-request 对同名在途会话幂等恢复（密码一致返回原码，支持重弹申请指令）。

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{auth, state::AppState};

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct RegisterRequestResp {
    pub reg_code: String,
    /// 用户要发到 CAMDA 群的完整文案片段："申请围场通行证#XXXXXXXX"
    pub message_hint: String,
}

pub async fn register_request(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterRequestResp>, ApiError> {
    auth::validate_username(&req.username).map_err(ApiError::bad_request)?;
    auth::validate_password(&req.password).map_err(ApiError::bad_request)?;
    if user_exists(&state.pool, &req.username).await? {
        return Err(ApiError::conflict("用户名已存在（不允许重复注册）"));
    }
    // 幂等恢复（2026-09-04）：同名在途会话且请求密码与锁存哈希一致 → 返回原校验码。
    // 场景：用户拿到申请指令后没复制/弹窗被关，回模块重新点"注册"应重新弹出，
    // 而不是被"已有申请在途"挡死。密码不一致 → 409 拒绝（防用户名抢占攻击：
    // 若放行覆盖，攻击者可改受害者 pending 密码，再用自己群里发的码抢建该用户名）。
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT reg_code, pass_hash FROM pending_regs WHERE username = $1 AND expires_at > now()",
    )
    .bind(&req.username)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((reg_code, pass_hash)) = existing {
        if auth::verify_password(&req.password, &pass_hash) {
            crate::applog::log_event(
                &state.pool, "info", "auth", "register_request", &req.username,
                format!("在途会话幂等恢复（重弹申请指令，校验码 {reg_code}）"),
                serde_json::json!({}),
            );
            return Ok(Json(RegisterRequestResp {
                message_hint: format!("申请围场通行证#{reg_code}"),
                reg_code,
            }));
        }
        return Err(ApiError::conflict(
            "该用户名已有注册申请在途，且密码不一致；若是你本人的申请，请用原密码重试，否则请等待其过期（30 分钟）",
        ));
    }
    let reg_code = auth::gen_reg_code();
    let pass_hash = auth::hash_password(&req.password)?;
    let expires = Utc::now() + Duration::minutes(auth::REG_CODE_TTL_MINUTES);
    sqlx::query(
        "INSERT INTO pending_regs (reg_code, username, pass_hash, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&reg_code)
    .bind(&req.username)
    .bind(&pass_hash)
    .bind(expires)
    .execute(&state.pool)
    .await?;
    crate::applog::log_event(
        &state.pool, "info", "auth", "register_request", &req.username,
        format!("申请注册（校验码 {reg_code}）"),
        serde_json::json!({}),
    );
    Ok(Json(RegisterRequestResp {
        message_hint: format!("申请围场通行证#{reg_code}"),
        reg_code,
    }))
}

/// 车手 ID 建号时才发放，取**最小未被使用的正整数**（2026-09-04 v4，用户定案）：
/// 反复注册-放弃的用户不会被越推越后，弃号立即回收。并发注册可能同时选中同一号
/// ——reg_seq 唯一约束会让后者建号失败重试，竞态窗口极小，可接受。
/// 注意 PG 查询非事务性锁——高并发下靠唯一约束兜底，不额外加锁。
/// 公开给 qq_bot 调用；建号失败（openid 已绑号等）由调用方回滚并回复错误文案。
pub async fn create_user_from_pending(
    pool: &PgPool,
    reg_code: &str,
    member_openid: &str,
) -> Result<Uuid, (StatusCode, String)> {
    let mut tx = pool.begin().await.map_err(internal)?;
    let row: Option<(String, String)> = sqlx::query_as(
        "DELETE FROM pending_regs WHERE reg_code = $1 AND expires_at > now() \
         RETURNING username, pass_hash",
    )
    .bind(reg_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(internal)?;
    let Some((username, pass_hash)) = row else {
        return Err((StatusCode::NOT_FOUND, "校验码无效或已过期，请重新申请".into()));
    };
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE member_openid = $1")
        .bind(member_openid)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        > 0
    {
        return Err((
            StatusCode::CONFLICT,
            "该 QQ 身份已绑定过账号，不允许重复注册".into(),
        ));
    }
    // 最小空缺号：1..=max+1 中第一个未被占用的值（无用户时 = 1）
    let reg_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MIN(s.seq), 1) \
         FROM generate_series(1, (SELECT COALESCE(MAX(reg_seq), 0) + 1 FROM users)) AS s(seq) \
         WHERE s.seq NOT IN (SELECT reg_seq FROM users)",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    let user_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, pass_hash, member_openid, reg_seq) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(user_id)
    .bind(&username)
    .bind(&pass_hash)
    .bind(member_openid)
    .bind(reg_seq)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(user_id)
}

fn internal(e: sqlx::Error) -> (StatusCode, String) {
    tracing::error!("建号事务数据库错误: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "服务器开小差了，请稍后再试".into())
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResp {
    pub token: String,
    pub user_id: Uuid,
    pub username: String,
    pub reg_seq: i64,
    /// false = 尚未设置头像（注册后首次登录），模块引导上传
    pub has_avatar: bool,
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let row: Option<(Uuid, String, String, i64, bool)> = sqlx::query_as(
        "SELECT id, pass_hash, username, reg_seq, (avatar_key IS NOT NULL) FROM users WHERE username = $1",
    )
    .bind(&req.username)
    .fetch_optional(&state.pool)
    .await?;
    let Some((user_id, pass_hash, username, reg_seq, has_avatar)) = row else {
        crate::applog::log_event(
            &state.pool, "warn", "auth", "login_failed", &req.username,
            "登录失败（用户名不存在）", serde_json::json!({}),
        );
        return Err(ApiError::unauthorized("用户名或密码错误"));
    };
    if !auth::verify_password(&req.password, &pass_hash) {
        crate::applog::log_event(
            &state.pool, "warn", "auth", "login_failed", &username,
            "登录失败（密码错误）", serde_json::json!({}),
        );
        return Err(ApiError::unauthorized("用户名或密码错误"));
    }
    let (token, token_hash) = auth::issue_token()?;
    let expires = Utc::now() + Duration::days(auth::TOKEN_TTL_DAYS);
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1,$2,$3)")
        .bind(&token_hash)
        .bind(user_id)
        .bind(expires)
        .execute(&state.pool)
        .await?;
    crate::applog::log_event(
        &state.pool, "info", "auth", "login", &username,
        "登录成功", serde_json::json!({}),
    );
    Ok(Json(LoginResp {
        token,
        user_id,
        username,
        reg_seq,
        has_avatar,
    }))
}

// ---- 密码重置（S4）：群内 "重置密码 用户名" → bot 生成码回复 → 模块提交新密码 ----

/// bot 端：为用户生成一次性重置码（30 分钟时效）。由 webhook 处理器调用。
pub async fn create_reset_code(pool: &PgPool, username: &str) -> Result<String, ApiError> {
    let user_id: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = $1")
            .bind(username)
            .fetch_optional(pool)
            .await?;
    let Some((user_id,)) = user_id else {
        return Err(ApiError::not_found("用户名不存在"));
    };
    // 同一用户旧码先失效，保持"最后一码有效"的简单语义
    sqlx::query("DELETE FROM reset_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    let code = auth::gen_reg_code();
    let expires = Utc::now() + Duration::minutes(auth::RESET_CODE_TTL_MINUTES);
    sqlx::query("INSERT INTO reset_codes (code, user_id, expires_at) VALUES ($1,$2,$3)")
        .bind(&code)
        .bind(user_id)
        .bind(expires)
        .execute(pool)
        .await?;
    Ok(code)
}

#[derive(Deserialize)]
pub struct ResetByCode {
    pub reset_code: String,
    pub new_password: String,
}

/// POST /v1/auth/reset-by-code：模块端用码换新密码。旧 sessions 全部失效。
pub async fn reset_by_code(
    State(state): State<AppState>,
    Json(req): Json<ResetByCode>,
) -> Result<axum::http::StatusCode, ApiError> {
    auth::validate_password(&req.new_password).map_err(ApiError::bad_request)?;
    let mut tx = state.pool.begin().await?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "DELETE FROM reset_codes WHERE code = $1 AND expires_at > now() RETURNING user_id",
    )
    .bind(&req.reset_code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id,)) = row else {
        return Err(ApiError::not_found("重置码无效或已过期，请在群内重新申请"));
    };
    let pass_hash = auth::hash_password(&req.new_password)?;
    sqlx::query("UPDATE users SET pass_hash = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&pass_hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    crate::applog::log_event_tx(
        &mut tx, "info", "auth", "reset_by_code", &req.reset_code,
        "密码重置成功（旧登录态全部失效）", serde_json::json!({}),
    ).await;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---- 个人资料（GET /v1/me）：模块重进后恢复登录态 + 计时赛积分 ----

#[derive(Serialize)]
pub struct MeResp {
    pub user_id: Uuid,
    pub username: String,
    pub reg_seq: i64,
    pub has_avatar: bool,
    /// 计时赛总积分（与总榜同口径：跨版本 best-of-best 每赛道积分求和；无成绩=0）
    pub total_points: i64,
}

/// GET /v1/me（Bearer）。模块进程重进时经此恢复 username/reg_seq/积分——
/// token 只证明身份，profile 必须另拉；401 = token 失效（模块端自动登出）。
pub async fn me(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<MeResp>, ApiError> {
    let user_id = crate::api::laps::authenticate(&state, &headers).await?;
    let row: Option<(String, i64, bool)> = sqlx::query_as(
        "SELECT username, reg_seq, (avatar_key IS NOT NULL) FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((username, reg_seq, has_avatar)) = row else {
        return Err(ApiError::unauthorized("账号不存在，请重新登录"));
    };
    // 积分口径与 leaderboard::points_board 总榜分支一致（版本=独立赛季累加 + v40 线性公式）。
    // ⚠️ 窗口函数必须基于全量 best_laps 计算再过滤用户——若把 WHERE user_id 放进
    // per_track CTE，n_in_track 恒为 1，每人每赛道都走 100 分特判，与总榜口径脱节。
    let total_points: i64 = sqlx::query_scalar(
        r#"WITH per_track AS (
             SELECT user_id, version_code, gp_index, lap_ms,
                    rank() OVER (PARTITION BY version_code, gp_index ORDER BY lap_ms ASC) AS rank_in_track,
                    count(*) OVER (PARTITION BY version_code, gp_index) AS n_in_track
             FROM best_laps
           )
           SELECT coalesce(sum(CASE WHEN n_in_track = 1 THEN 100
                                    ELSE round((n_in_track + 1 - rank_in_track)::numeric * 100 / n_in_track)
                               END), 0)::bigint
           FROM per_track WHERE user_id = $1"#,
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(MeResp {
        user_id,
        username,
        reg_seq,
        has_avatar,
        total_points,
    }))
}

async fn user_exists(pool: &PgPool, username: &str) -> Result<bool, ApiError> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE username = $1")
            .bind(username)
            .fetch_one(pool)
            .await?
            > 0,
    )
}

// ---- 共享错误类型（auth 与 laps 共用；HTTP 状态+明确文案） ----

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0}")]
    NotFound(&'static str),
    #[error("{0}")]
    Unauthorized(&'static str),
}

impl ApiError {
    pub fn bad_request(s: &'static str) -> Self {
        Self::BadRequest(s)
    }
    pub fn conflict(s: &'static str) -> Self {
        Self::Conflict(s)
    }
    pub fn not_found(s: &'static str) -> Self {
        Self::NotFound(s)
    }
    pub fn unauthorized(s: &'static str) -> Self {
        Self::Unauthorized(s)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        };
        (
            status,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!("db error: {e}");
        ApiError::BadRequest("服务器内部错误")
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("internal error: {e}");
        ApiError::BadRequest("服务器内部错误")
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/register-request", post(register_request))
        .route("/auth/login", post(login))
        .route("/auth/reset-by-code", post(reset_by_code))
        .route("/me", get(me))
        .route("/health", get(|| async { Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })) }))
}
