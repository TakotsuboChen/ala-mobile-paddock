//! /admin —— Web 管理端（服务端渲染 askama 页面 + 页内弹窗 fetch JSON API）。
//! 架构（v2）：GET 页面只渲染静态骨架，所有写操作走 /admin/api/* JSON 端点，
//! 前端 fetch() 调用后页内刷新（弹窗确认），URL 不再出现路径后缀式动作。
//! 品牌名/Logo（configs 表 site_title/site_logo）作用于导航栏 + 浏览器标签页（title/favicon）。
//! 认证：cookie 会话（admin_sessions 表），账号/密码在首次启动时从环境变量播种。

use askama::Template;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::Engine as _;
use chrono::{Duration, TimeZone, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{api::laps::track_display_name, state::AppState as App};

/// 显示层统一用北京时间（TIMESTAMPTZ 存 UTC 不动，展示转 +8）。
fn bj_time(t: chrono::DateTime<Utc>) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::FixedOffset::east_opt(8 * 3600).unwrap().from_utc_datetime(&t.naive_utc())
}

// ---------- 品牌配置（站点名 + Logo，作用于导航栏与浏览器标签页） ----------

const DEFAULT_SITE_TITLE: &str = "围场";

/// 读品牌配置：site_title（默认"围场"）+ site_logo（data URL，None = 用默认 🏁）。
async fn brand(pool: &PgPool) -> (String, Option<String>) {
    let title = crate::qq_bot::get_cfg(pool, "site_title")
        .await
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| DEFAULT_SITE_TITLE.to_string());
    let logo = crate::qq_bot::get_cfg(pool, "site_logo")
        .await
        .filter(|s| s.starts_with("data:image/"));
    (title, logo)
}

// ---------- 模板 ----------

/// 默认 favicon：内联 SVG 方格旗（data URL，无 Logo 配置时兜底）。
const DEFAULT_FAVICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'%3E%3Ctext y='0.9em' font-size='90'%3E%F0%9F%8F%81%3C/text%3E%3C/svg%3E";

fn favicon_href(site_logo: &Option<String>) -> String {
    site_logo.clone().unwrap_or_else(|| DEFAULT_FAVICON.to_string())
}

/// 导航栏品牌图标：有 Logo 渲染 <img>，否则 🏁 字符。
fn logo_html(site_logo: &Option<String>) -> String {
    match site_logo {
        Some(l) => format!(r#"<img src="{l}" alt="">"#),
        None => "🏁".to_string(),
    }
}

#[derive(Template)]
#[template(path = "admin_login.html")]
struct LoginTemplate {
    site_title: String,
    favicon_href: String,
    has_logo: bool,
    site_logo_src: String,
    error: String,
}

#[derive(Template)]
#[template(path = "admin_base.html")]
struct DashTemplate {
    title: &'static str,
    active: &'static str,
    site_title: String,
    favicon_href: String,
    logo_html: String,
    content: String,
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
#[template(path = "admin_users.html")]
struct UsersTemplate {
    users: Vec<UserRow>,
    q: String,
    q_json: String,
    page: i64,
    size: i64,
    pages: i64,
}

struct LapRow {
    id: String,
    username: String,
    track: String,
    version_name: String,
    lap_display: String,
    lap_ms_raw: i32,
    created: String,
}

#[derive(Template)]
#[template(path = "admin_laps.html")]
struct LapsTemplate {
    rows: Vec<LapRow>,
    q: String,
    q_json: String,
    page: i64,
    size: i64,
    pages: i64,
    tracks_json: String,
    usernames_json: String,
}

/// 分页参数（用户页/成绩页共用）：页大小 30/50/100。
#[derive(Deserialize, Clone)]
struct PageQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_size")]
    size: i64,
}

fn default_page() -> i64 {
    1
}

fn default_size() -> i64 {
    30
}

const SIZE_OPTIONS: [i64; 3] = [30, 50, 100];

fn clamp_page(q: &mut PageQuery, total: i64) {
    if !SIZE_OPTIONS.contains(&q.size) {
        q.size = 30;
    }
    q.page = q.page.max(1);
    let pages = ((total + q.size - 1) / q.size).max(1);
    if q.page > pages {
        q.page = pages;
    }
}

async fn fetch_users(pool: &PgPool, q: &str, size: i64, offset: i64) -> (Vec<UserRow>, i64) {
    let like = format!("%{q}%");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username LIKE $1")
        .bind(&like)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.member_openid, u.reg_seq, u.created_at, \
         (SELECT count(*) FROM best_laps b WHERE b.user_id = u.id) \
         FROM users u WHERE u.username LIKE $1 ORDER BY u.reg_seq ASC LIMIT $2 OFFSET $3",
    )
    .bind(&like)
    .bind(size)
    .bind(offset)
    .fetch_all(pool)
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
    (rows, total)
}

async fn fetch_laps(pool: &PgPool, qname: &str, size: i64, offset: i64) -> (Vec<LapRow>, i64) {
    let like = format!("%{qname}%");
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM laps l JOIN users u ON u.id=l.user_id WHERE u.username LIKE $1")
            .bind(&like)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    let rows: Vec<LapRow> = sqlx::query_as(
        "SELECT l.id, u.username, l.gp_index, l.version_code, l.lap_ms, l.created_at \
         FROM laps l JOIN users u ON u.id=l.user_id \
         WHERE u.username LIKE $1 ORDER BY l.created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(&like)
    .bind(size)
    .bind(offset)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(id, name, gp, ver, ms, created): (Uuid, String, i16, i32, i32, chrono::DateTime<Utc>)| {
            LapRow {
                id: id.to_string(),
                username: name,
                track: track_display_name(gp).to_string(),
                version_name: crate::api::laps::version_display(ver),
                lap_display: crate::api::leaderboard::format_lap_ms(ms),
                lap_ms_raw: ms,
                created: bj_time(created).format("%Y-%m-%d %H:%M:%S").to_string(),
            }
        },
    )
    .collect();
    (rows, total)
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

/// JSON API 统一响应：{ok, message}。message 直接进页内 Toast。
fn api_res(ok: bool, message: impl Into<String>) -> Response {
    Json(json!({"ok": ok, "message": message.into()})).into_response()
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

// ---------- 页面（GET，只渲染骨架；写操作全在 /admin/api/*） ----------

async fn login_page(State(state): State<App>) -> Response {
    ensure_admin_seeded(&state.pool).await.ok();
    let (site_title, site_logo) = brand(&state.pool).await;
    let t = LoginTemplate {
        site_title,
        favicon_href: favicon_href(&site_logo),
        has_logo: site_logo.is_some(),
        site_logo_src: site_logo.unwrap_or_default(),
        error: String::new(),
    };
    html_res(StatusCode::OK, t.render().unwrap_or_default())
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_submit(State(state): State<App>, axum::Form(f): axum::Form<LoginForm>) -> Response {
    let row: Option<(String,)> = sqlx::query_as("SELECT pass_hash FROM admins WHERE username = $1")
        .bind(&f.username)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let ok = row
        .map(|(h,)| crate::auth::verify_password(&f.password, &h))
        .unwrap_or(false);
    if !ok {
        crate::applog::log_event(
            &state.pool, "warn", "admin", "admin_login_failed", &f.username,
            "管理端登录失败（用户名或密码错误）", json!({}),
        );
        let (site_title, site_logo) = brand(&state.pool).await;
        let t = LoginTemplate {
            site_title,
            favicon_href: favicon_href(&site_logo),
            has_logo: site_logo.is_some(),
            site_logo_src: site_logo.unwrap_or_default(),
            error: "用户名或密码错误".into(),
        };
        return html_res(StatusCode::UNAUTHORIZED, t.render().unwrap());
    }
    let (raw, hash) = crate::auth::issue_token().unwrap();
    crate::applog::log_event(
        &state.pool, "info", "admin", "admin_login", &f.username,
        "管理端登录成功", json!({}),
    );
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

async fn users_page(
    State(state): State<App>,
    headers: HeaderMap,
    Query(mut pq): Query<PageQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    let like = format!("%{}%", pq.q);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username LIKE $1")
        .bind(&like)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    clamp_page(&mut pq, total);
    let (users, _) = fetch_users(&state.pool, &pq.q, pq.size, (pq.page - 1) * pq.size).await;
    let q_json = serde_json::to_string(&pq.q).unwrap_or_else(|_| "\"\"".into());
    let body = UsersTemplate {
        users,
        q: pq.q.clone(),
        q_json,
        page: pq.page,
        size: pq.size,
        pages: ((total + pq.size - 1) / pq.size).max(1),
    }
    .render()
    .unwrap();
    let (site_title, site_logo) = brand(&state.pool).await;
    let t = DashTemplate {
        title: "用户管理",
        active: "users",
        favicon_href: favicon_href(&site_logo),
        logo_html: logo_html(&site_logo),
        site_title,
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

async fn laps_page(
    State(state): State<App>,
    headers: HeaderMap,
    Query(mut pq): Query<PageQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    let like = format!("%{}%", pq.q);
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM laps l JOIN users u ON u.id=l.user_id WHERE u.username LIKE $1",
    )
    .bind(&like)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);
    clamp_page(&mut pq, total);
    let tracks: Vec<(i16, String)> = (0..16)
        .map(|i| (i, track_display_name(i).to_string()))
        .collect();
    // ⚠️ query_scalar 输出类型此处必须是 String 标量——曾被 .map(|(u,): (String,)|) 标注
    // 反推成元组导致运行时解码失败（下拉空白）。保持无标注，交给标量上下文推断。
    let usernames: Vec<String> =
        sqlx::query_scalar("SELECT username FROM users ORDER BY reg_seq")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();
    let tracks_json =
        serde_json::to_string(&tracks).unwrap_or_else(|_| "[]".into());
    let usernames_json =
        serde_json::to_string(&usernames).unwrap_or_else(|_| "[]".into());
    let q_json = serde_json::to_string(&pq.q).unwrap_or_else(|_| "\"\"".into());
    let (rows, _) = fetch_laps(&state.pool, &pq.q, pq.size, (pq.page - 1) * pq.size).await;
    let body = LapsTemplate {
        rows,
        q: pq.q.clone(),
        q_json,
        page: pq.page,
        size: pq.size,
        pages: ((total + pq.size - 1) / pq.size).max(1),
        tracks_json,
        usernames_json,
    }
    .render()
    .unwrap();
    let (site_title, site_logo) = brand(&state.pool).await;
    let t = DashTemplate {
        title: "成绩管理",
        active: "laps",
        favicon_href: favicon_href(&site_logo),
        logo_html: logo_html(&site_logo),
        site_title,
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

// ---------- 日志页 ----------

#[derive(Template)]
#[template(path = "admin_logs.html")]
struct LogsTemplate {
    rows: Vec<LogRow>,
    q: String,
    q_json: String,
    level: String,
    cat: String,
    page: i64,
    size: i64,
    pages: i64,
}

struct LogRow {
    created: String,
    level: String,
    cat_display: String,
    actor: String,
    event: String,
    message: String,
}

/// 日志页查询参数：level/cat 筛选 + 通用 q（actor/message LIKE）。
#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    level: String,
    #[serde(default)]
    cat: String,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_size")]
    size: i64,
}

fn cat_display(cat: &str) -> &'static str {
    match cat {
        "admin" => "管理端",
        "auth" => "认证",
        "lap" => "成绩",
        "bot" => "Bot",
        _ => "其他",
    }
}

/// 日志页查询（三段筛选全部可选）。level/cat 值由调用方白名单校验后传入；
/// q 做 actor/message/event 的 LIKE 匹配。返回 (行, 总数)。
async fn fetch_logs(
    pool: &PgPool,
    q: &str,
    level: &str,
    cat: &str,
    size: i64,
    offset: i64,
) -> (Vec<LogRow>, i64) {
    let like = format!("%{q}%");
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app_logs \
         WHERE (actor LIKE $1 OR message LIKE $1 OR event LIKE $1) \
           AND ($2 = '' OR level = $2) AND ($3 = '' OR category = $3)",
    )
    .bind(&like)
    .bind(level)
    .bind(cat)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let rows: Vec<(chrono::DateTime<Utc>, String, String, String, String, String)> = sqlx::query_as(
        "SELECT created_at, level, category, actor, event, message FROM app_logs \
         WHERE (actor LIKE $1 OR message LIKE $1 OR event LIKE $1) \
           AND ($2 = '' OR level = $2) AND ($3 = '' OR category = $3) \
         ORDER BY id DESC LIMIT $4 OFFSET $5",
    )
    .bind(&like)
    .bind(level)
    .bind(cat)
    .bind(size)
    .bind(offset)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let rows = rows
        .into_iter()
        .map(|(created, level, cat, actor, event, message)| LogRow {
            created: bj_time(created).format("%Y-%m-%d %H:%M:%S").to_string(),
            level,
            cat_display: cat_display(&cat).to_string(),
            actor,
            event,
            message,
        })
        .collect();
    (rows, total)
}

async fn logs_page(
    State(state): State<App>,
    headers: HeaderMap,
    Query(mut lq): Query<LogQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    if !["info", "warn", "error"].contains(&lq.level.as_str()) {
        lq.level.clear();
    }
    if !["admin", "auth", "lap", "bot"].contains(&lq.cat.as_str()) {
        lq.cat.clear();
    }
    // 页码收敛（size 白名单 + page 边界）需先拿总数——与用户/成绩页同节奏
    let like = format!("%{}%", lq.q);
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app_logs \
         WHERE (actor LIKE $1 OR message LIKE $1 OR event LIKE $1) \
           AND ($2 = '' OR level = $2) AND ($3 = '' OR category = $3)",
    )
    .bind(&like)
    .bind(&lq.level)
    .bind(&lq.cat)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);
    let mut pq = PageQuery { q: lq.q.clone(), page: lq.page, size: lq.size };
    clamp_page(&mut pq, total);
    let (rows, _) = fetch_logs(
        &state.pool, &lq.q, &lq.level, &lq.cat, pq.size, (pq.page - 1) * pq.size,
    )
    .await;
    let q_json = serde_json::to_string(&lq.q).unwrap_or_else(|_| "\"\"".into());
    let body = LogsTemplate {
        rows,
        q: lq.q.clone(),
        q_json,
        level: lq.level.clone(),
        cat: lq.cat.clone(),
        page: pq.page,
        size: pq.size,
        pages: ((total + pq.size - 1) / pq.size).max(1),
    }
    .render()
    .unwrap();
    let (site_title, site_logo) = brand(&state.pool).await;
    let t = DashTemplate {
        title: "日志",
        active: "logs",
        favicon_href: favicon_href(&site_logo),
        logo_html: logo_html(&site_logo),
        site_title,
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

// ---------- 用户 API（JSON，页内弹窗调用） ----------

#[derive(Deserialize)]
struct ApiRename {
    new_username: String,
}

/// 编辑用户 = 改名（前端在同一个弹窗里也可顺带重置密码，分两次调用本组 API）。
async fn api_rename_user(
    State(state): State<App>,
    headers: HeaderMap,
    PathErr(user_id): PathErr<Uuid>,
    Json(f): Json<ApiRename>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    let new_name = f.new_username.trim().to_string();
    if crate::auth::validate_username(&new_name).is_err() {
        return api_res(false, "用户名须为 1–16 位中文/字母/数字，两侧禁空格");
    }
    let Ok(mut tx) = state.pool.begin().await else {
        return api_res(false, "数据库连接失败");
    };
    let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username=$1 AND id<>$2")
        .bind(&new_name)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(1); // 查询失败按"已占用"处理，拒绝改名
    if taken > 0 {
        return api_res(false, "用户名已被占用");
    }
    let n = sqlx::query("UPDATE users SET username=$2 WHERE id=$1")
        .bind(user_id)
        .bind(&new_name)
        .execute(&mut *tx)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if n > 0 {
        crate::applog::log_event_tx(
            &mut tx, "info", "admin", "rename_user", &admin,
            format!("重命名用户 → {new_name}"),
            json!({"user_id": user_id.to_string(), "new_username": new_name}),
        ).await;
        tx.commit().await.ok();
        api_res(true, format!("用户名已改为 {new_name}（该用户需用新名登录）"))
    } else {
        api_res(false, "用户不存在")
    }
}

#[derive(Deserialize)]
struct ApiResetPw {
    new_password: String,
}

/// 人工重置密码（PADDOCK_PLAN §1"管理端同时留人工重置入口"）：直接设新密码，
/// 作废该用户全部模块会话。
async fn api_reset_password(
    State(state): State<App>,
    headers: HeaderMap,
    PathErr(user_id): PathErr<Uuid>,
    Json(f): Json<ApiResetPw>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    if crate::auth::validate_password(&f.new_password).is_err() {
        return api_res(false, "密码至少 8 位，且须同时包含数字和字母");
    }
    let hash = match crate::auth::hash_password(&f.new_password) {
        Ok(h) => h,
        Err(_) => return api_res(false, "密码哈希失败"),
    };
    let Ok(mut tx) = state.pool.begin().await else {
        return api_res(false, "数据库连接失败");
    };
    let n = sqlx::query("UPDATE users SET pass_hash=$2 WHERE id=$1")
        .bind(user_id)
        .bind(&hash)
        .execute(&mut *tx)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if n == 0 {
        return api_res(false, "用户不存在");
    }
    sqlx::query("DELETE FROM sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .ok();
    crate::applog::log_event_tx(
        &mut tx, "info", "admin", "reset_password", &admin,
        "重置用户密码（该用户全部登录已失效）",
        json!({"user_id": user_id.to_string()}),
    ).await;
    tx.commit().await.ok();
    api_res(true, "密码已重置（该用户所有登录已失效）")
}

/// 删除用户（弹窗输入用户名确认）。事务：
/// 1. 收集该用户有 laps 的全部 (gp, version) 维度；
/// 2. DELETE users（sessions/laps/best_laps 级联清；records.user_id 置 NULL）；
/// 3. 对每个维度重算 records（悬空的纪录行回放给剩余用户，无圈则删行）。
async fn api_delete_user(
    State(state): State<App>,
    headers: HeaderMap,
    PathErr(user_id): PathErr<Uuid>,
    Json(f): Json<ApiDeleteUser>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    let name: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id=$1")
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let Some(name) = name else {
        return api_res(false, "用户不存在");
    };
    if f.confirm.trim() != name {
        return api_res(false, "确认输入与用户名不一致，未删除");
    }
    match delete_user_recalc(&state.pool, user_id, &admin).await {
        Ok(n) => api_res(true, format!("已删除用户 {n}（成绩与登录态一并清除，纪录已重算）")),
        Err(e) => api_res(false, format!("删除失败：{e}")),
    }
}

#[derive(Deserialize)]
struct ApiDeleteUser {
    confirm: String,
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
        apply_record(&mut tx, *gp, "alltime", 0, va).await?;
        apply_record(&mut tx, *gp, "version", *ver, vb).await?;
    }
    crate::applog::log_event_tx(
        &mut tx, "warn", "admin", "delete_user", admin,
        format!("删除用户 {name}（成绩与登录态一并清除，纪录已重算）"),
        json!({"user_id": user_id.to_string(), "username": name}),
    ).await;
    tx.commit().await?;
    Ok(name)
}

// ---------- 成绩 API（JSON） ----------

#[derive(Deserialize)]
struct ApiEditLap {
    /// 分/秒/毫秒三段输入（前端弹窗拆好），服务端合并校验
    min: i32,
    sec: i32,
    ms: i32,
}

/// 编辑圈时（改 lap_ms）→ 同事务重算 best_laps/records。
async fn api_edit_lap(
    State(state): State<App>,
    headers: HeaderMap,
    PathErr(lap_id): PathErr<Uuid>,
    Json(f): Json<ApiEditLap>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    if !(0..=59).contains(&f.min) || !(0..=59).contains(&f.sec) || !(0..=999).contains(&f.ms) {
        return api_res(false, "时间分段非法（分 0–59，秒 0–59，毫秒 0–999）");
    }
    let new_ms = f.min * 60_000 + f.sec * 1000 + f.ms;
    match recalc_edit(&state.pool, lap_id, new_ms, &admin).await {
        Ok(Some((gp, ver))) => {
            crate::qq_bot::broadcast_lap_change(&state, gp, ver, new_ms).await;
            api_res(true, format!("圈时已改为 {} 并重算", crate::api::leaderboard::format_lap_ms(new_ms)))
        }
        Ok(None) => api_res(false, "目标不存在或圈时超出合法范围（0 < 圈时 ≤ 60:00.000）"),
        Err(_) => api_res(false, "编辑失败"),
    }
}

async fn api_delete_lap(
    State(state): State<App>,
    headers: HeaderMap,
    PathErr(lap_id): PathErr<Uuid>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    match recalc_delete(&state.pool, lap_id, &admin).await {
        Ok(Some((gp, ver, before_alltime, before_version))) => {
            // 仅当删除导致纪录变化（值变或易主）才播报——删非最快圈不应触发
            crate::qq_bot::broadcast_lap_change_if_changed(
                &state, gp, ver,
                before_alltime, before_version,
            ).await;
            api_res(true, "已删除并重算榜单/纪录")
        }
        Ok(None) => api_res(false, "该成绩不存在（可能已被删除）"),
        Err(_) => api_res(false, "删除失败"),
    }
}

#[derive(Deserialize)]
struct ApiAddLap {
    username: String,
    gp_index: i16,
    min: i32,
    sec: i32,
    ms: i32,
}

/// 添加成绩（管理端补录）： laps 全量留档 → 重算该维度 best_laps/records。
async fn api_add_lap(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiAddLap>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    let username = f.username.trim();
    if !(0..16).contains(&f.gp_index) {
        return api_res(false, "赛道越界");
    }
    if !(0..=59).contains(&f.min) || !(0..=59).contains(&f.sec) || !(0..=999).contains(&f.ms) {
        return api_res(false, "时间分段非法（分 0–59，秒 0–59，毫秒 0–999）");
    }
    let lap_ms = f.min * 60_000 + f.sec * 1000 + f.ms;
    if lap_ms <= 0 {
        return api_res(false, "圈时必须大于 0");
    }
    let user_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE username=$1")
        .bind(username)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let Some(user_id) = user_id else {
        return api_res(false, format!("用户 {username} 不存在"));
    };
    let Ok(mut tx) = state.pool.begin().await else {
        return api_res(false, "数据库连接失败");
    };
    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(f.gp_index)
    .bind(200146) // 管理端补录按当前游戏版本 8.0.4 记账
    .bind(lap_ms)
    .execute(&mut *tx)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3 AND lap_ms=$4")
        .bind(user_id).bind(f.gp_index).bind(200146i32).bind(lap_ms)
        .fetch_one(&mut *tx).await.unwrap_or(0) == 0 {
        return api_res(false, "写入成绩失败");
    }
    if let Err(e) = recalc_dims(&mut tx, user_id, f.gp_index, 200146).await {
        return api_res(false, format!("重算失败：{e}"));
    }
    crate::applog::log_event_tx(
        &mut tx, "info", "admin", "add_lap", &admin,
        format!("补录成绩：{username} · {} · {}", track_display_name(f.gp_index), crate::api::leaderboard::format_lap_ms(lap_ms)),
        json!({"username": username, "gp": f.gp_index, "lap_ms": lap_ms}),
    ).await;
    tx.commit().await.ok();
    // 补录成绩若刷新纪录同样播报（与模块上传同链路）
    crate::qq_bot::broadcast_lap_change(&state, f.gp_index, 200146, lap_ms).await;
    api_res(
        true,
        format!(
            "已为 {username} 添加 {} 的成绩 {} 并重算",
            track_display_name(f.gp_index),
            crate::api::leaderboard::format_lap_ms(lap_ms)
        ),
    )
}

/// 删除单条有效圈记录。为保持"防伪全放行 + 事后删"定案：
/// 删除后必须在同事务里重算 best_laps/records（从 laps 全量留档回放）。
/// 返回 Some((gp, version, 删前alltime纪录, 删前version纪录)) 供"变化才播报"判定。
async fn recalc_delete(pool: &PgPool, lap_id: Uuid, admin: &str) -> anyhow::Result<Option<(i16, i32, Option<i32>, Option<i32>)>> {
    let mut tx = pool.begin().await?;
    let target: Option<(Uuid, i16, i32)> =
        sqlx::query_as("DELETE FROM laps WHERE id=$1 RETURNING user_id, gp_index, version_code")
            .bind(lap_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((uid, gp, ver)) = target else {
        return Ok(None);
    };
    // 删前快照两个维度的纪录值（供删除后对比：无变化不播报）
    let before_alltime: Option<i32> = sqlx::query_scalar(
        "SELECT lap_ms FROM records WHERE gp_index=$1 AND kind='alltime'",
    ).bind(gp).fetch_optional(&mut *tx).await?.flatten();
    let before_version: Option<i32> = sqlx::query_scalar(
        "SELECT lap_ms FROM records WHERE gp_index=$1 AND kind='version' AND version_code=$2",
    ).bind(gp).bind(ver).fetch_optional(&mut *tx).await?.flatten();
    recalc_dims(&mut tx, uid, gp, ver).await?;
    sqlx::query("DELETE FROM admin_sessions WHERE expires_at < now()")
        .execute(&mut *tx)
        .await
        .ok();
    crate::applog::log_event_tx(
        &mut tx, "warn", "admin", "delete_lap", admin,
        format!("删除成绩（赛道 {gp} · 版本 {ver}，纪录已重算）"),
        json!({"lap_id": lap_id.to_string(), "gp": gp, "ver": ver}),
    )
    .await;
    tx.commit().await?;
    Ok(Some((gp, ver, before_alltime, before_version)))
}

/// 编辑圈时（改 lap_ms）后重算同维度。返回 Some((gp, version)) 表示目标存在且已改。
async fn recalc_edit(pool: &PgPool, lap_id: Uuid, new_ms: i32, admin: &str) -> anyhow::Result<Option<(i16, i32)>> {
    let mut tx = pool.begin().await?;
    let target: Option<(Uuid, i16, i32)> =
        sqlx::query_as("SELECT user_id, gp_index, version_code FROM laps WHERE id=$1")
            .bind(lap_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((uid, gp, ver)) = target else {
        return Ok(None);
    };
    // 校验合法圈时（防手滑输入 0 或负数把榜单刷穿）
    if new_ms <= 0 || new_ms > 3_600_000 {
        return Ok(None);
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
    crate::applog::log_event_tx(
        &mut tx, "info", "admin", "edit_lap", admin,
        format!("修改成绩圈时 → {}（赛道 {gp} · 版本 {ver}，纪录已重算）", crate::api::leaderboard::format_lap_ms(new_ms)),
        json!({"lap_id": lap_id.to_string(), "new_ms": new_ms}),
    )
    .await;
    tx.commit().await?;
    Ok(Some((gp, ver)))
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
            // upsert：best_laps 缺行（管理端补录新维度首圈/编辑后新维度）也能补插——
            // 原 UPDATE-only 写法对不存在的行静默 0 行，导致 laps 有圈而 best_laps/榜单无成绩
            sqlx::query(
                "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms) VALUES ($1,$2,$3,$4) \
                 ON CONFLICT (user_id, gp_index, version_code) \
                 DO UPDATE SET lap_ms=EXCLUDED.lap_ms, updated_at=now()",
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

// ---------- 设置页（品牌 + 改密码/用户名 + bot 配置 + 消息规则） ----------

#[derive(Template)]
#[template(path = "admin_settings.html")]
struct SettingsTemplate {
    site_title: String,
    favicon_href: String,
    site_logo_src: String,
    bot_app_id: String,
    bot_secret_set: bool,
    rules_json: String,
}

async fn settings_page(State(state): State<App>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return Redirect::to("/admin").into_response();
    }
    render_settings(&state).await
}

async fn render_settings(state: &App) -> Response {
    let (_site_title, _site_logo) = brand(&state.pool).await;
    let app_id = crate::qq_bot::get_cfg(&state.pool, "bot_app_id")
        .await
        .unwrap_or_default();
    let secret_set = crate::qq_bot::get_cfg(&state.pool, "bot_app_secret")
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let group_openid = crate::qq_bot::get_cfg(&state.pool, "bot_group_openid")
        .await
        .unwrap_or_default();
    // 旧单群配置迁移：bot_group_openid 有值且新键为空 → 转为勾选集
    if !group_openid.trim().is_empty() {
        let selected = crate::qq_bot::broadcast_groups(&state.pool).await;
        if selected.is_empty() {
            crate::qq_bot::set_cfg(&state.pool, "bot_broadcast_groups", group_openid.trim())
                .await
                .ok();
        }
    }
    let rules_json = crate::qq_bot::load_rules_json(&state.pool).await;
    let (dash_title, dash_logo) = brand(&state.pool).await;
    let body = SettingsTemplate {
        favicon_href: favicon_href(&dash_logo),
        site_logo_src: dash_logo.clone().unwrap_or_else(|| DEFAULT_FAVICON.to_string()),
        site_title: dash_title.clone(),
        bot_app_id: app_id,
        bot_secret_set: secret_set,
        rules_json,
    }
    .render()
    .unwrap();
    let t = DashTemplate {
        title: "设置",
        active: "settings",
        favicon_href: favicon_href(&dash_logo),
        logo_html: logo_html(&dash_logo),
        site_title: dash_title,
        content: body,
    };
    html_res(StatusCode::OK, t.render().unwrap())
}

#[derive(Deserialize)]
struct ApiChangePw {
    old_password: String,
    new_password: String,
    new_password2: String,
}

async fn api_change_password(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiChangePw>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    if f.new_password != f.new_password2 {
        return api_res(false, "两次输入的新密码不一致");
    }
    if crate::auth::validate_password(&f.new_password).is_err() {
        return api_res(false, "新密码至少 8 位，且须同时包含数字和字母");
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
        return api_res(false, "当前密码错误");
    }
    let new_hash = match crate::auth::hash_password(&f.new_password) {
        Ok(h) => h,
        Err(_) => return api_res(false, "密码哈希失败"),
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
    audit(&state.pool, "change_admin_password", json!({})).await;
    api_res(true, "密码已修改，请重新登录")
}

#[derive(Deserialize)]
struct ApiChangeUsername {
    new_username: String,
    password: String,
}

/// 修改管理员用户名：需验证当前密码防会话劫持。改完作废全部管理端会话。
async fn api_change_username(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiChangeUsername>,
) -> Response {
    let Some(admin) = admin_name(&state, &headers).await else {
        return api_res(false, "会话已过期，请重新登录");
    };
    let name = f.new_username.trim();
    if name.is_empty() || name.len() > 32 {
        return api_res(false, "用户名 1–32 字符");
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
        return api_res(false, "密码错误");
    }
    let taken: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admins WHERE username = $1 AND username <> $2")
            .bind(name)
            .bind(&admin)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(1);
    if taken > 0 {
        return api_res(false, "该用户名已被占用");
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
    audit(&state.pool, "change_admin_username", json!({"new": name})).await;
    api_res(true, "用户名已修改，请重新登录")
}

#[derive(Deserialize)]
struct ApiBotConfig {
    bot_app_id: String,
    bot_app_secret: String,
}

/// bot 凭据保存。secret 留空 = 保留原值（不回显，防泄露）。
async fn api_save_bot_config(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiBotConfig>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return api_res(false, "会话已过期，请重新登录");
    }
    match async {
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
                json!({"app_id": f.bot_app_id.trim()}),
            )
            .await;
            api_res(true, "bot 配置已保存（即时生效，无需重启）")
        }
        Err(_) => api_res(false, "保存失败，请重试"),
    }
}

// ---------- 消息规则 API ----------

#[derive(Deserialize)]
struct ApiSaveRules {
    rules: Vec<crate::qq_bot::BotRule>,
}

/// 全量保存消息规则（前端编辑器整表提交）。
async fn api_save_rules(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiSaveRules>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return api_res(false, "会话已过期，请重新登录");
    }
    for r in &f.rules {
        let is_action = r.kind == "reply" && (r.action == "reg_code" || r.action == "reset_password");
        let conds_ok = !r.conditions.is_empty() || !r.keyword.trim().is_empty();
        if r.enabled && r.kind == "reply" && !is_action && !conds_ok {
            return api_res(false, "回复规则至少需要一个条件");
        }
        if r.template.trim().is_empty() {
            return api_res(false, "消息模板不能为空");
        }
        // 内置动作必须有提取锚点（校验码/用户名从锚点后提取）
        if is_action && r.keyword.trim().is_empty() {
            return api_res(false, "内置动作规则必须填写触发词（提取锚点）");
        }
        for c in &r.conditions {
            if c.op.is_empty() || (c.field != "content" && c.field != "event") {
                return api_res(false, "条件字段/操作符非法");
            }
            if c.field == "event" && !matches!(c.value.as_str(), "record_alltime" | "record_version") {
                return api_res(false, "事件条件目前支持：record_alltime（历史纪录）/ record_version（版本纪录）");
            }
        }
    }
    match crate::qq_bot::save_rules(&state.pool, &f.rules).await {
        Ok(()) => {
            audit(&state.pool, "save_bot_rules", json!({"count": f.rules.len()})).await;
            api_res(true, format!("已保存 {} 条消息规则", f.rules.len()))
        }
        Err(e) => api_res(false, format!("保存失败：{e}")),
    }
}

#[derive(Deserialize)]
struct ApiBroadcastGroups {
    /// 勾选的目标群 openid 列表（空 = 关闭播报）
    groups: Vec<String>,
}

/// 保存播报目标群多选。
async fn api_save_broadcast_groups(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiBroadcastGroups>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return api_res(false, "会话已过期，请重新登录");
    }
    let known = crate::qq_bot::known_groups(&state.pool).await;
    for g in &f.groups {
        if !known.iter().any(|k| k == g) {
            return api_res(false, "包含未知群，请刷新页面重试");
        }
    }
    let joined = f.groups.join(",");
    if let Err(e) = crate::qq_bot::set_cfg(&state.pool, "bot_broadcast_groups", &joined).await {
        return api_res(false, format!("保存失败：{e}"));
    }
    audit(&state.pool, "save_broadcast_groups", json!({"count": f.groups.len()})).await;
    api_res(true, if f.groups.is_empty() { "已清空播报目标群（播报关闭）".to_string() } else { format!("播报目标群已保存（{} 个）", f.groups.len()) })
}

/// 已知群列表（设置页选择器数据源，带群名缓存）。
async fn api_known_groups(State(state): State<App>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return api_res(false, "会话已过期，请重新登录");
    }
    let names = crate::qq_bot::group_names_public(&state.pool).await;
    Json(json!({
        "ok": true,
        "known": crate::qq_bot::known_groups(&state.pool).await,
        "selected": crate::qq_bot::broadcast_groups(&state.pool).await,
        "names": names,
    }))
    .into_response()
}

// ---------- 品牌 API ----------

#[derive(Deserialize)]
struct ApiBrand {
    /// 站点名（trim 后 1–32 字符）
    site_title: String,
    /// data URL（image/svg+xml|png|jpeg|webp）；空串 = 清除恢复默认 🏁
    site_logo: Option<String>,
}

const BRAND_LOGO_MAX_BYTES: usize = 256 * 1024;

async fn api_save_brand(
    State(state): State<App>,
    headers: HeaderMap,
    Json(f): Json<ApiBrand>,
) -> Response {
    if require_admin(&state, &headers).await.is_none() {
        return api_res(false, "会话已过期，请重新登录");
    }
    let title = f.site_title.trim();
    if title.is_empty() || title.chars().count() > 32 {
        return api_res(false, "站点名 1–32 字符");
    }
    if let Err(e) = crate::qq_bot::set_cfg(&state.pool, "site_title", title).await {
        return api_res(false, format!("保存失败：{e}"));
    }
    match f.site_logo.as_deref() {
        Some("") | None => {
            crate::qq_bot::set_cfg(&state.pool, "site_logo", "").await.ok();
        }
        Some(data_url) => {
            // 校验 data URL：仅允许 image/(svg+xml|png|jpeg|webp)，大小封顶
            let ok_prefix = ["data:image/svg+xml;", "data:image/png;", "data:image/jpeg;", "data:image/webp;"]
                .iter()
                .any(|p| data_url.starts_with(p));
            if !ok_prefix || !data_url.contains(";base64,") {
                return api_res(false, "Logo 仅支持 SVG/PNG/JPG/WebP 图片");
            }
            let b64 = data_url.split(";base64,").nth(1).unwrap_or("");
            let size = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map(|b| b.len())
                .unwrap_or(0);
            if size == 0 {
                return api_res(false, "Logo 数据无效");
            }
            if size > BRAND_LOGO_MAX_BYTES {
                return api_res(false, "Logo 不能超过 256KB");
            }
            if let Err(e) = crate::qq_bot::set_cfg(&state.pool, "site_logo", data_url).await {
                return api_res(false, format!("保存失败：{e}"));
            }
        }
    }
    audit(&state.pool, "save_brand", json!({"site_title": title})).await;
    api_res(true, "品牌配置已保存（刷新页面生效）")
}

async fn audit(pool: &PgPool, action: &str, detail: serde_json::Value) {
    crate::applog::log_event(
        pool, "info", "admin", action, "admin",
        action_message(action, &detail),
        detail,
    );
}

/// audit() 兼容层：action → 人类可读摘要（旧 admin_audit 调用点全部转 applog）。
fn action_message(action: &str, detail: &serde_json::Value) -> String {
    match action {
        "change_admin_password" => "修改管理员密码（全部管理端会话已作废）".into(),
        "change_admin_username" => format!("修改管理员用户名 → {}", detail["new"].as_str().unwrap_or("")),
        "save_bot_config" => format!("保存 bot 配置（app_id={}) ", detail["app_id"].as_str().unwrap_or("")),
        "save_bot_rules" => format!("保存消息规则（{} 条）", detail["count"]),
        "save_broadcast_groups" => format!("保存播报目标群（{} 个）", detail["count"]),
        "save_brand" => format!("保存品牌配置（站点名：{}）", detail["site_title"].as_str().unwrap_or("")),
        other => other.to_string(),
    }
}

/// JSON 端点的路径提取器：UUID 解析失败统一 404 JSON（前端 fetch 不会走浏览器跳转）。
struct PathErr<U>(U);
impl<S, U> axum::extract::FromRequestParts<S> for PathErr<U>
where
    U: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<U>::from_request_parts(parts, state).await {
            Ok(p) => Ok(PathErr(p.0)),
            Err(_) => Err((
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "message": "目标不存在"})),
            )),
        }
    }
}

pub fn router() -> Router<App> {
    Router::new()
        .route("/", get(login_page))
        .route("/login", post(login_submit))
        .route("/logout", post(logout))
        .route("/users", get(users_page))
        .route("/laps", get(laps_page))
        .route("/logs", get(logs_page))
        .route("/settings", get(settings_page))
        // 用户 API
        .route("/api/users/{id}/rename", post(api_rename_user))
        .route("/api/users/{id}/reset-password", post(api_reset_password))
        .route("/api/users/{id}/delete", post(api_delete_user))
        // 成绩 API
        .route("/api/laps/{id}/edit", post(api_edit_lap))
        .route("/api/laps/{id}/delete", post(api_delete_lap))
        .route("/api/laps/add", post(api_add_lap))
        // 设置 API
        .route("/api/settings/password", post(api_change_password))
        .route("/api/settings/username", post(api_change_username))
        .route("/api/settings/bot", post(api_save_bot_config))
        .route("/api/bot/rules", post(api_save_rules))
        .route("/api/bot/groups", get(api_known_groups))
        .route("/api/bot/groups", post(api_save_broadcast_groups))
        .route("/api/brand", post(api_save_brand))
}
