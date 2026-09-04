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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{api::auth_handlers, state::AppState as App};

// ---------- 消息规则引擎（管理端"消息配置"卡维护，configs 表存 JSON） ----------

/// 一条条件。field = "content"（消息内容）/ "event"（事件类型，broadcast 用）；
/// op = contains / not_contains / equals / starts / ends。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleCond {
    pub field: String,
    pub op: String,
    pub value: String,
}

/// 一条消息规则。kind = "reply"（被动回复）/ "broadcast"（主动播报）。
/// action（reply 用）= "reply" 普通回复 / "reg_code" 注册校验（建号动作）/
/// "reset_password" 密码重置（发码动作）。内置动作从触发词（keyword）后提取
/// 校验码/用户名；成功走 template，失败按类型走独立文案字段（空=内置默认）。
/// match_all = true 时条件 AND，false 时 OR；conditions 为空 = 恒命中。
/// 旧格式 keyword 迁移：读入时合成 conditions（与字段 keyword 兼容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotRule {
    pub id: String,
    pub kind: String,
    /// 内置动作的触发词/提取锚点（reg_code/reset_password 用；普通回复可空）
    #[serde(default)]
    pub keyword: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub conditions: Vec<RuleCond>,
    /// AND（true）/ OR（false）；默认 AND
    #[serde(default = "default_true")]
    pub match_all: bool,
    pub template: String,
    /// 通用失败模板（可 {{code}}/{{name}}）；仅在对应类型独立文案为空时使用
    #[serde(default)]
    pub fail_template: String,
    // ---- reg_code 专用失败文案（每类失败独立模板；空 = 内置默认） ----
    /// 锚点后提不出 #码
    #[serde(default)]
    pub no_code_template: String,
    /// 码无效/过期（含并发被用掉）
    #[serde(default)]
    pub invalid_code_template: String,
    /// 该 QQ 已有在途会话 / 已绑定过账号
    #[serde(default)]
    pub dup_openid_template: String,
    /// 平台未给群身份（member_openid 缺失）
    #[serde(default)]
    pub no_identity_template: String,
    // ---- reset_password 专用 ----
    /// 用户名不存在
    #[serde(default)]
    pub no_user_template: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

const RULES_CFG_KEY: &str = "bot_message_rules";

/// 内置预设规则（首次未配置时的默认集；保存任何规则后即被用户数据覆盖）。
/// 完整呈现内部真实语义：所有内置文案原文预填（输入框不留空）。
/// 动作规则（reg_code/reset_password）无可编辑条件——触发词即条件；
/// 播报两条：历史优先于版本，同破只播历史（与 Toast 取最高一致）。
fn preset_rules() -> Vec<BotRule> {
    vec![
        BotRule {
            id: "preset-reg".into(),
            kind: "reply".into(),
            keyword: "申请围场通行证".into(),
            action: "reg_code".into(),
            conditions: vec![],
            match_all: true,
            template: "校验成功，欢迎 {{paddock_name}} 加入，您是全服第 {{paddock_id}} 位车手！请返回模块直接点击登录。".into(),
            fail_template: String::new(),
            no_code_template: "未识别到校验码：请在「申请围场通行证」后跟 #校验码（模块端申请页可复制完整指令）".into(),
            invalid_code_template: "校验码 {{code}} 无效或已过期，请在围场页重新申请".into(),
            dup_openid_template: "该 QQ 身份已有其他注册校验在途，请勿重复申请".into(),
            no_identity_template: "无法识别你的群身份，请确认已在 QQ 群设置中允许机器人获取群信息".into(),
            no_user_template: String::new(),
            enabled: true,
        },
        BotRule {
            id: "preset-reset".into(),
            kind: "reply".into(),
            keyword: "重置密码".into(),
            action: "reset_password".into(),
            conditions: vec![],
            match_all: true,
            template: "重置码已生成：{{code}}（30 分钟内有效，请勿泄露）。请在围场页用「忘记密码」提交新密码".into(),
            fail_template: String::new(),
            no_code_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: "用户名不存在，请核对后重试".into(),
            enabled: true,
        },
        BotRule {
            id: "preset-bc-alltime".into(),
            kind: "broadcast".into(),
            keyword: String::new(),
            action: String::new(),
            conditions: vec![RuleCond {
                field: "event".into(),
                op: "equals".into(),
                value: "record_alltime".into(),
            }],
            match_all: true,
            template: "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出{{lap}}，刷新了全服历史最快圈速！".into(),
            fail_template: String::new(),
            no_code_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: String::new(),
            enabled: true,
        },
        BotRule {
            id: "preset-bc-version".into(),
            kind: "broadcast".into(),
            keyword: String::new(),
            action: String::new(),
            conditions: vec![RuleCond {
                field: "event".into(),
                op: "equals".into(),
                value: "record_version".into(),
            }],
            match_all: true,
            template: "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出{{lap}}，刷新了{{version}}版本的全服最快圈速！".into(),
            fail_template: String::new(),
            no_code_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: String::new(),
            enabled: true,
        },
    ]
}

/// 读规则列表。库中无配置或为空列表 → 返回预设集（面板默认展示、webhook 按预设渲染）。
/// 旧 keyword 格式（无 conditions）自动迁移为 conditions。
pub async fn load_rules(pool: &PgPool) -> Vec<BotRule> {
    let raw = get_cfg(pool, RULES_CFG_KEY).await;
    let mut rules: Vec<BotRule> = match raw {
        // "[]"（旧版面板曾保存过空表）也按未配置处理，保证预设永远可见
        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => Vec::new(),
    };
    if rules.is_empty() {
        rules = preset_rules();
    }
    for r in &mut rules {
        // 旧 keyword 格式迁移：普通回复/播报合成 conditions；
        // 动作规则（reg_code/reset_password）触发词即条件，不生成条件行
        if r.conditions.is_empty()
            && !r.keyword.trim().is_empty()
            && !(r.kind == "reply" && (r.action == "reg_code" || r.action == "reset_password"))
        {
            r.conditions = vec![RuleCond {
                field: "content".into(),
                op: "contains".into(),
                value: r.keyword.trim().to_string(),
            }];
            r.keyword = String::new();
        }
    }
    rules
}

/// 规则列表 → JSON 字符串（设置页模板注入用）。
pub async fn load_rules_json(pool: &PgPool) -> String {
    serde_json::to_string(&load_rules(pool).await).unwrap_or_else(|_| "[]".into())
}

/// 全量保存规则。多群播报时拒绝 @QQ 群用户名 变量（该变量只在被动回复场景有数据源）。
pub async fn save_rules(pool: &PgPool, rules: &[BotRule]) -> anyhow::Result<()> {
    set_cfg(pool, RULES_CFG_KEY, &serde_json::to_string(rules)?).await
}

fn cond_matches(cond: &RuleCond, content: &str, event: &str) -> bool {
    let target = if cond.field == "event" { event } else { content };
    let v = cond.value.as_str();
    match cond.op.as_str() {
        "not_contains" => !target.contains(v),
        "equals" => target == v,
        "starts" => target.starts_with(v),
        "ends" => target.ends_with(v),
        _ => target.contains(v), // contains 默认
    }
}

fn rule_matches(rule: &BotRule, content: &str, event: &str) -> bool {
    // 动作规则（reg_code/reset_password）：触发词即条件（词出现在消息中即命中）
    if rule.kind == "reply" && (rule.action == "reg_code" || rule.action == "reset_password") {
        let kw = rule.keyword.trim();
        return !kw.is_empty() && content.contains(kw);
    }
    // 其他规则：conditions 为空 = 恒命中（播报可配恒播）
    if rule.conditions.is_empty() {
        return true;
    }
    if rule.match_all {
        rule.conditions.iter().all(|c| cond_matches(c, content, event))
    } else {
        rule.conditions.iter().any(|c| cond_matches(c, content, event))
    }
}

/// 被动回复渲染变量：{{qq_name}}（群昵称）、{{paddock_name}}（围场用户名）、
/// {{paddock_id}}（车手 ID）、{{code}}（校验码/重置码）。
pub struct ReplyVars {
    pub qq_name: String,
    pub paddock_name: String,
    pub paddock_id: String,
    pub code: String,
}

/// 主动播报渲染变量：{{track}}、{{lap}}、{{version}}、{{paddock_name}}、{{paddock_id}}。
pub struct BroadcastVars {
    pub track: String,
    pub lap: String,
    pub version: String,
    pub paddock_name: String,
    pub paddock_id: String,
}

/// 删除场景专用：仅当删除导致该维度纪录变化（值变或易主）才播报。
/// 删非最快圈时 records 前后一致 → 静默。
pub async fn broadcast_lap_change_if_changed(
    state: &App,
    gp_index: i16,
    version_code: i32,
    before_alltime: Option<i32>,
    before_version: Option<i32>,
) {
    let cur_alltime: Option<i32> = sqlx::query_scalar(
        "SELECT lap_ms FROM records WHERE gp_index=$1 AND kind='alltime'",
    )
    .bind(gp_index)
    .fetch_one(&state.pool)
    .await
    .ok()
    .flatten();
    let cur_version: Option<i32> = sqlx::query_scalar(
        "SELECT lap_ms FROM records WHERE gp_index=$1 AND kind='version' AND version_code=$2",
    )
    .bind(gp_index)
    .bind(version_code)
    .fetch_one(&state.pool)
    .await
    .ok()
    .flatten();
    if cur_alltime == before_alltime && cur_version == before_version {
        return; // 纪录未变（删的是非最快圈）
    }
    // 变了：按当前纪录播报（历史优先于版本）
    if cur_alltime != before_alltime {
        broadcast_current_record(state, gp_index, version_code, "record_alltime").await;
    } else if cur_version != before_version {
        broadcast_current_record(state, gp_index, version_code, "record_version").await;
    }
}

/// 按当前 records 表持有者播报指定事件。
async fn broadcast_current_record(state: &App, gp_index: i16, version_code: i32, event: &str) {
    let row: Option<(i32, Uuid)> = if event == "record_alltime" {
        sqlx::query_as("SELECT lap_ms, user_id FROM records WHERE gp_index=$1 AND kind='alltime'")
            .bind(gp_index)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
    } else {
        sqlx::query_as(
            "SELECT lap_ms, user_id FROM records WHERE gp_index=$1 AND kind='version' AND version_code=$2",
        )
        .bind(gp_index)
        .bind(version_code)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
    };
    let Some((lap_ms, holder)) = row else { return };
    let (username, reg_seq): (String, i64) = sqlx::query_as(
        "SELECT username, reg_seq FROM users WHERE id=$1",
    )
    .bind(holder)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((String::new(), 0));
    broadcast(
        state,
        event,
        &BroadcastVars {
            track: crate::api::laps::track_display_name(gp_index).to_string(),
            lap: crate::api::leaderboard::format_lap_ms(lap_ms),
            version: crate::api::laps::version_display(version_code),
            paddock_name: username,
            paddock_id: reg_seq.to_string(),
        },
    )
    .await;
}

/// 圈速变更后统一播报入口（模块上传 / 管理端改·加·删成绩共用）。
/// 事件两分：alltime（历史）优先于 version——两纪录同破时只播历史（与 Toast 取最高一致）。
/// lap_ms 为 0 时（删除场景）按 records 表当前持有者与圈时播报（纪录易主也算刷新）。
pub async fn broadcast_lap_change(state: &App, gp_index: i16, version_code: i32, lap_ms: i32) {
    let alltime: Option<(i32, Uuid)> =
        sqlx::query_as("SELECT lap_ms, user_id FROM records WHERE gp_index=$1 AND kind='alltime'")
            .bind(gp_index)
            .fetch_one(&state.pool)
            .await
            .ok();
    let version: Option<(i32, Uuid)> = sqlx::query_as(
        "SELECT lap_ms, user_id FROM records WHERE gp_index=$1 AND kind='version' AND version_code=$2",
    )
    .bind(gp_index)
    .bind(version_code)
    .fetch_one(&state.pool)
    .await
    .ok();
    // lap_ms>0：刚写入/修改的圈时是否就是当前纪录值 → 该维度被刷新；
    // lap_ms==0（删除）：records 存在即视为刷新（易主/新值都播）
    let alltime_hit = if lap_ms > 0 {
        alltime.iter().any(|(ms, _)| *ms == lap_ms)
    } else {
        alltime.is_some()
    };
    let version_hit = if lap_ms > 0 {
        version.iter().any(|(ms, _)| *ms == lap_ms)
    } else {
        version.is_some()
    };
    if !alltime_hit && !version_hit {
        return;
    }
    let (holder_ms, holder_id) = if alltime_hit {
        alltime.as_ref().unwrap().clone()
    } else {
        version.as_ref().unwrap().clone()
    };
    let (username, reg_seq): (String, i64) = sqlx::query_as(
        "SELECT username, reg_seq FROM users WHERE id=$1",
    )
    .bind(holder_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((String::new(), 0));
    let event = if alltime_hit { "record_alltime" } else { "record_version" };
    broadcast(
        state,
        event,
        &BroadcastVars {
            track: crate::api::laps::track_display_name(gp_index).to_string(),
            lap: crate::api::leaderboard::format_lap_ms(holder_ms),
            version: crate::api::laps::version_display(version_code),
            paddock_name: username,
            paddock_id: reg_seq.to_string(),
        },
    )
    .await;
}

fn render(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

fn render_reply_template(t: &str, vars: &ReplyVars) -> String {
    render(t, &[
        ("qq_name", vars.qq_name.as_str()),
        ("paddock_name", vars.paddock_name.as_str()),
        ("paddock_id", vars.paddock_id.as_str()),
        ("code", vars.code.as_str()),
    ])
}

/// 主动播报：把事件广播到所有勾选的目标群（broadcast 规则渲染后逐群发送）。
/// 无勾选群或额度不足时静默失败（tracing 记录），不影响主流程。
pub async fn broadcast(state: &App, event: &str, vars: &BroadcastVars) {
    let groups = broadcast_groups(&state.pool).await;
    if groups.is_empty() {
        return;
    }
    let rules = load_rules(&state.pool).await;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "broadcast") {
        if !rule_matches(r, "", event) {
            continue;
        }
        let content = render(&r.template, &[
            ("track", vars.track.as_str()),
            ("lap", vars.lap.as_str()),
            ("version", vars.version.as_str()),
            ("paddock_name", vars.paddock_name.as_str()),
            ("paddock_id", vars.paddock_id.as_str()),
        ]);
        for g in &groups {
            // 主动消息：不带 msg_id（未认证额度极低，失败只记日志）
            send_direct_group(state, g, &content).await;
        }
    }
}

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
async fn send_direct_group(state: &App, group: &str, content: &str) {
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

// ---------- 群消息处理 ----------

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

    // 统一规则分发：内置动作（注册/重置）与普通回复全部走规则引擎，
    // 管理端"消息配置"里的每条规则就是完整的真实语义（可编辑）。
    let normalized = content.replace('＃', "#");
    let rules = load_rules(&state.pool).await;
    let qq_name = msg.qq_name();
    crate::applog::log_event(
        &state.pool, "info", "bot", "group_msg", &qq_name,
        format!("群消息：{content}"),
        json!({"group": msg.group_openid}),
    );
    for r in rules.iter().filter(|r| r.enabled && r.kind == "reply") {
        if !rule_matches(r, &normalized, "") {
            continue;
        }
        match r.action.as_str() {
            "reg_code" => {
                handle_reg_code(state, r, &msg_id, &ref_id, &normalized, &qq_name, msg.member_openid(), &msg.group_openid).await;
                return; // 一条消息只命中一条动作规则
            }
            "reset_password" => {
                handle_reset_password(state, r, &msg_id, &ref_id, &normalized, &qq_name, &msg.group_openid).await;
                return;
            }
            _ => {
                // 普通回复：围场变量按 member_openid 反查
                let (name, seq): (String, i64) = sqlx::query_as(
                    "SELECT username, reg_seq FROM users WHERE member_openid = $1",
                )
                .bind(msg.member_openid())
                .fetch_one(&state.pool)
                .await
                .unwrap_or((String::new(), 0));
                let vars = ReplyVars {
                    qq_name: qq_name.clone(),
                    paddock_name: name,
                    paddock_id: seq.to_string(),
                    code: String::new(),
                };
                let reply = render_reply_template(&r.template, &vars);
                send_group_reply(state, msg.group_openid.clone(), &msg_id, &ref_id, reply).await;
                return;
            }
        }
    }
    // 无规则命中：bot 静默
}

/// 各失败类型的内置默认文案（面板预设预填的源头；用户改模板后以模板为准）。
/// 模板变量 {{code}}（校验码）/{{name}}（用户名）——动态部分经变量注入，
/// {{reason}} 废除：文案必须完全可编辑，不允许留一个"展开后才可见"的占位符。
fn builtin_reason(t: FailType, code: &str) -> String {
    match t {
        FailType::NoCode => "未识别到校验码：请在「申请围场通行证」后跟 #校验码".to_string(),
        FailType::InvalidCode => format!("校验码 {code} 无效或已过期，请在围场页重新申请"),
        FailType::DupOpenid => "该 QQ 身份已有其他注册校验在途，请勿重复申请".to_string(),
        FailType::NoIdentity => "无法识别你的群身份，请确认已在 QQ 群设置中允许机器人获取群信息".to_string(),
        FailType::NoUser => "用户名不存在，请核对后重试".to_string(),
    }
}

/// 动作失败文案：每类失败独立模板优先 → fail_template → 内置默认原文。
/// 模板内可用 {{code}}/{{name}}；无任何隐藏占位符。
fn fail_type_reply(rule: &BotRule, t: FailType, code: &str, username: &str) -> String {
    let specific = match t {
        FailType::NoCode => &rule.no_code_template,
        FailType::InvalidCode => &rule.invalid_code_template,
        FailType::DupOpenid => &rule.dup_openid_template,
        FailType::NoIdentity => &rule.no_identity_template,
        FailType::NoUser => &rule.no_user_template,
    };
    let tpl = if !specific.trim().is_empty() {
        specific
    } else if !rule.fail_template.trim().is_empty() {
        &rule.fail_template
    } else {
        return builtin_reason(t, code);
    };
    tpl.replace("{{code}}", code)
       .replace("{{name}}", username)
       .replace("{{qq_name}}", "") // 失败场景 qq_name 多数无效，留空避免渲染残留
}

/// 注册校验（action=reg_code）：从触发词后提取校验码 → pending_regs 定位会话 →
/// 直接建号 → 按规则模板引用回复。成功走 template，失败走 fail_template（空=内置文案）。
/// 约束（PADDOCK_PLAN §6 S4）：一条码一openid；重复绑定他人码会被唯一约束拦。
async fn handle_reg_code(
    state: &App,
    rule: &BotRule,
    msg_id: &str,
    ref_id: &str,
    content: &str,
    qq_name: &str,
    member_openid: &str,
    group: &str,
) {
    // 提取锚点（keyword）后的码：容忍 # 两侧空格（客户端话题渲染会插空格）
    let anchor = if rule.keyword.is_empty() { "申请围场通行证" } else { rule.keyword.as_str() };
    let code = content
        .find(anchor)
        .map(|idx| {
            let rest = content[idx + anchor.len()..].trim_start();
            rest.strip_prefix('#')
                .map(|c| c.trim().to_uppercase())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let reply = if member_openid.is_empty() {
        fail_type_reply(rule, FailType::NoIdentity, "", "")
    } else if code.is_empty() {
        // 提不出码（锚点后没有 #xxx）：独立出口（v17 新增，此前带空码落到"码无效"）
        fail_type_reply(rule, FailType::NoCode, "", "")
    } else {
        let n: Result<i64, _> = sqlx::query_scalar(
            "SELECT count(*) FROM pending_regs WHERE reg_code = $1 AND expires_at > now()",
        )
        .bind(&code)
        .fetch_one(&state.pool)
        .await;
        match n {
            Ok(0) | Err(_) => fail_type_reply(rule, FailType::InvalidCode, &code, ""),
            Ok(_) => {
                // openid 已被其他在途会话占用：直接拒绝（防一 QQ 多账号）
                let occupied: Option<String> = sqlx::query_scalar(
                    "SELECT reg_code FROM pending_regs WHERE member_openid = $1 AND expires_at > now() AND reg_code <> $2",
                )
                .bind(member_openid)
                .bind(&code)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or(None);
                if occupied.is_some() {
                    fail_type_reply(rule, FailType::DupOpenid, &code, "")
                } else {
                    // 建号事务：DELETE pending RETURNING 锁存数据 → INSERT users。
                    // 失败时 pending 已被事务回滚恢复，用户可重试。
                    match auth_handlers::create_user_from_pending(&state.pool, &code, member_openid).await {
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
                            crate::applog::log_event(
                                &state.pool, "info", "auth", "user_register", &username,
                                format!("注册成功：{username}（车手 ID {reg_seq}，经群内校验码建号）"),
                                json!({"reg_seq": reg_seq, "code": code}),
                            );
                            let vars = ReplyVars {
                                qq_name: qq_name.to_string(),
                                paddock_name: username,
                                paddock_id: reg_seq.to_string(),
                                code: code.to_string(),
                            };
                            render_reply_template(&rule.template, &vars)
                        }
                        Err((status, msg)) => {
                            tracing::warn!("建号失败 {status}: {msg}（code={code}）");
                            // 建号事务内两失败分支：404=码被并发用掉（InvalidCode）、409=已绑定（DupOpenid）
                            let ft = if status == StatusCode::CONFLICT { FailType::DupOpenid } else { FailType::InvalidCode };
                            fail_type_reply(rule, ft, &code, "")
                        }
                    }
                }
            }
        }
    };
    send_group_reply(state, group.to_string(), msg_id, ref_id, reply).await;
}

/// 失败类型：对应 BotRule 的每类独立模板字段。
#[derive(Clone, Copy)]
enum FailType {
    NoCode,
    InvalidCode,
    DupOpenid,
    NoIdentity,
    NoUser,
}

/// 密码重置（action=reset_password）：触发词后提取用户名 → 一次性码（30 分钟）→
/// 按规则模板引用回复。
async fn handle_reset_password(
    state: &App,
    rule: &BotRule,
    msg_id: &str,
    ref_id: &str,
    content: &str,
    qq_name: &str,
    group: &str,
) {
    let anchor = if rule.keyword.is_empty() { "重置密码" } else { rule.keyword.as_str() };
    let name = content
        .find(anchor)
        .map(|idx| content[idx + anchor.len()..].trim().to_string())
        .unwrap_or_default();
    let reply = match auth_handlers::create_reset_code(&state.pool, &name).await {
        Ok(code) => {
            crate::applog::log_event(
                &state.pool, "info", "auth", "reset_code_issued", &name,
                format!("签发密码重置码（群内申请，30 分钟有效）"),
                json!({}),
            );
            let vars = ReplyVars {
                qq_name: qq_name.to_string(),
                paddock_name: name.clone(),
                paddock_id: String::new(),
                code,
            };
            render_reply_template(&rule.template, &vars)
        }
        Err(e) => fail_type_reply(rule, FailType::NoUser, "", &name),
    };
    send_group_reply(state, group.to_string(), msg_id, ref_id, reply).await;
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
    // 单聊同走规则引擎（文案可编辑）：命中动作规则按其执行；reg_code 在单聊强制引导回群
    // （单聊没有 member_openid 无法建号，这是安全语义）；未命中任何规则 = 静默（与群聊一致）
    let rules = load_rules(&state.pool).await;
    let mut reply: Option<String> = None;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "reply") {
        if !rule_matches(r, &normalized, "") {
            continue;
        }
        match r.action.as_str() {
            "reg_code" => {
                let anchor = if r.keyword.is_empty() { "申请围场通行证" } else { r.keyword.as_str() };
                let _ = anchor;
                // 单聊无群身份，建号不可行：按该规则的 NoIdentity 文案引导回群
                reply = Some(fail_type_reply(r, FailType::NoIdentity, "", ""));
            }
            "reset_password" => {
                let anchor = if r.keyword.is_empty() { "重置密码" } else { r.keyword.as_str() };
                let name = normalized
                    .find(anchor)
                    .map(|idx| normalized[idx + anchor.len()..].trim().to_string())
                    .unwrap_or_default();
                reply = Some(match auth_handlers::create_reset_code(&state.pool, &name).await {
                    Ok(code) => {
                        let vars = ReplyVars {
                            qq_name: String::new(),
                            paddock_name: name.clone(),
                            paddock_id: String::new(),
                            code,
                        };
                        render_reply_template(&r.template, &vars)
                    }
                    Err(_) => fail_type_reply(r, FailType::NoUser, "", &name),
                });
            }
            _ => {
                // 普通回复：单聊无 member_openid（是 user_openid，两体系不互通），围场变量空
                let vars = ReplyVars {
                    qq_name: String::new(),
                    paddock_name: String::new(),
                    paddock_id: String::new(),
                    code: String::new(),
                };
                reply = Some(render_reply_template(&r.template, &vars));
            }
        }
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
    let mut payload = json!({
        "msg_type": 0,
        "content": job.content,
        "msg_seq": 1,
    });
    // 被动回复必带 msg_id；主动消息（broadcast）不带——字段缺失即主动消息语义
    if !job.msg_id.is_empty() {
        payload["msg_id"] = json!(job.msg_id);
    }
    // 引用回复：带 message_reference 让回复以引用原消息形式呈现（非裸发）
    if !job.ref_msg_id.is_empty() {
        payload["message_reference"] = json!({ "message_id": job.ref_msg_id });
    }
    let res = client
        .post(url)
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