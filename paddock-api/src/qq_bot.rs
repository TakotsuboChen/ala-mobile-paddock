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

use crate::state::AppState as App;

// ---------- 平台中立部分：全部来自 bot_core ----------
//
// 规则引擎、内置动作、模板渲染、播报判定、configs 读写都搬到了 [`crate::bot_core`]，
// QQ 与 Telegram 两个适配层共用同一份实现——这是「交互逻辑完全一致」的落点。
// 本文件只保留 QQ 特有的部分：Ed25519 验签、QQ REST 收发、群信息拉取、播报目标群管理。

pub use crate::bot_core::{
    action_metas, get_cfg, is_action_key, load_rules, load_rules_json, load_rules_raw,
    save_rules, set_cfg, BotRule,
};

/// 主动播报入口（`laps.rs` / `admin.rs` 的既有调用点，签名不变）。
pub use crate::bot_core::{
    broadcast_lap_change, broadcast_lap_change_if_changed,
};

// ---------- 播报目标群（webhook 自动登记 bot 能见到的群，管理端勾选） ----------

const BCAST_GROUPS_CFG_KEY: &str = "bot_broadcast_groups";
/// 群名缓存：group_openid → "群名称|member_num"（拿不到名称时名称段为空）。
const GROUP_NAMES_CFG_KEY: &str = "bot_group_names";
/// 群名人工覆盖：group_openid → 自定义显示名（管理端设置），优先于 API 缓存。
const GROUP_NAMES_CUSTOM_CFG_KEY: &str = "bot_group_names_custom";

/// webhook 收到群消息时登记该群（去重）并异步拉取群名称（/v2/groups/{id}/info，
/// 30 QPM；11253 白名单权限不足时静默保留 openid 显示）。
pub async fn remember_group(pool: &PgPool, group_openid: &str) {
    if group_openid.is_empty() {
        return;
    }
    let mut groups = known_groups(pool).await;
    if !groups.iter().any(|g| g == group_openid) {
        groups.push(group_openid.to_string());
        let _ = set_cfg(pool, "bot_known_groups", &groups.join(",")).await;
        // 新群出现：拉一次群信息（失败不影响登记）
        fetch_group_info(pool, group_openid).await;
    }
}

/// 拉取群基本信息并缓存（名称 + 人数）。失败只记日志（多为 11253 白名单未开放）。
async fn fetch_group_info(pool: &PgPool, group_openid: &str) {
    let Some(app_id) = get_cfg(pool, "bot_app_id").await else { return };
    let Some(secret) = get_cfg(pool, "bot_app_secret").await else { return };
    let Ok(token) = get_access_token(&app_id, &secret).await else {
        tracing::warn!("[qq_bot] 拉群信息跳过：获取 access_token 失败");
        return;
    };
    let url = format!("https://api.sgroup.qq.com/v2/groups/{group_openid}/info");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("QQBot {token}"))
        .header("X-Union-Appid", &app_id)
        .send()
        .await;
    let Ok(resp) = resp else { return };
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        tracing::warn!("[qq_bot] 群信息 {status}: {v}（11253 = 白名单权限未开放）");
        return;
    }
    let name = v.get("group_name").and_then(|x| x.as_str()).unwrap_or("");
    let member = v.get("group_member_num").and_then(|x| x.as_i64()).unwrap_or(0);
    if !name.is_empty() {
        let mut names = group_names_map(pool).await;
        names.insert(group_openid.to_string(), format!("{name}|{member}"));
        if let Ok(s) = serde_json::to_string(&names) {
            let _ = set_cfg(pool, GROUP_NAMES_CFG_KEY, &s).await;
        }
    }
}

/// 群名缓存表（openid → "名称|人数"）。
async fn group_names_map(pool: &PgPool) -> std::collections::HashMap<String, String> {
    get_cfg(pool, GROUP_NAMES_CFG_KEY)
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 群名人工覆盖表（openid → 自定义名）。
pub async fn group_names_custom_map(pool: &PgPool) -> std::collections::HashMap<String, String> {
    get_cfg(pool, GROUP_NAMES_CUSTOM_CFG_KEY)
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 保存群名人工覆盖（openid → 名称；空名 = 删除该条覆盖，回落 API 缓存）。
/// 只允许覆盖已知群；保存前 trim，全空表删键省存储。
pub async fn set_group_name_custom(pool: &PgPool, openid: &str, name: &str) -> Result<(), String> {
    let openid = openid.trim();
    let known = known_groups(pool).await;
    if !known.iter().any(|k| k == openid) {
        return Err("群不在已知列表中".into());
    }
    let mut map = group_names_custom_map(pool).await;
    let name = name.trim();
    if name.is_empty() {
        map.remove(openid);
    } else {
        if name.chars().count() > 32 {
            return Err("名称最长 32 字符".into());
        }
        map.insert(openid.to_string(), name.to_string());
    }
    let val = if map.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&map).map_err(|e| e.to_string())?
    };
    set_cfg(pool, GROUP_NAMES_CUSTOM_CFG_KEY, &val)
        .await
        .map_err(|e| e.to_string())
}

/// 群名缓存（管理端 API 用）：自定义覆盖与 API 缓存分键返回，前端按 自定义 > 缓存 优先显示。
pub async fn group_names_public(
    pool: &PgPool,
) -> (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, String>,
) {
    (
        group_names_custom_map(pool).await,
        group_names_map(pool).await,
    )
}

/// bot 见过的全部群（设置页选择列表数据源）。
pub async fn known_groups(pool: &PgPool) -> Vec<String> {
    get_cfg(pool, "bot_known_groups")
        .await
        .map(|s| {
            s.split(',')
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 勾选的播报目标群列表。
pub async fn broadcast_groups(pool: &PgPool) -> Vec<String> {
    get_cfg(pool, BCAST_GROUPS_CFG_KEY)
        .await
        .map(|s| {
            s.split(',')
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// 主动群消息（无 msg_id）。发送队列 worker 复用 send_message。
pub async fn send_direct_group(state: &App, group: &str, content: &str) {
    let Some(tx) = &state.bot.tx else {
        return;
    };
    let _ = tx
        .send(SendJob {
            target_openid: group.to_string(),
            msg_id: String::new(),
            ref_msg_id: String::new(),
            content: content.to_string(),
            scene: Scene::Group,
        })
        .await;
}

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

// ---------- 群消息处理（QQ 适配层：解析 → 委托 bot_core → 入队） ----------

#[derive(Deserialize)]
struct GroupMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    group_openid: String,
    /// 事件的 member_openid 在 author 对象内（d.author.member_openid），不在 d 顶层；
    /// username = 发言者群昵称（QQ 客户端展示名，@ 变量数据源）。
    #[serde(default)]
    author: Value,
    /// 引用回复所需的 REFIDX 消息索引在 message_scene.ext 数组的 "msg_idx=REFIDX_xxx" 项里
    #[serde(default)]
    message_scene: Value,
}

impl GroupMessage {
    fn member_openid(&self) -> &str {
        self.author
            .get("member_openid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.author.get("id").and_then(Value::as_str).unwrap_or(""))
    }

    /// 发言者昵称（全量模式 author.username；取不到回退 member_openid，再不行空串）。
    fn qq_name(&self) -> String {
        self.author
            .get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.member_openid().to_string())
    }

    /// 被引用消息 ID（引用回复用）：message_scene.ext 里的 "msg_idx=REFIDX_xxx"。
    fn ref_msg_id(&self) -> String {
        self.message_scene
            .get("ext")
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter().filter_map(Value::as_str).find_map(|s| {
                    s.strip_prefix("msg_idx=").map(|v| v.to_string())
                })
            })
            .filter(|v| v.starts_with("REFIDX_") && !v.is_empty())
            .unwrap_or_default()
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
    let ref_id = msg.ref_msg_id();

    // 群登记：bot 能收到消息的群记录下来，设置页"播报目标群"选择器数据源
    remember_group(&state.pool, &msg.group_openid).await;

    let qq_name = msg.qq_name();
    crate::applog::log_event(
        &state.pool, "info", "bot", "group_msg", &qq_name,
        format!("群消息：{content}"),
        json!({"group": msg.group_openid, "platform": "qq"}),
    );

    // 决策全部委托 bot_core（与 Telegram 共用同一份规则引擎与动作实现）
    let reply = crate::bot_core::decide_reply(
        state,
        crate::bot_core::Platform::Qq,
        content,
        &qq_name,
        msg.member_openid(),
    )
    .await;
    if let Some(reply) = reply {
        send_group_reply(state, msg.group_openid.clone(), &msg_id, &ref_id, reply).await;
    }
    // 无规则命中 / 静默路径：bot 不回复
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
    #[serde(default)]
    message_scene: Value,
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
    let ref_id = msg
        .message_scene
        .get("ext")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().filter_map(Value::as_str).find_map(|s| {
                s.strip_prefix("msg_idx=").map(|v| v.to_string())
            })
        })
        .filter(|v| v.starts_with("REFIDX_"))
        .unwrap_or_default();
    // 与群消息同款归一化：#（全角）与空白容忍——客户端话题渲染会在 # 前插空格
    let normalized = content.replace('＃', "#");
    // 单聊同走规则引擎（文案可编辑）：命中动作规则按其执行；内置动作在单聊强制引导回群
    // （单聊没有 member_openid 无法建号/反查，这是安全语义）；未命中任何规则 = 静默
    let rules = load_rules(&state.pool).await;
    let mut reply: Option<String> = None;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "reply") {
        if !crate::bot_core::rule_matches_public(r, &normalized, "") {
            continue;
        }
        reply = Some(if is_action_key(&r.action) {
            // 单聊无群身份（member_openid），三个动作一律引导回群
            crate::bot_core::fail_type_reply(
                crate::bot_core::Platform::Qq,
                r,
                crate::bot_core::FailType::NoIdentity,
                "",
                "",
            )
        } else {
            // 普通回复：单聊无 member_openid（是 user_openid，两体系不互通），围场变量空
            crate::bot_core::handle_plain_reply(
                state,
                crate::bot_core::Platform::Qq,
                r,
                "",
                "",
            )
            .await
        });
        break; // 一条消息只命中一条规则
    }
    if let Some(reply) = reply {
        send_c2c_reply(state, user_openid, &msg_id, &ref_id, reply).await;
    }
    // 未命中规则：单聊与群聊一致静默
}

// ---------- 发送队列（防频控：串行 worker，未认证 30/qpm → 2s 间隔足够） ----------

pub struct SendJob {
    /// 群消息 = group_openid；单聊 = user_openid（按 scene 路由到对应 API）
    pub target_openid: String,
    /// 被动回复凭据（5 分钟窗）；空 = 主动消息
    pub msg_id: String,
    /// 引用回复的被引用消息 ID（msg_idx=REFIDX_）；空 = 不引用
    pub ref_msg_id: String,
    pub content: String,
    pub scene: Scene,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Scene {
    Group,
    C2c,
}

impl Scene {
    fn scene_str(self) -> &'static str {
        match self {
            Scene::Group => "群",
            Scene::C2c => "单聊",
        }
    }
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

async fn send_group_reply(state: &App, group: String, msg_id: &str, ref_msg_id: &str, content: String) {
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
            ref_msg_id: ref_msg_id.to_string(),
            content,
            scene: Scene::Group,
        })
        .await;
}

/// C2C 被动回复入队（60 分钟窗 / 每消息 4 次）。
async fn send_c2c_reply(state: &App, user_openid: String, msg_id: &str, ref_msg_id: &str, content: String) {
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
            ref_msg_id: ref_msg_id.to_string(),
            content,
            scene: Scene::C2c,
        })
        .await;
}

/// 调用 QQ REST API 发消息（被动回复：带 msg_id）。group=true 走群接口，否则单聊。
/// 调用 QQ REST API 发消息。msg_id 非空 = 被动回复（5 分钟窗）；空 = 主动消息
/// （未认证额度极低，仅用于 broadcast 规则，失败只记日志）。
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
    // 提及标签 <qqbot-at-user id=".."/> 只在 markdown 通道被解析（社区框架实证：
    // bunqq-core/Gensokyo——msg_type=0 文本通道对 XML 标签原样打印，2026-09-06 四轮
    // 实测矩阵：旧<@>+md=转义、旧<@>+文本=转义、新标签+文本=原样打印，唯余新标签+md）。
    // ⚠️ markdown 与 content 字段互斥，含标签即整体走 markdown.content。
    //
    // ⚠️ **判据不能只看 @ 标签**（2026-11-03 修复）：QQ markdown 通道是**通用富文本
    // 载体**，`***粗体***`、`# 标题` 同样只有走它才渲染。旧实现只认
    // `<qqbot-at-user`，导致「纪录持有者是 Telegram 用户」时（没有 QQ openid ⇒
    // 无 @ 标签）整个播报掉进 `msg_type=0` 纯文本通道，群里看到的是
    // `***张三***` / `# 纪录快讯` 原始源码。
    // ⇒ 判据改为「含 @ 标签 **或** 含 QQ markdown 语法」。
    let has_markdown = job.content.contains("<qqbot-at-user")
        || job.content.contains("***")
        || job.content.lines().any(|l| l.starts_with("# "));
    let mut payload = if has_markdown {
        json!({
            "msg_type": 2,
            "markdown": { "content": job.content },
            "msg_seq": 1,
        })
    } else {
        json!({
            "msg_type": 0,
            "content": job.content,
            "msg_seq": 1,
        })
    };
    // 被动回复必带 msg_id；主动消息（broadcast）不带——字段缺失即主动消息语义
    if !job.msg_id.is_empty() {
        payload["msg_id"] = json!(job.msg_id);
    }
    // 引用回复：带 message_reference 让回复以引用原消息形式呈现（非裸发）
    if !job.ref_msg_id.is_empty() {
        payload["message_reference"] = json!({ "message_id": job.ref_msg_id });
    }
    let res = client
        .post(&url)
        // 官方鉴权格式固定为 "QQBot {access_token}"（非 Bearer，见 api-use.html）
        .header("Authorization", format!("QQBot {access_token}"))
        .header("X-Union-Appid", &app_id)
        .json(&payload)
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        crate::applog::log_event(
            pool, "error", "bot", "send_failed", "bot",
            format!("消息发送失败 {status}（{}）", job.scene.scene_str()),
            json!({"content": job.content, "target": job.target_openid, "resp": body}),
        );
        anyhow::bail!("QQ API {status}: {body}");
    }
    tracing::info!("[qq_bot] 消息已发送 → {body}");
    crate::applog::log_event(
        pool, "info", "bot", "send", "bot",
        format!("{}: {}", if job.msg_id.is_empty() { "主动播报" } else { "回复" }, job.content),
        json!({"target": job.target_openid}),
    );
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

pub fn router() -> Router<App> {
    Router::new()
        .route("/webhook", post(webhook))
        .route("/", get(|| async { "ok" }))
}