//! /qq/webhook —— CAMDA 群 bot 回调（QQ 官方开放平台 webhook 模式，S4）。
//!
//! 验签（官方文档 sign.html，[V] 2026-08-31 核对）：
//!   seed = bot secret 循环填充满 32 字节 → Ed25519 密钥对；
//!   签名体 = X-Signature-Timestamp + 原始 body；
//!   签名在 header X-Signature-Ed25519（hex，64 字节）。
//! op=13 回调地址验证：payload.d = {plain_token, event_ts}，
//!   应答 {plain_token, signature}，signature = hex(Sign(event_ts + plain_token))。
//! 事件处理（GROUP_MESSAGE_CREATE 群全量消息定案）：
//!   "申请围场通行证#XXXXXXXX" → 定位 pending 会话绑定 member_openid → 被动回复结果
//!   "重置密码 用户名"          → 生成一次性码 → 被动回复（30 分钟有效）
//! 被动回复：HTTP 回调模式收包后走 REST API 发群消息（msg_id 5 分钟窗、每消息 5 次、
//!   同 msg_id+msg_seq 去重），频控未认证 30/qpm → 发送队列串行 + 间隔。
//! bot 凭据（AppID/Secret/群 openid）存 configs 表，由管理端设置页维护。

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::{api::auth_handlers, state::AppState as App};

/// bot 密钥 seed：官方算法——secret 字节循环填充至 32 字节。
fn expand_seed(secret: &str) -> [u8; 32] {
    let s = secret.as_bytes();
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = s[i % s.len()];
    }
    seed
}

fn signing_key(secret: &str) -> SigningKey {
    SigningKey::from_bytes(&expand_seed(secret))
}

/// 校验事件签名（op=0 Dispatch 与 op=13 均验）。
fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(sig_hex) = headers.get("X-Signature-Ed25519").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(ts) = headers.get("X-Signature-Timestamp").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Ok(sig) = hex::decode(sig_hex) else {
        return false;
    };
    if sig.len() != 64 {
        return false;
    }
    let Ok(sig) = Signature::from_slice(&sig) else {
        return false;
    };
    let key = signing_key(secret);
    let mut msg = ts.as_bytes().to_vec();
    msg.extend_from_slice(body);
    key.verifying_key().verify(&msg, &sig).is_ok()
}

#[derive(Deserialize)]
struct CallbackValidation {
    plain_token: String,
    event_ts: String,
}

/// POST /qq/webhook 统一入口：验签 → op=13 验证应答 / op=0 事件分发。
/// 任何失败一律 HTTP 2xx（QQ 平台对非 2xx 重试且要求验证期必须回 JSON），
/// 业务错误记录进日志即可，不外泄内部状态。
async fn webhook(
    State(state): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        tracing::warn!("[qq_bot] 非 JSON body，忽略");
        return (StatusCode::OK, Json(json!({}))).into_response();
    };
    let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);

    // bot 凭据：configs 表（管理端设置页维护）；未配置时无法验签，直接忽略事件。
    let (_app_id, secret) = match (get_cfg(&state.pool, "bot_app_id").await, get_cfg(&state.pool, "bot_app_secret").await) {
        (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => (a, s),
        _ => {
            tracing::warn!("[qq_bot] bot 凭据未配置（管理端设置页），忽略 op={op} 请求");
            return (StatusCode::OK, Json(json!({}))).into_response();
        }
    };

    if !verify_signature(&secret, &headers, &body) {
        tracing::warn!("[qq_bot] 签名校验失败，拒绝处理");
        return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
    }

    // 全量落盘：平台推送的事件类型与我们处理的类型存在认知缺口时，
    // 原始 payload 是唯一的裁决依据（临时诊断日志）。
    tracing::info!("[qq_bot] payload 原文: {}", String::from_utf8_lossy(&body));

    if op == 13 {
        // 回调地址验证
        let Ok(CallbackValidation { plain_token, event_ts }) =
            serde_json::from_value::<CallbackValidation>(payload["d"].clone())
        else {
            tracing::warn!("[qq_bot] op=13 payload 缺 plain_token/event_ts");
            return (StatusCode::OK, Json(json!({}))).into_response();
        };
        let key = signing_key(&secret);
        let sig = key.sign(format!("{event_ts}{plain_token}").as_bytes());
        tracing::info!("[qq_bot] op=13 回调验证应答完成");
        return (
            StatusCode::OK,
            Json(json!({
                "plain_token": plain_token,
                "signature": hex::encode(sig.to_bytes()),
            })),
        )
            .into_response();
    }

    if op == 0 {
        let kind = payload["t"].as_str().unwrap_or("");
        let event_id = payload["id"].as_str().unwrap_or("");
        if kind == "GROUP_MESSAGE_CREATE" || kind == "GROUP_AT_MESSAGE_CREATE" {
            handle_group_message(&state, event_id, &payload["d"]).await;
        } else if kind == "C2C_MESSAGE_CREATE" {
            handle_c2c_message(&state, event_id, &payload["d"]).await;
        } else {
            tracing::debug!("[qq_bot] 忽略事件 {kind}");
        }
    }
    // op=12 由平台约定 ACK：直接 2xx 即可
    (StatusCode::OK, Json(json!({}))).into_response()
}

// ---------- 群消息处理 ----------

#[derive(Deserialize)]
struct GroupMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    group_openid: String,
    /// 事件的 member_openid 在 author 对象内（d.author.member_openid），不在 d 顶层
    #[serde(default)]
    author: Value,
}

impl GroupMessage {
    fn member_openid(&self) -> &str {
        self.author
            .get("member_openid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.author.get("id").and_then(Value::as_str).unwrap_or(""))
    }
}

async fn handle_group_message(state: &App, event_id: &str, d: &Value) {
    let Ok(msg) = serde_json::from_value::<GroupMessage>(d.clone()) else {
        tracing::warn!("[qq_bot] GROUP_MESSAGE payload 解析失败，忽略");
        return;
    };
    let content = msg.content.trim();
    if content.is_empty() || msg.group_openid.is_empty() {
        return;
    }
    // 被动回复的 msg_id 优先用 d.id（消息 ID，官方示例为 ROBOT1.0_xxx 形态）；
    // 事件外层 id 为 "事件类型:xxx" 前缀形态，仅作兜底。
    let msg_id = if msg.id.is_empty() { event_id.to_string() } else { msg.id.clone() };

    // QQ 客户端会把 "#码" 渲染成话题样式并在 # 前插空格（实测 "申请围场通行证 #XXX"），
    // 也可能有全角 #。归一化后再匹配：# 两侧允许空格，全角 # 当半角用。
    let normalized = content.replace('＃', "#");
    if let Some(idx) = normalized.find("申请围场通行证") {
        let rest = normalized[idx + "申请围场通行证".len()..].trim_start();
        let code = rest
            .strip_prefix('#')
            .map(|c| c.trim().to_uppercase())
            .unwrap_or_default();
        handle_reg_code(state, &msg_id, &code, msg.member_openid(), &msg.group_openid).await;
    } else if let Some(idx) = normalized.find("重置密码") {
        let name = normalized[idx + "重置密码".len()..].trim();
        handle_reset_password(state, &msg_id, name, &msg.group_openid).await;
    }
    // 其余群内聊天不响应（bot 静默）
}

/// 注册码监听：码 → pending_regs 定位会话 → 绑定 member_openid 并**直接建号** → 被动回复。
/// 新流程（2026-09-01 定案 v2）：申请时已存密码哈希+车手 ID，校验成功即建号并回复序号；
/// 用户回模块用申请时的账号密码直接登录。
/// 约束（PADDOCK_PLAN §6 S4）：一条码一openid；重复绑定他人码会被唯一约束拦。
async fn handle_reg_code(
    state: &App,
    event_id: &str,
    code: &str,
    member_openid: &str,
    group: &str,
) {
    let reply = if member_openid.is_empty() {
        "无法识别你的群身份，请确认已在 QQ 群设置中允许机器人获取群信息".to_string()
    } else {
        let n: Result<i64, _> = sqlx::query_scalar(
            "SELECT count(*) FROM pending_regs WHERE reg_code = $1 AND expires_at > now()",
        )
        .bind(code)
        .fetch_one(&state.pool)
        .await;
        match n {
            Ok(0) | Err(_) => format!("校验码 {code} 无效或已过期，请在围场页重新申请"),
            Ok(_) => {
                // openid 已被其他在途会话占用：直接拒绝（防一 QQ 多账号）
                let occupied: Option<String> = sqlx::query_scalar(
                    "SELECT reg_code FROM pending_regs WHERE member_openid = $1 AND expires_at > now() AND reg_code <> $2",
                )
                .bind(member_openid)
                .bind(code)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or(None);
                if occupied.is_some() {
                    "该 QQ 身份已有其他注册校验在途，请勿重复申请".to_string()
                } else {
                    // 建号事务：DELETE pending RETURNING 锁存数据 → INSERT users。
                    // 失败时 pending 已被事务回滚恢复，用户可重试。
                    match auth_handlers::create_user_from_pending(&state.pool, code, member_openid).await {
                        Ok(user_id) => {
                            let username: String = sqlx::query_scalar(
                                "SELECT username FROM users WHERE id = $1",
                            )
                            .bind(user_id)
                            .fetch_one(&state.pool)
                            .await
                            .unwrap_or_default();
                            let reg_seq: i64 = sqlx::query_scalar(
                                "SELECT reg_seq FROM users WHERE id = $1",
                            )
                            .bind(user_id)
                            .fetch_one(&state.pool)
                            .await
                            .unwrap_or(0);
                            format!("@{username} 校验成功，欢迎您加入 CAMDA，您是全服第 {reg_seq} 位车手！请返回模块直接点击登录。")
                        }
                        Err((status, msg)) => {
                            tracing::warn!("建号失败 {status}: {msg}（code={code}）");
                            msg
                        }
                    }
                }
            }
        }
    };
    send_group_reply(state, group.to_string(), event_id, reply).await;
}

/// 重置密码：用户名 → 一次性码（30 分钟）→ 被动回复。
async fn handle_reset_password(state: &App, event_id: &str, username: &str, group: &str) {
    let reply = match auth_handlers::create_reset_code(&state.pool, username).await {
        Ok(code) => format!("重置码已生成：{code}（30 分钟内有效，请勿泄露）。请在围场页用「忘记密码」提交新密码"),
        Err(e) => e.to_string(),
    };
    send_group_reply(state, group.to_string(), event_id, reply).await;
}

// ---------- C2C 单聊处理（私聊；回复走 /v2/users/{openid}/messages，60min 窗/4 次） ----------

#[derive(Deserialize)]
struct C2cMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    author: Value,
}

async fn handle_c2c_message(state: &App, event_id: &str, d: &Value) {
    let Ok(msg) = serde_json::from_value::<C2cMessage>(d.clone()) else {
        tracing::warn!("[qq_bot] C2C_MESSAGE payload 解析失败，忽略");
        return;
    };
    // 文档：C2C 场景 User 结构 user_openid 必填，author.id 同为用户 openid——双取兜底。
    let user_openid = msg
        .author
        .get("user_openid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| msg.author.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let content = msg.content.trim();
    if content.is_empty() || user_openid.is_empty() {
        return;
    }
    // 被动回复用 d.id（消息 ID）；事件外层 id 仅兜底。
    let msg_id = if msg.id.is_empty() { event_id.to_string() } else { msg.id };
    // 与群消息同款归一化：#（全角）与空白容忍——客户端话题渲染会在 # 前插空格
    let normalized = content.replace('＃', "#");
    let reply: String = if normalized.contains("申请围场通行证") {
        // 单聊场景没有 member_openid（群内身份），注册校验必须在群里完成：
        // 提示用户去群里发，避免单聊绑定失败却给含糊回复。
        "注册校验需要在群内完成：请在群里发送「申请围场通行证#校验码」".to_string()
    } else if let Some(idx) = normalized.find("重置密码") {
        let name = normalized[idx + "重置密码".len()..].trim();
        match auth_handlers::create_reset_code(&state.pool, name.trim()).await {
            Ok(code) => format!("重置码已生成：{code}（30 分钟内有效，请勿泄露）。请在围场页用「忘记密码」提交新密码"),
            Err(e) => e.to_string(),
        }
    } else {
        "支持指令：\n重置密码 用户名 —— 获取密码重置码".to_string()
    };
    send_c2c_reply(state, user_openid, &msg_id, reply).await;
}

// ---------- 发送队列（防频控：串行 worker，未认证 30/qpm → 2s 间隔足够） ----------

pub struct SendJob {
    /// 群消息 = group_openid；单聊 = user_openid（按 scene 路由到对应 API）
    pub target_openid: String,
    pub msg_id: String,
    pub content: String,
    pub scene: Scene,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Scene {
    Group,
    C2c,
}

/// 启动 bot 发送队列 worker。凭据从 configs 表动态读取（管理端改配置无需重启）。
pub fn run_sender(pool: PgPool, mut rx: mpsc::Receiver<SendJob>) {
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let r = match job.scene {
                Scene::Group => send_message(&pool, &job, true).await,
                Scene::C2c => send_message(&pool, &job, false).await,
            };
            if let Err(e) = r {
                tracing::error!("[qq_bot] 发送失败: {e:#} (msg_id={})", job.msg_id);
            }
            // 2 秒间隔：30/qpm 频控内绝对安全
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

async fn send_group_reply(state: &App, group: String, msg_id: &str, content: String) {
    if group.is_empty() {
        tracing::warn!("[qq_bot] 消息缺 group_openid，回复丢弃: {content}");
        return;
    }
    let Some(tx) = &state.bot.tx else {
        tracing::warn!("[qq_bot] 发送队列未启用，回复丢弃: {content}");
        return;
    };
    let _ = tx
        .send(SendJob {
            target_openid: group,
            msg_id: msg_id.to_string(),
            content,
            scene: Scene::Group,
        })
        .await;
}

/// C2C 被动回复入队（60 分钟窗 / 每消息 4 次）。
async fn send_c2c_reply(state: &App, user_openid: String, msg_id: &str, content: String) {
    if user_openid.is_empty() {
        tracing::warn!("[qq_bot] 消息缺 user_openid，回复丢弃: {content}");
        return;
    }
    let Some(tx) = &state.bot.tx else {
        tracing::warn!("[qq_bot] 发送队列未启用，回复丢弃: {content}");
        return;
    };
    let _ = tx
        .send(SendJob {
            target_openid: user_openid,
            msg_id: msg_id.to_string(),
            content,
            scene: Scene::C2c,
        })
        .await;
}

/// 调用 QQ REST API 发消息（被动回复：带 msg_id）。group=true 走群接口，否则单聊。
async fn send_message(pool: &PgPool, job: &SendJob, group: bool) -> anyhow::Result<()> {
    let app_id = get_cfg(pool, "bot_app_id")
        .await
        .ok_or_else(|| anyhow::anyhow!("bot_app_id 未配置"))?;
    let secret = get_cfg(pool, "bot_app_secret")
        .await
        .ok_or_else(|| anyhow::anyhow!("bot_app_secret 未配置"))?;
    let access_token = get_access_token(&app_id, &secret).await?;

    let url = if group {
        format!(
            "https://api.sgroup.qq.com/v2/groups/{}/messages",
            job.target_openid
        )
    } else {
        format!(
            "https://api.sgroup.qq.com/v2/users/{}/messages",
            job.target_openid
        )
    };
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        // 官方鉴权格式固定为 "QQBot {access_token}"（非 Bearer，见 api-use.html）
        .header("Authorization", format!("QQBot {access_token}"))
        .header("X-Union-Appid", &app_id)
        .json(&json!({
            "msg_type": 0,
            "content": job.content,
            "msg_id": job.msg_id,
            "msg_seq": 1,
        }))
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        anyhow::bail!("QQ API {status}: {body}");
    }
    tracing::info!("[qq_bot] 消息已发送 → {body}");
    Ok(())
}

/// 获取 access_token：appid + secret → 临时票据（含 expires_in，简单做法：不为 token 做缓存——
/// 群消息低频（≤30/qpm），每条消息获取一次也能接受）。若后续频控吃紧，加 moka 缓存。
async fn get_access_token(app_id: &str, secret: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let mut res = None;
    // api.sgroup.qq.com 域名轮询（官方建议：失败换 api.sgroup.qq.com 备用域名）
    for host in ["https://bots.qq.com/app/getAppAccessToken"] {
        let resp = client
            .post(host)
            .json(&json!({
                "appId": app_id,
                "clientSecret": secret,
            }))
            .send()
            .await?;
        let v: Value = resp.json().await?;
        if let Some(tok) = v.get("access_token").and_then(Value::as_str) {
            return Ok(tok.to_string());
        }
        tracing::warn!("[qq_bot] 获取 access_token 失败: {v}");
        res = Some(v);
    }
    anyhow::bail!("获取 access_token 两次失败: {:?}", res)
}

// ---------- configs 读写 ----------

pub async fn get_cfg(pool: &PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM configs WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub async fn set_cfg(pool: &PgPool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO configs (key, value, updated_at) VALUES ($1,$2,now()) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub fn router() -> Router<App> {
    Router::new()
        .route("/webhook", post(webhook))
        .route("/", get(|| async { "ok" }))
}