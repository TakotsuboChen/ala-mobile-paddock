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
use chrono::{Duration, TimeZone, Utc};

/// 显示层统一用北京时间（TIMESTAMPTZ 存 UTC 不动，展示转 +8）。
fn bj_time(t: chrono::DateTime<Utc>) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::FixedOffset::east_opt(8 * 3600).unwrap().from_utc_datetime(&t.naive_utc())
}
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
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct UsersTemplate {
    users: Vec<UserRow>,
    notice: String,
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
    lap_ms_raw: i32,
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
            created: bj_time(created).format("%Y-%m-%d").to_string(),
            best_count: cnt,
        },
    )
    .collect();
    let body = UsersTemplate { users: rows, notice: String::new() }.render().unwrap();
    let t = DashTemplate {
        title: "用户管理",
        active: "users",
        content: body,
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
            expires: bj_time(exp).format("%m-%d %H:%M:%S").to_string(),
        },
    )
    .collect();
    let body = PendingTemplate { rows, notice }.render().unwrap();
    let t = DashTemplate {
        title: "注册会话 · 代绑",
        active: "pending",
        content: body,
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
                lap_ms_raw: ms,
                created: bj_time(created).format("%m-%d %H:%M:%S").to_string(),
            }
        },
    )
    .collect();
    let body = LapsTemplate { rows, notice }.render().unwrap();
    let t = DashTemplate {
        title: "成绩管理",
        active: "laps",
        content: body,
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

#[derive(Deserialize)]
struct EditLapForm {
    lap_ms: String,
}

/// 编辑圈时（改 lap_ms）→ 同事务重算 best_laps/records。输入是毫秒文本
/// （页面展示 1:15.705 格式，编辑按毫秒数字改）。
async fn edit_lap(
    State(state): State<App>,
    headers: HeaderMap,
    Path(lap_id): Path<Uuid>,
    Form(f): Form<EditLapForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let notice = match f.lap_ms.trim().parse::<i32>() {
        Err(_) => "圈时必须是毫秒整数（如 75705）".to_string(),
        Ok(ms) => match recalc_edit(&state.pool, lap_id, ms, &admin).await {
            Ok(true) => format!("圈时已改为 {ms}ms 并重算"),
            Ok(false) => "目标不存在或圈时超出合法范围（0 < ms ≤ 3600000）".to_string(),
            Err(_) => "编辑失败".to_string(),
        },
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
    recalc_dims(&mut tx, uid, gp, ver).await?;
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

/// 编辑圈时（改 lap_ms）后重算同维度。返回 Some(维度) 表示目标存在。
async fn recalc_edit(pool: &PgPool, lap_id: Uuid, new_ms: i32, admin: &str) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    let target: Option<(Uuid, i16, i32)> =
        sqlx::query_as("SELECT user_id, gp_index, version_code FROM laps WHERE id=$1")
            .bind(lap_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((uid, gp, ver)) = target else {
        return Ok(false);
    };
    // 校验合法圈时（防手滑输入 0 或负数把榜单刷穿）
    if new_ms <= 0 || new_ms > 3_600_000 {
        return Ok(false);
    }
    sqlx::query("UPDATE laps SET lap_ms=$2 WHERE id=$1")
        .bind(lap_id)
        .bind(new_ms)
        .execute(&mut *tx)
        .await?;
    recalc_dims(&mut tx, uid, gp, ver).await?;
    sqlx::query("DELETE FROM admin_sessions WHERE expires_at < now()")
        .execute(&mut *tx)
        .await
        .ok();
    sqlx::query(
        "INSERT INTO admin_audit (admin_user, action, detail) VALUES ($1,'edit_lap', $2)",
    )
    .bind(admin)
    .bind(serde_json::json!({"lap_id": lap_id, "new_ms": new_ms}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// 重算 (user, gp, version) 维度的 best_laps + 全服 records（持有者=最快圈主人）。
async fn recalc_dims(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    uid: Uuid,
    gp: i16,
    ver: i32,
) -> anyhow::Result<()> {
    // 重算 best_laps
    let ub: Option<i32> = sqlx::query_scalar(
        "SELECT min(lap_ms) FROM laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
    )
    .bind(uid)
    .bind(gp)
    .bind(ver)
    .fetch_one(&mut **tx)
    .await?;
    match ub {
        Some(ms) => {
            sqlx::query(
                "UPDATE best_laps SET lap_ms=$4, updated_at=now() WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
            ).bind(uid).bind(gp).bind(ver).bind(ms).execute(&mut **tx).await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
            )
            .bind(uid)
            .bind(gp)
            .bind(ver)
            .execute(&mut **tx)
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
    .fetch_optional(&mut **tx)
    .await?;
    let va: Option<(i32, Uuid)> = sqlx::query_as(
        "SELECT lap_ms, user_id FROM laps WHERE gp_index=$1 \
         ORDER BY lap_ms ASC, created_at ASC LIMIT 1",
    )
    .bind(gp)
    .fetch_optional(&mut **tx)
    .await?;
    apply_record(tx, gp, "alltime", 0, va).await?;
    apply_record(tx, gp, "version", ver, vb).await?;
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

// ---------- 设置页（S4：改密码 + bot 配置 + 用户密码重置） ----------

#[derive(Template)]
#[template(path = "admin_settings.html")]
struct SettingsTemplate {
    bot_app_id: String,
    bot_secret_set: bool,
    notice: String,
}

async fn settings_page(State(state): State<App>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    render_settings(&state, String::new()).await
}

async fn render_settings(state: &App, notice: String) -> Response {
    let app_id = crate::qq_bot::get_cfg(&state.pool, "bot_app_id")
        .await
        .unwrap_or_default();
    let secret_set = crate::qq_bot::get_cfg(&state.pool, "bot_app_secret")
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let body = SettingsTemplate {
        bot_app_id: app_id,
        bot_secret_set: secret_set,
        notice,
    }
    .render()
    .unwrap();
    let t = DashTemplate {
        title: "设置",
        active: "settings",
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

#[derive(Deserialize)]
struct ChangePwForm {
    old_password: String,
    new_password: String,
    new_password2: String,
}

async fn change_password(
    State(state): State<App>,
    headers: HeaderMap,
    Form(f): Form<ChangePwForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    if f.new_password != f.new_password2 {
        return render_settings(&state, "两次输入的新密码不一致".into()).await;
    }
    if crate::auth::validate_password(&f.new_password).is_err() {
        return render_settings(
            &state,
            "新密码至少 8 位，且须同时包含数字和字母".into(),
        )
        .await;
    }
    let row: Option<(String,)> =
        sqlx::query_as("SELECT pass_hash FROM admins WHERE username = $1")
            .bind(&admin)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);
    let ok = row
        .map(|(h,)| crate::auth::verify_password(&f.old_password, &h))
        .unwrap_or(false);
    if !ok {
        return render_settings(&state, "当前密码错误".into()).await;
    }
    let new_hash = match crate::auth::hash_password(&f.new_password) {
        Ok(h) => h,
        Err(_) => return render_settings(&state, "密码哈希失败".into()).await,
    };
    sqlx::query("UPDATE admins SET pass_hash = $2 WHERE username = $1")
        .bind(&admin)
        .bind(&new_hash)
        .execute(&state.pool)
        .await
        .ok();
    // 改密后作废全部管理端会话，用新密码重新登录
    sqlx::query("DELETE FROM admin_sessions")
        .execute(&state.pool)
        .await
        .ok();
    audit(&state.pool, "change_admin_password", serde_json::json!({})).await;
    Redirect::to("/admin").into_response()
}

#[derive(Deserialize)]
struct ChangeUsernameForm {
    new_username: String,
    password: String,
}

/// 修改管理员用户名：需验证当前密码防会话劫持。改完作废全部管理端会话。
async fn change_username(
    State(state): State<App>,
    headers: HeaderMap,
    Form(f): Form<ChangeUsernameForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let name = f.new_username.trim();
    if name.is_empty() || name.len() > 32 {
        return render_settings(&state, "用户名 1–32 字符".into()).await;
    }
    let row: Option<(String,)> =
        sqlx::query_as("SELECT pass_hash FROM admins WHERE username = $1")
            .bind(&admin)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);
    let ok = row
        .map(|(h,)| crate::auth::verify_password(&f.password, &h))
        .unwrap_or(false);
    if !ok {
        return render_settings(&state, "密码错误".into()).await;
    }
    let taken: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admins WHERE username = $1 AND username <> $2")
            .bind(name)
            .bind(&admin)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(1);
    if taken > 0 {
        return render_settings(&state, "该用户名已被占用".into()).await;
    }
    sqlx::query("UPDATE admins SET username = $2 WHERE username = $1")
        .bind(&admin)
        .bind(name)
        .execute(&state.pool)
        .await
        .ok();
    sqlx::query("DELETE FROM admin_sessions")
        .execute(&state.pool)
        .await
        .ok();
    audit(&state.pool, "change_admin_username", serde_json::json!({"new": name})).await;
    Redirect::to("/admin").into_response()
}

#[derive(Deserialize)]
struct BotConfigForm {
    bot_app_id: String,
    bot_app_secret: String,
}

/// bot 凭据保存。secret 留空 = 保留原值（不回显，防泄露）。
async fn save_bot_config(
    State(state): State<App>,
    headers: HeaderMap,
    Form(f): Form<BotConfigForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    let notice = match async {
        crate::qq_bot::set_cfg(&state.pool, "bot_app_id", f.bot_app_id.trim()).await?;
        if !f.bot_app_secret.trim().is_empty() {
            crate::qq_bot::set_cfg(&state.pool, "bot_app_secret", f.bot_app_secret.trim()).await?;
        }
        anyhow::Ok(())
    }
    .await
    {
        Ok(()) => {
            audit(
                &state.pool,
                "save_bot_config",
                serde_json::json!({"app_id": f.bot_app_id.trim()}),
            )
            .await;
            "bot 配置已保存（即时生效，无需重启）。请回 QQ 开放平台重新触发回调验证".to_string()
        }
        Err(_) => "保存失败，请重试".to_string(),
    };
    render_settings(&state, notice).await
}

#[derive(Deserialize)]
struct AdminResetForm {
    new_password: String,
}

/// 人工重置密码（PADDOCK_PLAN §1"管理端同时留人工重置入口"）：直接设新密码，
/// 作废该用户全部模块会话。与 bot 码重置等效但绕过群流程。
async fn admin_reset_password(
    State(state): State<App>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Form(f): Form<AdminResetForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let notice = if crate::auth::validate_password(&f.new_password).is_err() {
        "密码至少 8 位，且须同时包含数字和字母".to_string()
    } else {
        match crate::auth::hash_password(&f.new_password) {
            Ok(hash) => {
                let mut tx = state.pool.begin().await.ok().unwrap();
                sqlx::query("UPDATE users SET pass_hash=$2 WHERE id=$1")
                    .bind(user_id)
                    .bind(&hash)
                    .execute(&mut *tx)
                    .await
                    .ok();
                sqlx::query("DELETE FROM sessions WHERE user_id=$1")
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await
                    .ok();
                sqlx::query(
                    "INSERT INTO admin_audit (admin_user, action, detail) VALUES ($1,'reset_password',$2)",
                )
                .bind(&admin)
                .bind(serde_json::json!({"user_id": user_id.to_string()}))
                .execute(&mut *tx)
                .await
                .ok();
                tx.commit().await.ok();
                "密码已重置（该用户所有登录已失效）".to_string()
            }
            Err(_) => "密码哈希失败".to_string(),
        }
    };
    render_users_with_notice(&state, notice).await
}

async fn render_users_with_notice(state: &App, notice: String) -> Response {
    // 与 users_page 相同的行渲染逻辑；notice 显示在用户列表页顶部
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
            created: bj_time(created).format("%Y-%m-%d").to_string(),
            best_count: cnt,
        },
    )
    .collect();
    let body = UsersTemplate { users: rows, notice }.render().unwrap();
    let t = DashTemplate {
        title: "用户管理",
        active: "users",
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

/// 编辑用户名（管理端；同 login 用户名规则）。改名后模块端需用新名登录。
async fn admin_rename_user(
    State(state): State<App>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Form(f): Form<AdminRenameForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let new_name = f.new_username.trim().to_string();
    let notice = if crate::auth::validate_username(&new_name).is_err() {
        "用户名须为 1–16 位中文/字母/数字，两侧禁空格".to_string()
    } else {
        let mut tx = match state.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => return render_users_with_notice(&state, "数据库连接失败".into()).await,
        };
        let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username=$1 AND id<>$2")
            .bind(&new_name)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(1); // 查询失败按"已占用"处理，拒绝改名
        if taken > 0 {
            "用户名已被占用".to_string()
        } else {
            let n = sqlx::query("UPDATE users SET username=$2 WHERE id=$1")
                .bind(user_id)
                .bind(&new_name)
                .execute(&mut *tx)
                .await
                .map(|r| r.rows_affected())
                .unwrap_or(0);
            if n > 0 {
                sqlx::query(
                    "INSERT INTO admin_audit (admin_user, action, detail) VALUES ($1,'rename_user',$2)",
                )
                .bind(&admin)
                .bind(serde_json::json!({"user_id": user_id.to_string(), "new_username": new_name}))
                .execute(&mut *tx)
                .await
                .ok();
                tx.commit().await.ok();
                format!("用户名已改为 {new_name}（该用户需用新名登录）")
            } else {
                "用户不存在".to_string()
            }
        }
    };
    render_users_with_notice(&state, notice).await
}

/// 删除用户（确认对话框输入用户名确认）。事务：
/// 1. 收集该用户有 laps 的全部 (gp, version) 维度；
/// 2. DELETE users（sessions/laps/best_laps 级联清；records.user_id 置 NULL）；
/// 3. 对每个维度重算 records（悬空的纪录行回放给剩余用户，无圈则删行）。
async fn admin_delete_user(
    State(state): State<App>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Form(f): Form<AdminDeleteForm>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return Redirect::to("/admin").into_response();
    };
    let notice = if f.confirm.trim() != f.username.trim() {
        "确认输入与用户名不一致，未删除".to_string()
    } else {
        match delete_user_recalc(&state.pool, user_id, &admin).await {
            Ok(name) => format!("已删除用户 {name}（成绩与登录态一并清除，纪录已重算）"),
            Err(e) => format!("删除失败：{e}"),
        }
    };
    render_users_with_notice(&state, notice).await
}

/// 删除用户 + 重算受影响 records。返回被删用户名。
async fn delete_user_recalc(pool: &PgPool, user_id: Uuid, admin: &str) -> anyhow::Result<String> {
    let mut tx = pool.begin().await?;
    let name: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id=$1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(name) = name else {
        anyhow::bail!("用户不存在");
    };
    // 该用户参与过的维度（其圈影响过 records 的 gp+version 全集）
    let dims: Vec<(i16, i32)> = sqlx::query_as(
        "SELECT DISTINCT gp_index, version_code FROM laps WHERE user_id=$1",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    // 删用户：sessions/laps/best_laps CASCADE 清；records.user_id SET NULL（下步重算覆盖）
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for (gp, ver) in &dims {
        let vb: Option<(i32, Uuid)> = sqlx::query_as(
            "SELECT lap_ms, user_id FROM laps WHERE gp_index=$1 AND version_code=$2 \
             ORDER BY lap_ms ASC, created_at ASC LIMIT 1",
        )
        .bind(*gp)
        .bind(*ver)
        .fetch_optional(&mut *tx)
        .await?;
        let va: Option<(i32, Uuid)> = sqlx::query_as(
            "SELECT lap_ms, user_id FROM laps WHERE gp_index=$1 \
             ORDER BY lap_ms ASC, created_at ASC LIMIT 1",
        )
        .bind(*gp)
        .fetch_optional(&mut *tx)
        .await?;
        apply_record(&mut tx, *gp, "alltime", 0, va).await?;
        apply_record(&mut tx, *gp, "version", *ver, vb).await?;
    }
    sqlx::query(
        "INSERT INTO admin_audit (admin_user, action, detail) VALUES ($1,'delete_user',$2)",
    )
    .bind(admin)
    .bind(serde_json::json!({"user_id": user_id.to_string(), "username": name}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(name)
}

#[derive(serde::Deserialize)]
struct AdminRenameForm {
    new_username: String,
}

#[derive(serde::Deserialize)]
struct AdminDeleteForm {
    /// 页面隐藏字段：当前用户名；确认框输入必须与其一致
    username: String,
    /// 管理员输入的确认文本
    confirm: String,
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
        .route("/laps/{id}/edit", post(edit_lap))
        .route("/settings", get(settings_page))
        .route("/settings/password", post(change_password))
        .route("/settings/username", post(change_username))
        .route("/settings/bot", post(save_bot_config))
        .route("/users/{id}/reset-password", post(admin_reset_password))
        .route("/users/{id}/rename", post(admin_rename_user))
        .route("/users/{id}/delete", post(admin_delete_user))
}
