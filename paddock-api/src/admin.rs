//! /admin —— Web 管理端（服务端渲染，askama 模板 + 少量原生 JS）。
//! S1 范围：登录、代绑 openid（bot 未上线的注册过渡桥）、用户列表、成绩管理（删除+重算）。
//! 认证：cookie 会话（admin_sessions 表），ACCOUNT/密码在首次启动时从环境变量播种。

use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{api::laps::track_display_name, state::AppState as App};

// ---------- 模板 ----------

#[derive(Template)]
#[template(path = "admin_login.html")]
struct LoginTemplate {
    error: String,
}

#[derive(Template)]
#[template(path = "admin_base.html")]
struct DashTemplate {
    title: &'static str,
    active: &'static str,
    content: String,
    admin_user: String,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct UsersTemplate {
    users: Vec<UserRow>,
}

struct UserRow {
    id: String,
    username: String,
    member_openid: String,
    reg_seq: i64,
    created: String,
    best_count: i64,
}

#[derive(Template)]
#[template(path = "admin_pending.html")]
struct PendingTemplate {
    rows: Vec<PendingRow>,
    notice: String,
}

struct PendingRow {
    reg_code: String,
    username: String,
    member_openid: String,
    status: String,
    expires: String,
}

#[derive(Template)]
#[template(path = "admin_laps.html")]
struct LapsTemplate {
    rows: Vec<LapRow>,
    notice: String,
}

struct LapRow {
    id: String,
    username: String,
    track: String,
    version_code: i32,
    lap_display: String,
    created: String,
}

// ---------- 认证 ----------

const COOKIE: &str = "paddock_admin";

async fn require_admin(state: &App, headers: &HeaderMap) -> Option<String> {
    let token = headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)?
        .1
        .to_string();
    let hash = crate::auth::sha256_hex(&token);
    let exp: Option<(chrono::DateTime<Utc>,)> =
        sqlx::query_as("SELECT expires_at FROM admin_sessions WHERE token_hash = $1")
            .bind(&hash)
            .fetch_optional(&state.pool)
            .await
            .ok()?;
    let exp = exp?.0;
    (exp > Utc::now()).then_some(hash)
}

fn html_res(status: StatusCode, s: String) -> Response {
    (status, Html(s)).into_response()
}

async fn ensure_admin_seeded(pool: &PgPool) -> anyhow::Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admins")
        .fetch_one(pool)
        .await?;
    if count == 0 {
        let username = std::env::var("PADDOCK_ADMIN_USER").unwrap_or_else(|_| "admin".into());
        let password = std::env::var("PADDOCK_ADMIN_PASS")
            .expect("首次启动需设 PADDOCK_ADMIN_USER/PADDOCK_ADMIN_PASS 播种管理员（此后可改）");
        let hash = crate::auth::hash_password(&password)?;
        sqlx::query("INSERT INTO admins (id, username, pass_hash) VALUES ($1,$2,$3)")
            .bind(Uuid::now_v7())
            .bind(&username)
            .bind(&hash)
            .execute(pool)
            .await?;
        tracing::info!("已播种管理员账号: {username}");
    }
    Ok(())
}

// ---------- 页面与动作 ----------

async fn login_page(State(state): State<App>) -> Response {
    ensure_admin_seeded(&state.pool).await.ok();
    let t = LoginTemplate {
        error: String::new(),
    };
    html_res(StatusCode::OK, t.render().unwrap_or_default())
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_submit(State(state): State<App>, Form(f): Form<LoginForm>) -> Response {
    let row: Option<(String,)> = sqlx::query_as("SELECT pass_hash FROM admins WHERE username = $1")
        .bind(&f.username)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let ok = row
        .map(|(h,)| crate::auth::verify_password(&f.password, &h))
        .unwrap_or(false);
    if !ok {
        let t = LoginTemplate {
            error: "用户名或密码错误".into(),
        };
        return html_res(StatusCode::UNAUTHORIZED, t.render().unwrap());
    }
    let (raw, hash) = crate::auth::issue_token().unwrap();
    sqlx::query("INSERT INTO admin_sessions (token_hash, expires_at) VALUES ($1,$2)")
        .bind(&hash)
        .bind(Utc::now() + Duration::hours(12))
        .execute(&state.pool)
        .await
        .ok();
    let mut res = Redirect::to("/admin/users").into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        format!("{COOKIE}={raw}; Path=/admin; HttpOnly; SameSite=Lax; Max-Age=43200")
            .parse()
            .unwrap(),
    );
    res
}

async fn logout(State(state): State<App>, headers: HeaderMap) -> Response {
    if let Some(hash) = require_admin(&state, &headers).await {
        sqlx::query("DELETE FROM admin_sessions WHERE token_hash = $1")
            .bind(&hash)
            .execute(&state.pool)
            .await
            .ok();
    }
    let mut res = Redirect::to("/admin").into_response();
    res.headers_mut().append(
        header::SET_COOKIE,
        format!("{COOKIE}=; Path=/admin; Max-Age=0")
            .parse()
            .unwrap(),
    );
    res
}

async fn users_page(State(state): State<App>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.member_openid, u.reg_seq, u.created_at, \
         (SELECT count(*) FROM best_laps b WHERE b.user_id = u.id) \
         FROM users u ORDER BY u.reg_seq ASC LIMIT 500",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(id, username, openid, seq, created, cnt): (
            Uuid,
            String,
            Option<String>,
            i64,
            chrono::DateTime<Utc>,
            i64,
        )| UserRow {
            id: id.to_string(),
            username,
            member_openid: openid.unwrap_or_default(),
            reg_seq: seq,
            created: created.format("%Y-%m-%d").to_string(),
            best_count: cnt,
        },
    )
    .collect();
    let body = UsersTemplate { users: rows }.render().unwrap();
    let t = DashTemplate {
        title: "用户管理",
        active: "users",
        content: body,
        admin_user: "admin".into(),
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

async fn pending_page(State(state): State<App>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    render_pending(&state, String::new()).await
}

async fn render_pending(state: &App, notice: String) -> Response {
    let rows: Vec<PendingRow> = sqlx::query_as(
        "SELECT reg_code, username, member_openid, status, expires_at FROM pending_regs \
         WHERE expires_at > now() ORDER BY expires_at DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(code, name, openid, status, exp): (
            String,
            String,
            Option<String>,
            String,
            chrono::DateTime<Utc>,
        )| PendingRow {
            reg_code: code,
            username: name,
            member_openid: openid.unwrap_or_default(),
            status,
            expires: exp.format("%H:%M:%S").to_string(),
        },
    )
    .collect();
    let body = PendingTemplate { rows, notice }.render().unwrap();
    let t = DashTemplate {
        title: "注册会话 · 代绑",
        active: "pending",
        content: body,
        admin_user: "admin".into(),
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

#[derive(Deserialize)]
struct BindForm {
    reg_code: String,
    member_openid: String,
}

/// 代绑：bot 未上线期间，管理员把群里发码用户的 member_openid 手工绑入会话
/// （openid 从 QQ 开放平台"消息与事件"日志或 bot 调试日志复制）。
async fn bind_submit(
    State(state): State<App>,
    headers: HeaderMap,
    Form(f): Form<BindForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    let n = sqlx::query("UPDATE pending_regs SET member_openid=$2, status='verified' WHERE reg_code=$1 AND status='pending' AND expires_at>now()")
        .bind(&f.reg_code)
        .bind(&f.member_openid)
        .execute(&state.pool)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    audit(
        &state.pool,
        "bind_openid",
        serde_json::json!({"reg_code": f.reg_code, "rows": n}),
    )
    .await;
    let notice = if n > 0 {
        format!("已代绑 {}（状态 → verified）", f.reg_code)
    } else {
        "码不存在/已过期/已绑定".into()
    };
    render_pending(&state, notice).await
}

#[derive(Deserialize)]
struct LapsQuery {
    #[serde(default)]
    q: String,
}

async fn laps_page(
    State(state): State<App>,
    headers: HeaderMap,
    Query(q): Query<LapsQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    render_laps(&state, &q.q, String::new()).await
}

async fn render_laps(state: &App, qname: &str, notice: String) -> Response {
    let like = format!("%{qname}%");
    let rows: Vec<LapRow> = sqlx::query_as(
        "SELECT l.id, u.username, l.gp_index, l.version_code, l.lap_ms, l.created_at \
         FROM laps l JOIN users u ON u.id=l.user_id \
         WHERE u.username LIKE $1 ORDER BY l.created_at DESC LIMIT 300",
    )
    .bind(&like)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(id, name, gp, ver, ms, created): (Uuid, String, i16, i32, i32, chrono::DateTime<Utc>)| {
            LapRow {
                id: id.to_string(),
                username: name,
                track: format!("{}({})", track_display_name(gp as i16), gp),
                version_code: ver,
                lap_display: crate::api::leaderboard::format_lap_ms(ms),
                created: created.format("%m-%d %H:%M:%S").to_string(),
            }
        },
    )
    .collect();
    let body = LapsTemplate { rows, notice }.render().unwrap();
    let t = DashTemplate {
        title: "成绩管理",
        active: "laps",
        content: body,
        admin_user: "admin".into(),
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

/// 删除单条有效圈记录。为保持"防伪全放行 + 事后删"定案：
/// 删除后必须在同事务里重算 best_laps/records（从 laps 全量留档回放）。
async fn delete_lap(
    State(state): State<App>,
    headers: HeaderMap,
    Path(lap_id): Path<Uuid>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let ok = recalc_delete(&state.pool, lap_id, &admin).await.is_ok();
    let notice = if ok {
        "已删除并重算".into()
    } else {
        "删除失败".into()
    };
    render_laps(&state, "", notice).await
}

/// 删除该条圈 → 若它当时是某 best/record 的来源，从 laps 回放重算。小数据量直接全量重放该 (user,gp,version)。
async fn recalc_delete(pool: &PgPool, lap_id: Uuid, admin: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let target: Option<(Uuid, i16, i32)> =
        sqlx::query_as("DELETE FROM laps WHERE id=$1 RETURNING user_id, gp_index, version_code")
            .bind(lap_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((uid, gp, ver)) = target else {
        return Ok(());
    };
    // 重算 best_laps
    let ub: Option<i32> = sqlx::query_scalar(
        "SELECT min(lap_ms) FROM laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
    )
    .bind(uid)
    .bind(gp)
    .bind(ver)
    .fetch_one(&mut *tx)
    .await?;
    match ub {
        Some(ms) => {
            sqlx::query(
                "UPDATE best_laps SET lap_ms=$4, updated_at=now() WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
            ).bind(uid).bind(gp).bind(ver).bind(ms).execute(&mut *tx).await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
            )
            .bind(uid)
            .bind(gp)
            .bind(ver)
            .execute(&mut *tx)
            .await?;
        }
    }
    // 重算纪录行。持有者 = 拥有最快圈的用户（DISTINCT ON 取该圈主人，非 min(user_id)）。
    let vb: Option<(i32, Uuid)> = sqlx::query_as(
        "SELECT lap_ms, user_id FROM laps WHERE gp_index=$1 AND version_code=$2 \
         ORDER BY lap_ms ASC, created_at ASC LIMIT 1",
    )
    .bind(gp)
    .bind(ver)
    .fetch_optional(&mut *tx)
    .await?;
    let va: Option<(i32, Uuid)> = sqlx::query_as(
        "SELECT lap_ms, user_id FROM laps WHERE gp_index=$1 \
         ORDER BY lap_ms ASC, created_at ASC LIMIT 1",
    )
    .bind(gp)
    .fetch_optional(&mut *tx)
    .await?;
    apply_record(&mut tx, gp, "alltime", 0, va).await?;
    apply_record(&mut tx, gp, "version", ver, vb).await?;
    sqlx::query("DELETE FROM admin_sessions WHERE expires_at < now()")
        .execute(&mut *tx)
        .await
        .ok();
    sqlx::query(
        "INSERT INTO admin_audit (admin_user, action, detail) VALUES ($1,'delete_lap', $2)",
    )
    .bind(admin)
    .bind(serde_json::json!({"lap_id": lap_id, "gp": gp, "ver": ver}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    gp: i16,
    kind: &str,
    vc: i32,
    best: Option<(i32, Uuid)>,
) -> anyhow::Result<()> {
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

/// 从 cookie 会话解析管理员用户名（审计用）。简化版：只验证有效性。
async fn admin_name(state: &App, headers: &HeaderMap) -> Option<String> {
    require_admin(state, headers)
        .await
        .map(|_| "admin".to_string())
}

async fn audit(pool: &PgPool, action: &str, detail: serde_json::Value) {
    sqlx::query("INSERT INTO admin_audit (admin_user, action, detail) VALUES ('admin',$1,$2)")
        .bind(action)
        .bind(detail)
        .execute(pool)
        .await
        .ok();
}

pub fn router() -> Router<App> {
    Router::new()
        .route("/", get(login_page))
        .route("/login", post(login_submit))
        .route("/logout", post(logout))
        .route("/users", get(users_page))
        .route("/pending", get(pending_page))
        .route("/pending/bind", post(bind_submit))
        .route("/laps", get(laps_page))
        .route("/laps/{id}/delete", post(delete_lap))
}
