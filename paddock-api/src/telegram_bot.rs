//! /tg/webhook —— Telegram Bot 回调（webhook 模式）。
//!
//! ## 与 QQ 的差异（都已在实现里处理）
//!
//! - **验签**：Telegram **不做签名**。`setWebhook` 时设的 `secret_token` 会在每个
//!   请求上以 `X-Telegram-Bot-Api-Secret-Token` 原样回显 ⇒ 只做一次**常量时间
//!   header 比对**。它不覆盖请求体、不防重放，安全性完全依赖 HTTPS + token 不泄露。
//! - **投递语义 = at-least-once**：任何非 2XX 都会被重投 ⇒ 必须按 `update_id` 去重，
//!   且 handler 必须**秒回 200**，重活丢给后台。
//! - **@ 判定走 entities**：`mention`（只有 @username）与 `text_mention`（带 user id）
//!   两种实体，offset/length 是 **UTF-16 code unit**，Rust 里不能拿字节下标切。
//! - **`chat_id`/`user_id` 用 i64**：最多 52 位有效位，会超 32 位。
//! - **论坛话题**：收到带 `message_thread_id` 的消息，回复必须原样带上，
//!   否则消息落到 General 话题。
//! - **渲染走 entities 而非 parse_mode**：MarkdownV2 有 18 个必须转义的字符，
//!   现有模板里 `.`/`-`/`#`/`!` 遍地都是，一个漏转义就整条 400 丢失。
//!   走 entities 完全绕开转义解析。

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::mpsc;

use crate::{
    bot_core::{self, BroadcastVars, Platform},
    state::AppState as App,
};

// ---------- 配置键 ----------

const TOKEN_CFG_KEY: &str = "tg_bot_token";
const SECRET_CFG_KEY: &str = "tg_webhook_secret";
const BROADCAST_CHATS_CFG_KEY: &str = "tg_broadcast_chats";
const KNOWN_CHATS_CFG_KEY: &str = "tg_known_chats";

// ---------- 发送队列 ----------

/// Telegram 发送任务。
///
/// ⚠️ **不复用 QQ 的 `SendJob`**：字段语义完全不同（`chat_id` 是 i64、
/// 没有 `msg_id` 被动回复窗、`scene` 概念不存在），强行合并会写出一堆
/// `String ↔ i64` / `"" ↔ None` 的转换代码，且 `scene` 字段对 TG 无意义。
pub struct TgSendJob {
    pub chat_id: i64,
    /// 引用回复的消息 id（Telegram 用整数，不是 QQ 的 REFIDX 字符串）
    pub reply_to: Option<i64>,
    /// 论坛话题 id：收到带话题的消息时回复必须带上，否则落到 General
    pub thread_id: Option<i64>,
    pub text: String,
    /// 实体区间（字节偏移）——发送时转成 Telegram 的 UTF-16 实体
    pub entities: Vec<bot_core::Entity>,
}

/// 启动 Telegram 发送 worker。
///
/// **独立队列、独立 worker**，不与 QQ 共用：
///   - QQ 未认证 30 条/分钟 ⇒ 2 秒串行间隔
///   - TG 单聊约 1 条/秒、单群 20 条/分钟、全局约 30 条/秒 ⇒ 100ms 间隔足够
/// 共用会让 TG 回复延迟被拖到 2s/条。独立性同时保证：TG 被限流时 QQ 完全不受影响。
pub fn run_sender(pool: PgPool, mut rx: mpsc::Receiver<TgSendJob>) {
    tokio::spawn(async move {
        // 每群 20 条/分钟 = 3 秒一条的硬上限；用「每 chat 上次发送时间」做本地节流，
        // 避免依赖 429 反馈（被限流时消息已经被拒，用户看到的是丢失而非延迟）。
        let mut last_sent: std::collections::HashMap<i64, std::time::Instant> =
            std::collections::HashMap::new();
        while let Some(job) = rx.recv().await {
            const PER_CHAT_MIN: std::time::Duration = std::time::Duration::from_secs(3);
            if let Some(prev) = last_sent.get(&job.chat_id) {
                let elapsed = prev.elapsed();
                if elapsed < PER_CHAT_MIN {
                    tokio::time::sleep(PER_CHAT_MIN - elapsed).await;
                }
            }
            let r = send_message(&pool, &job).await;
            last_sent.insert(job.chat_id, std::time::Instant::now());
            match r {
                Ok(()) => {}
                Err(SendErr::RetryAfter(secs)) => {
                    tracing::warn!("[tg_bot] 429 限流，等待 {secs}s 后重试");
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                    if let Err(e) = send_message(&pool, &job).await {
                        tracing::error!("[tg_bot] 重试仍失败: {e:?}");
                    }
                }
                Err(SendErr::ChatMigrated(new_id)) => {
                    tracing::warn!("[tg_bot] 群已升级为超级群，id {} → {new_id}，重发", job.chat_id);
                    migrate_chat(&pool, job.chat_id, new_id).await;
                    let retry = TgSendJob { chat_id: new_id, ..job };
                    if let Err(e) = send_message(&pool, &retry).await {
                        tracing::error!("[tg_bot] 迁移后重发失败: {e:?}");
                    }
                }
                Err(e) => tracing::error!("[tg_bot] 发送失败: {e:?}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
}

#[derive(Debug)]
#[allow(dead_code)] // Other 的载荷只在 `{e:?}` 里打印，编译器看不到字段读取
enum SendErr {
    /// 429：`parameters.retry_after` 秒后重试
    RetryAfter(u64),
    /// 400 + `parameters.migrate_to_chat_id`：群升级为超级群，用新 id 重发
    ChatMigrated(i64),
    Other(String),
}

// ---------- 入站解析 ----------

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    message_id: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    entities: Option<Vec<Entity>>,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    chat: Chat,
    /// group → supergroup 迁移服务消息：旧群消息里带新超级群 id
    #[serde(default)]
    migrate_to_chat_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Entity {
    #[serde(rename = "type")]
    kind: String,
    offset: usize,
    length: usize,
    #[serde(default)]
    user: Option<User>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
    #[serde(default)]
    is_bot: bool,
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Chat {
    #[serde(default)]
    id: i64,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    title: Option<String>,
}

/// 匿名管理员（`@GroupAnonymousBot`）与频道转发（`777000`）不是真实用户。
/// 拿它们入库会造出无法登录的幽灵账号。
const ANON_ADMIN_ID: i64 = 1087968824;
const CHANNEL_FORWARD_ID: i64 = 777000;

// ---------- 去重 ----------

/// `update_id` 去重表：Telegram 是 at-least-once，重投会带来重复 update。
///
/// 内存环形缓冲（容量 512）足够覆盖重试窗口——进程重启后表清空，最坏情况是
/// 重启瞬间的重投被处理两次（一次重复回复，可接受；建号有唯一约束兜底）。
static SEEN_UPDATES: Mutex<VecDeque<i64>> = Mutex::new(VecDeque::new());

fn is_duplicate(update_id: i64) -> bool {
    let mut seen = match SEEN_UPDATES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if seen.contains(&update_id) {
        return true;
    }
    if seen.len() >= 512 {
        seen.pop_front();
    }
    seen.push_back(update_id);
    false
}

// ---------- webhook ----------

/// POST /tg/webhook：验 header → 秒回 200 → 后台处理。
///
/// ⚠️ **必须秒回 2XX**：任何非 2XY 都会触发 Telegram 重投。
/// 且**不能在 handler 里同步调 Bot API**——一次网络往返就可能拖过超时。
async fn webhook(
    State(state): State<App>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(secret) = bot_core::get_cfg(&state.pool, SECRET_CFG_KEY)
        .await
        .filter(|s| !s.is_empty())
    else {
        tracing::warn!("[tg_bot] webhook secret 未配置，忽略请求");
        return (StatusCode::OK, Json(json!({}))).into_response();
    };
    if !verify_secret(&secret, &headers) {
        tracing::warn!("[tg_bot] secret_token 校验失败，拒绝处理");
        return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
    }

    let Ok(update) = serde_json::from_slice::<Update>(&body) else {
        tracing::warn!("[tg_bot] 非预期 Update 结构，忽略");
        return (StatusCode::OK, Json(json!({}))).into_response();
    };
    if is_duplicate(update.update_id) {
        tracing::debug!("[tg_bot] 重复 update {}，忽略", update.update_id);
        return (StatusCode::OK, Json(json!({}))).into_response();
    }

    // 后台处理：立即返回 200，避免拖过 Telegram 的超时导致重投
    let st = state.clone();
    tokio::spawn(async move {
        handle_update(&st, update).await;
    });
    (StatusCode::OK, Json(json!({}))).into_response()
}

/// 常量时间比较（避免按字符提前返回泄露前缀匹配长度）。
fn verify_secret(expected: &str, headers: &HeaderMap) -> bool {
    let Some(got) = headers
        .get("X-Telegram-Bot-Api-Secret-Token")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let a = expected.as_bytes();
    let b = got.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn handle_update(state: &App, update: Update) {
    let Some(msg) = update.message else { return };

    // group → supergroup 迁移：旧群消息里带新 id，更新映射
    if let Some(new_id) = msg.migrate_to_chat_id {
        tracing::info!("[tg_bot] 群 {} 升级为超级群 → {new_id}", msg.chat.id);
        migrate_chat(&state.pool, msg.chat.id, new_id).await;
        return;
    }

    let Some(from) = msg.from else { return };
    // 三类假身份不入库：匿名管理员 / 频道转发 / 其他 bot
    if from.is_bot || from.id == ANON_ADMIN_ID || from.id == CHANNEL_FORWARD_ID {
        return;
    }
    // 只处理群 / 超级群；私聊走「引导回群」语义（与 QQ 的 C2C 分支一致）
    let is_group = msg.chat.kind == "group" || msg.chat.kind == "supergroup";

    let Some(text) = msg.text.as_deref() else { return }; // 非文本消息（图片/贴纸等）
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    // @ bot 判定：先剥掉 @ 提及，再做关键词匹配（与 QQ 的 content 不含占位符语义对齐）
    let (content, at_bot) = strip_bot_mention(state, text, msg.entities.as_deref()).await;
    let content = content.trim();
    if content.is_empty() {
        return;
    }
    // 群里没 @ bot 时，只有「裸触发词」也响应（与 QQ 全量消息模式一致）。
    // ⚠️ 这里刻意不因 at_bot 为假就丢弃：QQ 侧同样是「收到全部消息 → 规则匹配」。
    let _ = at_bot;

    let identity_id = from.id.to_string();
    let display_name = if from.first_name.is_empty() {
        from.username.clone().unwrap_or_default()
    } else {
        from.first_name.clone()
    };

    // 群登记：bot 能收到消息的群记录下来，管理端「播报目标群」数据源
    if is_group {
        remember_chat(&state.pool, msg.chat.id, msg.chat.title.as_deref()).await;
    }

    // @username 被动补录：Telegram 每条消息的 `from.username` 都带当前值，
    // 顺手写回 users 表 ⇒ **存量用户下次发言即自动补上，无需重新注册**。
    // username 可能为空（用户没设置）或变更（用户随时可改），每次发言都以最新值为准。
    if let Some(un) = from.username.as_deref().filter(|u| !u.is_empty()) {
        update_telegram_username(&state.pool, from.id, un).await;
    }

    crate::applog::log_event(
        &state.pool, "info", "bot", "group_msg", &display_name,
        format!("{} 消息：{content}", if is_group { "群" } else { "私聊" }),
        json!({"chat": msg.chat.id, "platform": "tg"}),
    );

    // 私聊：三个内置动作一律引导回群（与 QQ 的 C2C 语义一致）
    if !is_group {
        let rules = bot_core::load_rules(&state.pool).await;
        for r in rules.iter().filter(|r| r.enabled && r.kind == "reply") {
            let normalized = content.replace('＃', "#");
            if !bot_core::rule_matches_public(r, &normalized, "") {
                continue;
            }
            let reply = if bot_core::is_action_key(&r.action) {
                // 私聊没有群身份，无法建号/按身份反查：引导回群
                bot_core::fail_type_reply(
                    Platform::Telegram,
                    r,
                    bot_core::FailType::NoIdentity,
                    "",
                    "",
                )
            } else {
                bot_core::handle_plain_reply(state, Platform::Telegram, r, "", "").await
            };
            send_reply(state, msg.chat.id, msg.message_id, msg.message_thread_id, reply).await;
            return;
        }
        return;
    }

    // 群聊：统一规则引擎（与 QQ 完全同一套决策逻辑）
    let reply = bot_core::decide_reply(state, Platform::Telegram, content, &display_name, &identity_id)
        .await;
    if let Some(reply) = reply {
        send_reply(state, msg.chat.id, msg.message_id, msg.message_thread_id, reply).await;
    }
}

/// 剥离 @bot 提及，返回（纯文本, 是否 @ 了 bot）。
///
/// ⚠️ **必须按实体的 UTF-16 offset/length 精确切片**，不能用 `text.contains("@name")`：
/// 后者会把 `@mybot2`、`@mybot_extra` 误判成提及，也会被大小写绊倒
/// （Telegram 用户名大小写不敏感）。
async fn strip_bot_mention(state: &App, text: &str, entities: Option<&[Entity]>) -> (String, bool) {
    let Some(entities) = entities else {
        return (text.to_string(), false);
    };
    let Some(token) = bot_core::get_cfg(&state.pool, TOKEN_CFG_KEY).await else {
        return (text.to_string(), false);
    };
    // token 形如 "110201874:AAHdq..."，前半段就是 bot 的 user id
    let bot_id: Option<i64> = token.split(':').next().and_then(|s| s.parse().ok());
    let bot_username = bot_core::get_cfg(&state.pool, "tg_bot_username").await;

    let units: Vec<u16> = text.encode_utf16().collect();
    // 收集要删除的 UTF-16 区间
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut at_bot = false;
    for e in entities {
        let end = e.offset.saturating_add(e.length).min(units.len());
        if e.offset >= end {
            continue;
        }
        match e.kind.as_str() {
            "text_mention" => {
                if let (Some(u), Some(bid)) = (&e.user, bot_id) {
                    if u.id == bid {
                        at_bot = true;
                        cuts.push((e.offset, end));
                    }
                }
            }
            "mention" => {
                // mention 实体不带 user id，只能切出 @username 再比
                let slice = String::from_utf16_lossy(&units[e.offset..end]);
                let name = slice.trim_start_matches('@');
                if let Some(bu) = &bot_username {
                    if !bu.is_empty() && name.eq_ignore_ascii_case(bu) {
                        at_bot = true;
                        cuts.push((e.offset, end));
                    }
                }
            }
            _ => {}
        }
    }
    if cuts.is_empty() {
        return (text.to_string(), at_bot);
    }
    cuts.sort_by_key(|c| c.0);
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut pos = 0;
    for (s, e) in cuts {
        if s > pos {
            out.extend_from_slice(&units[pos..s]);
        }
        pos = e;
    }
    if pos < units.len() {
        out.extend_from_slice(&units[pos..]);
    }
    (String::from_utf16_lossy(&out), at_bot)
}

// ---------- 发送 ----------

async fn send_reply(
    state: &App,
    chat_id: i64,
    reply_to: i64,
    thread_id: Option<i64>,
    text: String,
) {
    enqueue(state, TgSendJob {
        chat_id,
        reply_to: Some(reply_to),
        thread_id,
        text,
        entities: Vec::new(),
    })
    .await;
}

/// 主动播报入队（渲染由 `bot_core` 按 TG 语法做，含 @ 实体）。
pub async fn send_direct_chat(state: &App, chat_id: &str, template: &str, vars: &BroadcastVars) {
    let Ok(cid) = chat_id.trim().parse::<i64>() else {
        tracing::warn!("[tg_bot] 播报目标 chat_id 非法: {chat_id}");
        return;
    };
    let rendered = bot_core::render_broadcast_for(Platform::Telegram, template, vars);
    enqueue(state, TgSendJob {
        chat_id: cid,
        reply_to: None,
        thread_id: None,
        text: rendered.text,
        entities: rendered.entities,
    })
    .await;
}

async fn enqueue(state: &App, job: TgSendJob) {
    let Some(tx) = &state.bot.tg_tx else {
        tracing::warn!("[tg_bot] 发送队列未启用，消息丢弃: {}", job.text);
        return;
    };
    // ⚠️ 用 `send().await`（队列满时阻塞等待）而不是 `try_send`——
    // 后者在满队列时立即返回 Err(Full)，被吞掉就是静默丢消息。
    let _ = tx.send(job).await;
}

/// 调 Telegram Bot API 发消息。走 `entities` 而非 `parse_mode`。
async fn send_message(pool: &PgPool, job: &TgSendJob) -> Result<(), SendErr> {
    let token = bot_core::get_cfg(pool, TOKEN_CFG_KEY)
        .await
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SendErr::Other("tg_bot_token 未配置".into()))?;

    // 4096 字符上限（entities 解析后计）。保守按 UTF-16 长度切——对中文/ASCII
    // 等价于按码点，对 emoji 会切得早一点但永不超限（安全方向的错误）。
    for chunk in split_message(&job.text, &job.entities, 4000) {
        let payload = build_payload(job, &chunk);
        // 诊断：实体是否正确生成（entities 缺失 = 格式不生效，是排查第一落点）
        tracing::info!(
            "[tg_bot] payload entities={} text_len={}",
            payload.get("entities").map(|v| v.to_string()).unwrap_or_else(|| "NONE".into()),
            chunk.text.len()
        );
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| SendErr::Other(format!("网络错误: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);

        if status.is_success() && body.get("ok").and_then(Value::as_bool) == Some(true) {
            tracing::info!("[tg_bot] 消息已发送 → chat {}", job.chat_id);
            crate::applog::log_event(
                pool, "info", "bot", "send", "bot",
                format!("Telegram: {}", chunk.text),
                json!({"target": job.chat_id, "platform": "tg"}),
            );
            continue;
        }

        // 429：parameters.retry_after
        if status.as_u16() == 429 {
            let secs = body
                .pointer("/parameters/retry_after")
                .and_then(Value::as_u64)
                .unwrap_or(3);
            return Err(SendErr::RetryAfter(secs));
        }
        // 400 + parameters.migrate_to_chat_id：群升级为超级群
        if let Some(new_id) = body
            .pointer("/parameters/migrate_to_chat_id")
            .and_then(Value::as_i64)
        {
            return Err(SendErr::ChatMigrated(new_id));
        }

        crate::applog::log_event(
            pool, "error", "bot", "send_failed", "bot",
            format!("Telegram 消息发送失败 {status}"),
            json!({"content": chunk.text, "target": job.chat_id, "resp": body, "platform": "tg"}),
        );
        return Err(SendErr::Other(format!("{status}: {body}")));
    }
    Ok(())
}

/// 一段消息（正文 + 该段自己的实体）。
struct Chunk {
    text: String,
    /// (实体类型, UTF-16 段内偏移, UTF-16 长度)——Telegram 的实体区间单位
    entities: Vec<(bot_core::EntityKind, usize, usize)>,
}

/// 按 UTF-16 长度上限切分，每段**重算** mention 偏移。
///
/// 切点优先落在换行处；不切在 surrogate pair 中间（emoji 会被劈成乱码）。
///
/// ⚠️ **每段的实体必须按该段重算**：Telegram 的 offset 是相对本条消息正文的，
/// 直接把原始偏移套到第二段上会指向错误位置（甚至越界）。
fn split_message(text: &str, entities: &[bot_core::Entity], max_units: usize) -> Vec<Chunk> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_units {
        // ⚠️ 单段路径也必须做字节 → UTF-16 换算：上游 `bot_core` 给的是**字节**偏移，
        // 而 Telegram 要的是 UTF-16 code unit。中文 1 字 = 3 字节但只有 1 unit，
        // 直接把字节偏移塞进去会让实体整体错位（测试 `payload_entities_are_utf16_offsets` 抓过）。
        let converted = entities
            .iter()
            .map(|e| {
                let u_off = utf16_len(&text[..e.off]);
                let u_len = utf16_len(&text[e.off..e.off + e.len]);
                (e.kind, u_off, u_len)
            })
            .collect();
        return vec![Chunk {
            text: text.to_string(),
            entities: converted,
        }];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < units.len() {
        let mut end = (start + max_units).min(units.len());
        if end < units.len() {
            // 不要在 surrogate pair 中间切
            if (0xD800..0xDC00).contains(&units[end - 1]) && (0xDC00..0xE000).contains(&units[end]) {
                end -= 1;
            }
            // 优先在换行处切
            if let Some(nl) = units[start..end].iter().rposition(|&u| u == b'\n' as u16) {
                if nl > max_units / 2 {
                    end = start + nl + 1;
                }
            }
        }
        // 该段的 UTF-16 范围 [start, end) → 只保留完全落在段内的 mention，偏移改为段内相对值
        let seg_units = &units[start..end];
        let seg_entities = entities
            .iter()
            .filter_map(|e| {
                // 字节偏移 → 该段内的 UTF-16 偏移
                let u_off = utf16_len(&text[..e.off]);
                let u_len = utf16_len(&text[e.off..e.off + e.len]);
                let u_end = u_off + u_len;
                // 只保留整段落在本段内的实体（跨段的丢弃：被切断的标记本来也无意义）
                if u_off >= start && u_end <= end {
                    Some((e.kind, u_off - start, u_len))
                } else {
                    None
                }
            })
            .collect();
        out.push(Chunk {
            text: String::from_utf16_lossy(seg_units),
            entities: seg_entities,
        });
        start = end;
    }
    out
}

fn build_payload(job: &TgSendJob, chunk: &Chunk) -> Value {
    let mut p = json!({
        "chat_id": job.chat_id,
        "text": chunk.text,
        // 关闭链接预览卡片（链接仍可点）——播报里的 URL 不该撑出大卡片
        "link_preview_options": { "is_disabled": true },
    });
    if let Some(rid) = job.reply_to {
        // allow_sending_without_reply：目标消息被删时仍发送，避免竞态导致整条失败
        p["reply_parameters"] = json!({
            "message_id": rid,
            "allow_sending_without_reply": true,
        });
    }
    if let Some(tid) = job.thread_id {
        // ⚠️ 论坛话题：不带这个字段，回复会落到 General 话题
        p["message_thread_id"] = json!(tid);
    }
    if !chunk.entities.is_empty() {
        // ⚠️ 分段的实体已经在 `split_message` 里转成 **UTF-16 段内偏移**，
        // 这里直接使用；不要再做一次字节→UTF-16 换算（那会把偏移算两遍）。
        let ents: Vec<Value> = chunk
            .entities
            .iter()
            .map(|(kind, u_off, u_len)| match kind {
                bot_core::EntityKind::Bold => json!({
                    "type": "bold",
                    "offset": u_off,
                    "length": u_len,
                }),
                bot_core::EntityKind::Mention(uid) => json!({
                    "type": "text_mention",
                    "offset": u_off,
                    "length": u_len,
                    "user": { "id": uid },
                }),
                // `@username` 形式：正文里已经有 `@xxx` 文本，实体只标记区间。
                bot_core::EntityKind::UsernameMention => json!({
                    "type": "mention",
                    "offset": u_off,
                    "length": u_len,
                }),
            })
            .collect();
        p["entities"] = json!(ents);
    }
    p
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 把发言者的当前 `@username` 写回 users 表（仅更新已有 Telegram 账号）。
///
/// ⚠️ **不是身份识别**：识别永远用 `telegram_id`。username 只用于播报时生成
/// `@xxx` 形式的提及实体。为 NULL 是合法状态（用户没设 username）。
async fn update_telegram_username(pool: &PgPool, telegram_id: i64, username: &str) {
    let r = sqlx::query(
        "UPDATE users SET telegram_username = $2 \
         WHERE telegram_id = $1 AND (telegram_username IS DISTINCT FROM $2)",
    )
    .bind(telegram_id)
    .bind(username)
    .execute(pool)
    .await;
    match r {
        Ok(o) if o.rows_affected() > 0 => {
            tracing::info!("[tg_bot] 补录 @username：{telegram_id} → @{username}");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("[tg_bot] 补录 @username 失败: {e}"),
    }
}

/// 主动补查存量用户的 `@username`（管理端「检查状态」时调用）。
///
/// 场景：用户注册后一直没在群里发言（或发言时没设 username），播报只能退回
/// 蓝字形式。这里遍历库里已知的 Telegram 账号，逐个 `getChatMember` 拉最新
/// username。**失败不阻塞**（用户可能已退群 / bot 不是管理员）。
pub async fn backfill_usernames(pool: &PgPool) -> (usize, usize) {
    let Some(token) = bot_core::get_cfg(pool, TOKEN_CFG_KEY)
        .await
        .filter(|s| !s.is_empty())
    else {
        return (0, 0);
    };
    let chats = known_chats(pool).await;
    let users: Vec<(i64, String)> = sqlx::query_as(
        "SELECT telegram_id, username FROM users WHERE telegram_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut ok = 0usize;
    let mut fail = 0usize;
    let client = reqwest::Client::new();
    for (uid, _paddock_name) in &users {
        let mut found = None;
        for chat in &chats {
            let Ok(cid) = chat.parse::<i64>() else { continue };
            let url = format!("https://api.telegram.org/bot{token}/getChatMember");
            let resp = client
                .post(&url)
                .json(&json!({"chat_id": cid, "user_id": uid}))
                .send()
                .await;
            let Ok(resp) = resp else { continue };
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            if let Some(un) = v.pointer("/result/user/username").and_then(Value::as_str) {
                if !un.is_empty() {
                    found = Some(un.to_string());
                    break;
                }
            }
        }
        match found {
            Some(un) => {
                update_telegram_username(pool, *uid, &un).await;
                ok += 1;
            }
            None => fail += 1,
        }
    }
    (ok, fail)
}

// ---------- 群登记 ----------

/// 登记 bot 能收到消息的群（管理端「播报目标群」数据源）。
///
/// ⚠️ **Telegram 没有「我加入过哪些群」的查询接口**——只能靠收到消息被动登记。
/// 群名来自 `chat.title`（不需要额外调 getChat）。
async fn remember_chat(pool: &PgPool, chat_id: i64, title: Option<&str>) {
    let mut chats = known_chats(pool).await;
    let key = chat_id.to_string();
    if !chats.iter().any(|c| c == &key) {
        chats.push(key.clone());
        let _ = bot_core::set_cfg(pool, KNOWN_CHATS_CFG_KEY, &chats.join(",")).await;
    }
    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        let mut names: std::collections::HashMap<String, String> = bot_core::get_cfg(pool, "tg_chat_names")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if names.get(&key) != Some(&t.to_string()) {
            names.insert(key, t.to_string());
            if let Ok(s) = serde_json::to_string(&names) {
                let _ = bot_core::set_cfg(pool, "tg_chat_names", &s).await;
            }
        }
    }
}

pub async fn known_chats(pool: &PgPool) -> Vec<String> {
    split_list(bot_core::get_cfg(pool, KNOWN_CHATS_CFG_KEY).await)
}

/// 勾选的播报目标 chat 列表。
pub async fn broadcast_chats(pool: &PgPool) -> Vec<String> {
    split_list(bot_core::get_cfg(pool, BROADCAST_CHATS_CFG_KEY).await)
}

fn split_list(v: Option<String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// group → supergroup 迁移：把已知列表与播报列表里的旧 id 换成新 id。
/// ⚠️ 历史消息记录不改写，只更新「未来发送目的地」。
async fn migrate_chat(pool: &PgPool, old_id: i64, new_id: i64) {
    let old = old_id.to_string();
    let new = new_id.to_string();
    for key in [KNOWN_CHATS_CFG_KEY, BROADCAST_CHATS_CFG_KEY] {
        let list = split_list(bot_core::get_cfg(pool, key).await);
        if list.iter().any(|c| c == &old) {
            let updated: Vec<String> = list
                .into_iter()
                .map(|c| if c == old { new.clone() } else { c })
                .collect();
            let _ = bot_core::set_cfg(pool, key, &updated.join(",")).await;
        }
    }
}

// ---------- 管理端操作 ----------

/// 保存 token 时调用 `setWebhook`。返回 Telegram 的原始响应供页面回显。
pub async fn set_webhook(token: &str, url: &str, secret: &str) -> anyhow::Result<Value> {
    let endpoint = format!("https://api.telegram.org/bot{token}/setWebhook");
    let resp = reqwest::Client::new()
        .post(&endpoint)
        .json(&json!({
            "url": url,
            "secret_token": secret,
            "allowed_updates": ["message"],
            // 显式声明，避免沿用上次设置
        }))
        .send()
        .await?;
    Ok(resp.json().await?)
}

/// `getWebhookInfo`：排障用（`last_error_message` 直接暴露 401/403 原因）。
/// ⚠️ **不要用 `getUpdates` 探活**——那会造成 409 并把生产 webhook 踢掉。
pub async fn get_webhook_info(token: &str) -> anyhow::Result<Value> {
    let endpoint = format!("https://api.telegram.org/bot{token}/getWebhookInfo");
    let resp = reqwest::Client::new().get(&endpoint).send().await?;
    Ok(resp.json().await?)
}

/// `getMe`：取 bot 自身信息。`can_read_all_group_messages` 是隐私模式的可编程自检
/// （`true` = 隐私模式已关闭，能收到群内全部消息）。
pub async fn get_me(token: &str) -> anyhow::Result<Value> {
    let endpoint = format!("https://api.telegram.org/bot{token}/getMe");
    let resp = reqwest::Client::new().get(&endpoint).send().await?;
    Ok(resp.json().await?)
}

// ---------- 路由 ----------

pub fn router() -> Router<App> {
    Router::new()
        .route("/webhook", post(webhook))
        // GET 探活：Telegram 没有 QQ 那种 op=13 握手，这是唯一能区分
        // 「反代通了但 webhook 没注册」与「反代根本不通」的手段。
        .route("/", get(|| async { "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-Telegram-Bot-Api-Secret-Token", v.parse().unwrap());
        h
    }

    #[test]
    fn secret_matches_exactly() {
        assert!(verify_secret("abc123", &hdr("abc123")));
    }

    #[test]
    fn secret_rejects_wrong_value() {
        assert!(!verify_secret("abc123", &hdr("abc124")));
    }

    #[test]
    fn secret_rejects_different_length() {
        // 长度不同必须直接拒绝（且不 panic）
        assert!(!verify_secret("abc123", &hdr("abc12")));
        assert!(!verify_secret("abc123", &hdr("abc1234")));
    }

    #[test]
    fn secret_rejects_missing_header() {
        assert!(!verify_secret("abc123", &HeaderMap::new()));
    }

    #[test]
    fn duplicate_update_detected() {
        // 用一个不常见的 id 避免与其他测试互相污染
        let id = 987_654_321_i64;
        assert!(!is_duplicate(id), "首次不算重复");
        assert!(is_duplicate(id), "第二次应判重复");
    }

    #[test]
    fn short_message_not_split() {
        let chunks = split_message("短消息", &[], 4000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "短消息");
    }

    #[test]
    fn long_message_split_under_limit() {
        let text = "字".repeat(9000);
        let chunks = split_message(&text, &[], 4000);
        assert!(chunks.len() >= 3, "9000 字至少切成 3 段");
        for c in &chunks {
            assert!(c.text.encode_utf16().count() <= 4000, "每段不得超限");
        }
        assert_eq!(chunks.iter().map(|c| c.text.len()).sum::<usize>(), text.len());
    }

    #[test]
    fn split_prefers_newline() {
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("第{i}行内容比较长一些\n"));
        }
        let chunks = split_message(&text, &[], 200);
        assert!(chunks.len() > 1);
        // 除最后一段外，都应以换行结尾（说明切在了换行处）
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.text.ends_with('\n'), "应在换行处切：{:?}", &c.text[c.text.len()-10..]);
        }
    }

    #[test]
    fn mention_offsets_recomputed_per_chunk() {
        // 构造一条带 mention 的超长消息：mention 落在第 2 段
        let pad = "x".repeat(4100);
        let text = format!("{pad}张三 来了");
        let byte_off = pad.len();
        let entities = vec![bot_core::Entity {
            kind: bot_core::EntityKind::Mention(12345),
            off: byte_off,
            len: "张三".len(),
        }];
        let chunks = split_message(&text, &entities, 4000);
        assert!(chunks.len() >= 2);
        // 第 1 段不含实体
        assert!(chunks[0].entities.is_empty());
        // 第 2 段含，且偏移是段内相对值
        let last = chunks.last().unwrap();
        assert_eq!(last.entities.len(), 1, "mention 应落在最后一段");
        let (kind, off, len) = last.entities[0];
        assert_eq!(kind, bot_core::EntityKind::Mention(12345));
        let units: Vec<u16> = last.text.encode_utf16().collect();
        let got = String::from_utf16_lossy(&units[off..off + len]);
        assert_eq!(got, "张三");
    }

    #[test]
    fn payload_carries_thread_id() {
        let job = TgSendJob {
            chat_id: -100123,
            reply_to: Some(55),
            thread_id: Some(77),
            text: "hi".into(),
            entities: vec![],
        };
        let chunk = Chunk { text: "hi".into(), entities: vec![] };
        let p = build_payload(&job, &chunk);
        // ⚠️ 论坛话题不带这个字段，回复会落到 General 话题
        assert_eq!(p["message_thread_id"], json!(77));
        assert_eq!(p["reply_parameters"]["message_id"], json!(55));
        assert_eq!(p["reply_parameters"]["allow_sending_without_reply"], json!(true));
        assert_eq!(p["link_preview_options"]["is_disabled"], json!(true));
        // 走 entities 而非 parse_mode
        assert!(p.get("parse_mode").is_none(), "不得设 parse_mode");
    }

    #[test]
    fn payload_bold_entity_has_no_user() {
        let text = "车手 张三 刷新";
        let byte_off = "车手 ".len();
        let byte_len = "张三".len();
        let job = TgSendJob {
            chat_id: 1,
            reply_to: None,
            thread_id: None,
            text: text.to_string(),
            entities: vec![bot_core::Entity {
                kind: bot_core::EntityKind::Bold,
                off: byte_off,
                len: byte_len,
            }],
        };
        let chunks = split_message(&job.text, &job.entities, 4000);
        let p = build_payload(&job, &chunks[0]);
        let ents = p["entities"].as_array().unwrap();
        assert_eq!(ents[0]["type"], json!("bold"));
        assert_eq!(ents[0]["offset"], json!(3), "「车手 」= 3 个 UTF-16 unit");
        assert_eq!(ents[0]["length"], json!(2));
        assert!(ents[0].get("user").is_none(), "bold 实体不带 user");
    }

    #[test]
    fn payload_entities_are_utf16_offsets() {
        // 从**字节**偏移（bot_core 的产物）走一遍 split_message，验证转成 UTF-16
        let text = "张三😀 好";
        let byte_off = "张三😀 ".len();
        let byte_len = "好".len();
        let job = TgSendJob {
            chat_id: 1,
            reply_to: None,
            thread_id: None,
            text: text.to_string(),
            entities: vec![bot_core::Entity {
                kind: bot_core::EntityKind::Mention(9),
                off: byte_off,
                len: byte_len,
            }],
        };
        let chunks = split_message(&job.text, &job.entities, 4000);
        assert_eq!(chunks.len(), 1);
        let chunk = &chunks[0];
        let p = build_payload(&job, chunk);
        let ents = p["entities"].as_array().unwrap();
        assert_eq!(ents[0]["type"], json!("text_mention"));
        // "张三😀 " = 2 汉字(2 unit) + emoji(2 unit) + 空格(1 unit) = 5 unit
        assert_eq!(ents[0]["offset"], json!(5), "偏移必须是 UTF-16 而非字节");
        assert_eq!(ents[0]["length"], json!(1), "一个汉字 = 1 个 UTF-16 unit");
        assert_eq!(ents[0]["user"]["id"], json!(9));
    }
}
