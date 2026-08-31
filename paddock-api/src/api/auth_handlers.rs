//! 认证端点：注册申请 / 注册校验 / 登录 / 一次性码重置。
//! 注册闭环（定案）：模块申请码 → 用户在 CAMDA 群发"申请围场通行证#XXXXXXXX"
//! → bot 用码定位 pending 会话绑定 member_openid → 模块 verify 建号并登录。
//! bot 未上线（S1）期间由管理端代绑 member_openid。

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
    auth::validate_username(&req.username).map_err(|e| ApiError::bad_request(e))?;
    if user_exists(&state.pool, &req.username).await? {
        return Err(ApiError::conflict("用户名已存在（不允许重复注册）"));
    }
    // 同名在途会话检查：pending 里锁存了 username，防止同名并发注册
    let pending_same_name: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pending_regs WHERE username = $1 AND expires_at > now()",
    )
    .bind(&req.username)
    .fetch_one(&state.pool)
    .await?;
    if pending_same_name > 0 {
        return Err(ApiError::conflict("该用户名已有注册申请在途，请等待其完成或过期后再试"));
    }
    let reg_code = auth::gen_reg_code();
    let expires = Utc::now() + Duration::minutes(auth::REG_CODE_TTL_MINUTES);
    sqlx::query("INSERT INTO pending_regs (reg_code, username, expires_at) VALUES ($1, $2, $3)")
        .bind(&reg_code)
        .bind(&req.username)
        .bind(expires)
        .execute(&state.pool)
        .await?;
    Ok(Json(RegisterRequestResp {
        message_hint: format!("申请围场通行证#{reg_code}"),
        reg_code,
    }))
}

#[derive(Deserialize)]
pub struct RegisterVerify {
    pub reg_code: String,
    pub username: String,
    pub password: String,
}

/// 校验通过后建号。要求 pending 会话已被 bot（或管理端）绑上 member_openid。
pub async fn register_verify(
    State(state): State<AppState>,
    Json(req): Json<RegisterVerify>,
) -> Result<(axum::http::StatusCode, Json<LoginResp>), ApiError> {
    auth::validate_username(&req.username).map_err(ApiError::bad_request)?;
    auth::validate_password(&req.password).map_err(ApiError::bad_request)?;

    let mut tx = state.pool.begin().await?;
    let row: Option<(Option<String>, String, String)> = sqlx::query_as(
        "DELETE FROM pending_regs WHERE reg_code = $1 AND expires_at > now() RETURNING member_openid, status, username",
    )
    .bind(&req.reg_code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((member_openid, status, locked_username)) = row else {
        return Err(ApiError::not_found("校验码无效或已过期，请重新申请"));
    };
    // username 必须与申请时锁存的一致（防拿别人的码换名建号）
    if req.username != locked_username {
        return Err(ApiError::bad_request("用户名与申请时不一致，请使用申请时的用户名"));
    }
    if status != "verified" || member_openid.is_none() {
        return Err(ApiError::bad_request(
            "该码尚未在 CAMDA 群完成校验，请先在群内发送校验消息",
        ));
    }
    let openid = member_openid.unwrap();
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE member_openid = $1")
        .bind(&openid)
        .fetch_one(&mut *tx)
        .await?
        > 0
    {
        return Err(ApiError::conflict("该 QQ 身份已绑定过账号，不允许重复注册"));
    }
    let user_id = Uuid::now_v7();
    let pass_hash = auth::hash_password(&req.password)?;
    let reg_seq: i64 = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&mut *tx)
        .await?
        + 1;
    sqlx::query(
        "INSERT INTO users (id, username, pass_hash, member_openid, reg_seq) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(user_id)
    .bind(&req.username)
    .bind(&pass_hash)
    .bind(&openid)
    .bind(reg_seq)
    .execute(&mut *tx)
    .await?;
    let (token, token_hash) = auth::issue_token()?;
    let expires = Utc::now() + Duration::days(auth::TOKEN_TTL_DAYS);
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1,$2,$3)")
        .bind(&token_hash)
        .bind(user_id)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(LoginResp {
            token,
            user_id,
            username: req.username,
            reg_seq,
        }),
    ))
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
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let row: Option<(Uuid, String, String, i64)> =
        sqlx::query_as("SELECT id, pass_hash, username, reg_seq FROM users WHERE username = $1")
            .bind(&req.username)
            .fetch_optional(&state.pool)
            .await?;
    let Some((user_id, pass_hash, username, reg_seq)) = row else {
        return Err(ApiError::unauthorized("用户名或密码错误"));
    };
    if !auth::verify_password(&req.password, &pass_hash) {
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
    Ok(Json(LoginResp {
        token,
        user_id,
        username,
        reg_seq,
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
        .route("/auth/register-verify", post(register_verify))
        .route("/auth/login", post(login))
        // TODO(S4): /auth/reset-by-code —— bot 一次性码重置密码
        .route("/health", get(|| async { "ok" }))
}
