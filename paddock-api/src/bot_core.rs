//! bot 核心：**平台中立**的规则引擎、内置动作、播报判定与渲染。
//!
//! ## 为什么独立成文件
//!
//! QQ 与 Telegram 两个适配层（[`crate::qq_bot`] / [`crate::telegram_bot`]）必须共用
//! **同一份**规则匹配、动作执行与模板渲染代码——「交互逻辑完全一致」的真正落点在这里，
//! 不在配置文件。两套配置内容不同但引擎相同 ⇒ 同样的输入喂同样的规则，产出同样的文本。
//!
//! 平台差异被收敛到三个点，全部经 [`Platform`] 表达：
//!   1. 身份列（`users.member_openid` / `users.telegram_id`）
//!   2. `{{at_me}}` 的渲染（QQ 内联 XML 标签 / TG 转 `text_mention` 实体）
//!   3. 内置失败文案里的平台措辞（QQ 群设置 / Telegram 隐私模式）
//!
//! 发送、验签、入站 payload 解析留在各适配层——那些是真正平台特有的。

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{api::auth_handlers, state::AppState as App};

// ---------- 平台 ----------

/// 平台标识。决定身份列、@ 渲染方式、内置文案措辞。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Qq,
    Telegram,
}

impl Platform {
    /// 日志与审计用的平台名。
    pub fn label(self) -> &'static str {
        match self {
            Platform::Qq => "qq",
            Platform::Telegram => "tg",
        }
    }
}

/// 按平台身份反查围场账号（username + reg_seq）。查不到 = `None`。
///
/// ⚠️ 两个平台**必须分开写 SQL**：`telegram_id` 是 BIGINT，拿字符串绑定会让
/// Postgres 走隐式转换（可能报错或全表扫描）；QQ 的 `member_openid` 是 TEXT。
pub async fn lookup_identity(
    pool: &PgPool,
    platform: Platform,
    identity_id: &str,
) -> Option<(String, i64)> {
    lookup_identity_full(pool, platform, identity_id)
        .await
        .map(|u| (u.paddock_name, u.reg_seq))
}

/// 反查结果：围场名 + 车手号 + Telegram username（播报 @ 用）。
pub struct IdentityHit {
    pub paddock_name: String,
    pub reg_seq: i64,
    pub telegram_username: Option<String>,
}

/// 同上，但一并取出 `telegram_username`（生成 `@username` 形式的提及需要它）。
pub async fn lookup_identity_full(
    pool: &PgPool,
    platform: Platform,
    identity_id: &str,
) -> Option<IdentityHit> {
    if identity_id.is_empty() {
        return None;
    }
    match platform {
        Platform::Qq => sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT username, reg_seq, telegram_username FROM users WHERE member_openid = $1",
        )
        .bind(identity_id)
        .fetch_one(pool)
        .await
        .ok()
        .map(|(n, s, u)| IdentityHit { paddock_name: n, reg_seq: s, telegram_username: u }),
        Platform::Telegram => {
            let id: i64 = identity_id.parse().ok()?;
            sqlx::query_as::<_, (String, i64, Option<String>)>(
                "SELECT username, reg_seq, telegram_username FROM users WHERE telegram_id = $1",
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .ok()
            .map(|(n, s, u)| IdentityHit { paddock_name: n, reg_seq: s, telegram_username: u })
        }
    }
}

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
/// action（reply 用）= 内置动作 key（见 action_metas()）："reply" 普通回复 /
/// "reg_code" 注册校验（建号）/ "reset_password" 密码重置（发码）/
/// "query_username" 查询用户名。reg_code 从触发词后提取 #校验码
/// （提不出码静默，防关键词误触发）；reset_password / query_username 严格等于触发词
/// 才响应，按发言者平台身份反查账号（不提取用户名）。成功走 template，
/// 失败按类型走独立文案字段（空=内置默认，取自 action_metas()）。
/// match_all = true 时条件 AND，false 时 OR；conditions 为空 = 恒命中。
/// 旧格式 keyword 迁移：读入时合成 conditions。⚠️ action 的合法值与失败字段、
/// 内置文案的唯一事实源是 action_metas()——新增动作改那里，不在前端/此处硬编码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotRule {
    pub id: String,
    pub kind: String,
    /// 内置动作的触发词/提取锚点（有 action 语义的规则用；普通回复可空）
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
    /// **金标模板**（仅 broadcast 规则用）：当刷新的那条圈是"零辅助"（TC 与 ABS
    /// 全关）时改用本模板，否则用 [template]。留空 = 不区分，恒用 [template]。
    ///
    /// 为什么是独立字段而不是在 template 里插变量：金标文案往往需要**完全不同的
    /// 排版与措辞**（用户实测文案是多行 + 金标认证句），变量替换表达不了"整段换掉"。
    #[serde(default)]
    pub gold_template: String,
    /// 通用失败模板（可 {{code}}/{{name}}）；仅在对应类型独立文案为空时使用
    #[serde(default)]
    pub fail_template: String,
    // ---- 失败文案字段（每类失败独立模板；空 = 取 action_metas() 内置默认） ----
    /// 码无效/过期（含并发被用掉）——reg_code
    #[serde(default)]
    pub invalid_code_template: String,
    /// 该身份已有在途会话 / 已绑定过账号——reg_code
    #[serde(default)]
    pub dup_openid_template: String,
    /// 平台未给群身份（member_openid / telegram_id 缺失）——所有需群身份的动作
    #[serde(default)]
    pub no_identity_template: String,
    /// 平台身份反查不到账号——reset_password / query_username
    #[serde(default)]
    pub no_user_template: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

// ---------- 内置动作元数据（单一事实源） ----------
//
// ⚠️ 这是动作语义、匹配方式、失败字段与内置默认文案的【唯一事实源】。
// 管理端设置页从它渲染编辑器（不再有 JS 镜像），后端执行也从它取默认文案。
// 新增一个内置动作 = ① 此表加一条 ② handle_* 的 match 加一个分支。

/// 一个失败文案字段的元数据。key 即 BotRule 上的字段名。
#[derive(Debug, Clone, Serialize)]
pub struct FailFieldMeta {
    pub key: String,
    pub label: String,
    /// 内置默认文案（{{code}}/{{name}} 为变量占位；空 = 该动作无此类失败）
    pub default: String,
}

/// 一个内置动作的元数据。
#[derive(Debug, Clone, Serialize)]
pub struct ActionMeta {
    /// action 字段值；"reply" = 普通回复（条件触发+模板渲染，无动作语义）
    pub key: String,
    pub label: String,
    /// 触发语义说明（编辑器提示文案）
    pub desc: String,
    /// 匹配方式：normal=按 conditions 判定 / contains=消息含触发词 / exact=消息严格等于触发词
    pub match_mode: String,
    pub failure_fields: Vec<FailFieldMeta>,
}

fn fail_field(key: &str, label: &str, default: &str) -> FailFieldMeta {
    FailFieldMeta { key: key.into(), label: label.into(), default: default.into() }
}

/// 内置动作表。见上方契约注释——改这里即可让管理端编辑器自动跟上。
/// ⚠️ 失败文案的 default 是用户 2026-09-12 在管理端定稿的版本，视为默认基线。
/// 文案里的「QQ」字样在 Telegram 侧会被 [`platform_adapt`] 按平台替换
/// （仅当规则字段值与这里的默认逐字相同，即用户没改过）。
pub fn action_metas() -> Vec<ActionMeta> {
    vec![
        ActionMeta {
            key: "reply".into(),
            label: "普通回复".into(),
            desc: "满足触发条件即回复模板；围场用户名/围场 ID 按发言者平台身份自动填充（未注册为空）。".into(),
            match_mode: "normal".into(),
            failure_fields: vec![],
        },
        ActionMeta {
            key: "reg_code".into(),
            label: "注册校验（建号）".into(),
            desc: "消息含触发词且其后带 #校验码才响应（提不出码静默不回复）；校验通过即建号并回复成功文案。".into(),
            match_mode: "contains".into(),
            failure_fields: vec![
                fail_field("invalid_code_template", "校验码无效/过期",
                    "校验码 {{code}} 无效或已过期，请在围场页重新申请。"),
                fail_field("dup_openid_template", "该身份已有在途会话/已绑定",
                    "该 QQ 身份有其他注册校验在途或已绑定车手通行证，请勿重复申请。"),
                fail_field("no_identity_template", "无法识别群身份",
                    "无法识别你的群身份，请确认已在 QQ 群设置中允许机器人获取群信息。"),
            ],
        },
        ActionMeta {
            key: "reset_password".into(),
            label: "密码重置（发码）".into(),
            desc: "消息严格等于触发词才响应，账号按发言者平台身份自动匹配，回复一次性重置码。".into(),
            match_mode: "exact".into(),
            failure_fields: vec![
                fail_field("no_user_template", "未找到发言者绑定的围场账号",
                    "找不到该 QQ 身份对应的车手账号，请核对后重试。"),
            ],
        },
        ActionMeta {
            key: "query_username".into(),
            label: "查询用户名".into(),
            desc: "消息严格等于触发词才响应，按发言者平台身份反查围场账号，回复用户名。".into(),
            match_mode: "exact".into(),
            failure_fields: vec![
                fail_field("no_user_template", "未找到发言者绑定的围场账号",
                    "找不到该 QQ 身份对应的车手账号，请核对后重试。"),
            ],
        },
    ]
}

/// 动作元数据查询；未知 action 视作普通回复（防御旧脏值）。
pub fn action_meta(action: &str) -> ActionMeta {
    let metas = action_metas();
    metas
        .iter()
        .find(|m| m.key == action)
        .cloned()
        .unwrap_or_else(|| metas[0].clone())
}

/// 该 action 是否为「内置动作」（有执行语义，触发词即条件，不渲染条件编辑器）。
pub fn is_action_key(action: &str) -> bool {
    !action.is_empty() && action != "reply"
}

/// 从 BotRule 取某失败字段的值（空串 = 未配置）。
fn rule_field<'a>(rule: &'a BotRule, key: &str) -> &'a str {
    match key {
        "invalid_code_template" => &rule.invalid_code_template,
        "dup_openid_template" => &rule.dup_openid_template,
        "no_identity_template" => &rule.no_identity_template,
        "no_user_template" => &rule.no_user_template,
        _ => "",
    }
}

/// 某动作某失败字段的内置默认（空 = 无此字段）。
fn fail_default(action: &str, key: &str) -> String {
    action_meta(action)
        .failure_fields
        .iter()
        .find(|f| f.key == key)
        .map(|f| f.default.clone())
        .unwrap_or_default()
}

const RULES_CFG_KEY: &str = "bot_message_rules";

/// 内置预设规则（首次未配置时的默认集；保存任何规则后即被用户数据覆盖）。
/// 完整呈现内部真实语义：所有内置文案原文预填（输入框不留空）。
/// 动作规则（reg_code/reset_password/query_username）无可编辑条件——触发词即条件；
/// 播报两条：历史优先于版本，同破只播历史（与 Toast 取最高一致）。
/// ⚠️ 失败文案默认值一律取自 action_metas()（唯一事实源），此处不重复硬编码。
///
/// ⚠️ **规则集两平台共享**（用户 2026-11-03 定案）：交互逻辑完全一致，改一次两平台同时生效。
/// 平台措辞差异由 [`platform_adapt`] 在渲染时处理，不靠两套配置。
fn preset_rules() -> Vec<BotRule> {
    vec![
        BotRule {
            id: "preset-reg".into(),
            kind: "reply".into(),
            keyword: "申请围场通行证".into(),
            action: "reg_code".into(),
            conditions: vec![],
            match_all: true,
            template: "校验成功，欢迎「{{paddock_name}}」加入 CAMDA，您已是围场第 {{paddock_id}} 位车手！请返回模块直接点击登录。".into(),
            gold_template: String::new(),
            fail_template: String::new(),
            invalid_code_template: fail_default("reg_code", "invalid_code_template"),
            dup_openid_template: fail_default("reg_code", "dup_openid_template"),
            no_identity_template: fail_default("reg_code", "no_identity_template"),
            no_user_template: String::new(),
            enabled: true,
        },
        BotRule {
            id: "preset-reset".into(),
            kind: "reply".into(),
            keyword: "我需要重置密码".into(),
            action: "reset_password".into(),
            conditions: vec![],
            match_all: true,
            template: "车手「{{paddock_name}}」，您的重置码已生成：\n\n{{code}}\n\n30 分钟内有效，请在围场页用「忘记密码」功能提交新密码。".into(),
            gold_template: String::new(),
            fail_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: fail_default("reset_password", "no_user_template"),
            enabled: true,
        },
        BotRule {
            id: "preset-query-name".into(),
            kind: "reply".into(),
            keyword: "查询用户名".into(),
            action: "query_username".into(),
            conditions: vec![],
            match_all: true,
            template: "您的围场用户名是「{{paddock_name}}」。".into(),
            gold_template: String::new(),
            fail_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: fail_default("query_username", "no_user_template"),
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
            template: "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出 {{lap}}，刷新了全服历史最快圈速！".into(),
            gold_template: "# 纪录快讯\n{{at_me}}\n{{paddock_id}} 号车手\n***{{paddock_name}}***\n{{track}}\n{{lap}}\n刷新了全服历史最快圈速！\n本圈辅助全关，获得金标认证！".into(),
            fail_template: String::new(),
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
            template: "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出 {{lap}}，刷新了 {{version}} 版本的全服最快圈速！".into(),
            gold_template: "# 纪录快讯\n{{at_me}}\n{{paddock_id}} 号车手\n***{{paddock_name}}***\n{{track}}\n{{lap}}\n刷新了 {{version}} 版本的全服最快圈速！\n本圈辅助全关，获得金标认证！".into(),
            fail_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: String::new(),
            enabled: true,
        },
    ]
}

/// 读规则列表。库中无配置或为空列表 → 返回预设集（面板默认展示、webhook 按预设渲染）。
/// 旧 keyword 格式自动迁移为 conditions。
pub async fn load_rules(pool: &PgPool) -> Vec<BotRule> {
    let raw = get_cfg(pool, RULES_CFG_KEY).await;
    let key_exists = raw.as_ref().is_some_and(|s| !s.is_empty());
    // ⚠️ key 存在即用户数据——哪怕是 "[]"（显式保存的空表）或解析失败，
    // 都绝不回落 preset_rules()。任何"读→展示→保存"链路一旦混入预设，
    // 下一次保存就会把用户定制固化覆盖掉（v35 前的实际事故：用户改的文案
    // 每次部署后打开设置页再保存即被覆盖回默认）。
    let mut rules: Vec<BotRule> = match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_else(|e| {
            // 解析失败不静默换预设：宁可空跑（bot 静默）也保住库里原文可人工修
            tracing::error!("bot_message_rules JSON 解析失败（保留原文不回落预设）: {e}");
            Vec::new()
        }),
        _ => Vec::new(),
    };
    if rules.is_empty() && !key_exists {
        // 仅当 key 从未写入过（首次部署）才给预设；"[]"是用户显式数据，尊重
        rules = preset_rules();
    }
    for r in &mut rules {
        // 一次性语义迁移（2026-09-04）：旧 reset_password 规则的触发词是"重置密码"+
        // 按用户名提取，新语义是"我需要重置密码"严格匹配+按群身份反查。只升级
        // keyword 仍为旧默认值的规则（用户自定义词视为有意保留，不动）。
        if r.action == "reset_password" && r.keyword.trim() == "重置密码" {
            r.keyword = "我需要重置密码".into();
        }        // 脏值修复（2026-09-12）：播报规则的 action 曾被前端硬编码默认写成 "reply"。
        // 非 reply 类规则的 action 无意义 → 归零，避免管理端数据显示"播报 · 普通回复"。
        if r.kind != "reply" && !r.action.is_empty() {
            r.action = String::new();
        }
        // 一次性文案迁移（2026-09-04 二轮）：播报模板变量两侧空格（跑出 {{lap}} /
        // {{version}} 版本）。只升级仍与旧默认逐字相等的模板，用户改过的模板不动。
        // ⚠️ 按 id 取预设而非下标——preset_rules() 追加规则时下标会漂（2026-09-12
        // 新增 preset-query-name 即已使 [2]/[3] 指错条目）。
        if r.kind == "broadcast" {
            if r.template == "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出{{lap}}，刷新了全服历史最快圈速！" {
                r.template = preset_by_id("preset-bc-alltime").template;
            } else if r.template == "{{paddock_id}} 号车手「{{paddock_name}}」刚刚在{{track}}跑出{{lap}}，刷新了{{version}}版本的全服最快圈速！" {
                r.template = preset_by_id("preset-bc-version").template;
            }
            // 一次性补充迁移（2026-09-18，金标模板引入）：库里的播报规则是**上线时
            // 整份写入**的（见 v35+ 的"key 存在即用户数据"约定），新加的 gold_template
            // 字段在旧 JSON 里反序列化成空串 → 金标播报永不触发。
            //
            // ⚠️ 不能在这里就地补：load_rules 每次播报都要调用，"每次读都补空值"
            // 会让**用户手工清空**（表示不要金标播报）在下一轮被悄悄填回来——
            // "字段从未存在"与"用户清空"在 JSON 里无法区分。故改用**一次性标记**：
            // 只补一次，补完写下标记，此后用户怎么改都不再被覆盖。
            // 见 [`migrate_gold_templates_once`]。
        }
        // 旧 keyword 格式迁移：普通回复/播报合成 conditions；
        // 动作规则（有执行语义的 action）触发词即条件，不生成条件行
        if r.conditions.is_empty()
            && !r.keyword.trim().is_empty()
            && !(r.kind == "reply" && is_action_key(&r.action))
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

/// 一次性补充迁移：给库里已有的内置播报规则补上 `gold_template`（2026-09-18 引入）。
///
/// ## 为什么必须"一次性"而不是在 `load_rules` 里就地补
///
/// `load_rules` 每次播报都会调用。若在那里"见空就补"，**用户手工清空金标模板**
/// （表示"我不要金标播报"）会在下一轮被无声填回——而"字段从未存在"与"用户清空"
/// 在 JSON 里都是空串，无法区分。故本函数只跑一次，跑完在 configs 表写下标记，
/// 此后用户怎么改都不再被覆盖（与 bot 规则"key 存在即用户数据"同一原则）。
///
/// ## 只补内置条目
///
/// 按 `preset_by_id(r.id)` 命中才补：用户自建的播报规则 id 是 `r<timestamp>`，
/// 匹配不到预设 → 保持为空（用户没写过，也不该替他编）。补完的结果立即落库。
pub async fn migrate_gold_templates_once(pool: &PgPool) {
    const MARK: &str = "bot_gold_template_migrated_v1";
    if get_cfg(pool, MARK).await.is_some_and(|v| !v.is_empty()) {
        return;
    }
    // 读**原样**规则（不跑等值迁移、不回落预设）——迁移必须作用在库里真实数据上
    let raw = get_cfg(pool, RULES_CFG_KEY).await;
    if let Some(s) = raw.as_ref().filter(|s| !s.is_empty()) {
        if let Ok(mut rules) = serde_json::from_str::<Vec<BotRule>>(s) {
            let mut changed = false;
            for r in &mut rules {
                if r.kind != "broadcast" || !r.gold_template.trim().is_empty() {
                    continue;
                }
                let preset = preset_by_id(&r.id);
                if preset.kind == "broadcast" && !preset.gold_template.is_empty() {
                    r.gold_template = preset.gold_template;
                    changed = true;
                    tracing::info!("[bot] 已为播报规则 {} 补上预设金标模板", r.id);
                }
            }
            if changed {
                if let Err(e) = save_rules(pool, &rules).await {
                    tracing::error!("[bot] 金标模板迁移写库失败（下次启动重试）: {e}");
                    return; // 不写标记 → 下次启动重试
                }
            }
        } else {
            // 解析失败：不写标记，下次启动重试（保住库内原文，勿覆盖）
            tracing::error!("[bot] 金标模板迁移跳过：规则 JSON 无法解析");
            return;
        }
    }
    if let Err(e) = set_cfg(pool, MARK, "1").await {
        tracing::error!("[bot] 金标模板迁移标记写入失败: {e}");
    }
}

/// 按 id 取预设规则（文案迁移用；缺失时返回空规则兜底，绝不 panic）。
fn preset_by_id(id: &str) -> BotRule {
    preset_rules()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap_or(BotRule {
            id: id.into(),
            kind: String::new(),
            keyword: String::new(),
            action: String::new(),
            conditions: vec![],
            match_all: true,
            template: String::new(),
            gold_template: String::new(),
            fail_template: String::new(),
            invalid_code_template: String::new(),
            dup_openid_template: String::new(),
            no_identity_template: String::new(),
            no_user_template: String::new(),
            enabled: false,
        })
}

/// 规则列表 → JSON 字符串（设置页模板注入用）。
pub async fn load_rules_json(pool: &PgPool) -> String {
    serde_json::to_string(&load_rules(pool).await).unwrap_or_else(|_| "[]".into())
}

/// 库中原样规则（不跑等值迁移、不回落预设）——审计用：保存前后对照的是
/// 库里真实旧值而非迁移后的展示值。
pub async fn load_rules_raw(pool: &PgPool) -> Vec<BotRule> {
    let raw = get_cfg(pool, RULES_CFG_KEY).await;
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 全量保存规则。
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
    rule_matches_public(rule, content, event)
}

/// [`rule_matches`] 的公开版本（Telegram 的私聊分支需要自己遍历规则）。
pub fn rule_matches_public(rule: &BotRule, content: &str, event: &str) -> bool {
    // 内置动作规则（有执行语义的 action）：触发词即条件，由动作元数据的 match_mode 决定：
    // - contains（reg_code）：触发词出现在消息中即进入处理（是否响应由码提取结果决定——
    //   提不出 #码 静默，防"申请围场通行证"裸关键词被闲聊误触发）
    // - exact（reset_password/query_username）：消息内容严格等于触发词才响应
    //   （防包含关键词的闲聊误触发；账号按发言者平台身份反查）
    if rule.kind == "reply" && is_action_key(&rule.action) {
        let kw = rule.keyword.trim();
        if kw.is_empty() {
            return false;
        }
        return match action_meta(&rule.action).match_mode.as_str() {
            "exact" => content.trim() == kw,
            _ => content.contains(kw), // contains 默认
        };
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

/// 模板是否依赖身份（围场用户名/车手 ID/@车手）。普通回复若依赖身份却查不到账号，
/// 必须走失败文案而不是把空串渲染进去（"未注册用户收到空用户名"的根因）。
fn template_needs_identity(t: &str) -> bool {
    t.contains("{{paddock_name}}") || t.contains("{{paddock_id}}") || t.contains("{{at_me}}")
}

/// 播报用赛道名：去国旗与文字间空格（播报文案紧凑，模块 UI 侧仍保留空格）。
fn track_display_name_compact(gp_index: i16) -> String {
    let name = crate::api::laps::track_display_name(gp_index);
    // 契约格式="国旗 emoji(2 个区域指示符 U+1F1E6..U+1F1FF) + 空格 + 文字"。
    // ⚠️ 旗码点必须成对判定【并拼回输出】——只取 rest 会把整面旗丢掉（2026-09-06 二次事故）。
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b))
            if ('\u{1F1E6}'..='\u{1F1FF}').contains(&a)
                && ('\u{1F1E6}'..='\u{1F1FF}').contains(&b) =>
        {
            let rest = chars.as_str().strip_prefix(' ').unwrap_or(chars.as_str());
            format!("{a}{b}{rest}")
        }
        _ => name.to_string(),
    }
}

// ---------- 渲染 ----------

/// QQ 的"车手"提及：openid 非空 → 官方 XML 风格标签 <qqbot-at-user id=".." />
/// （text-chain 规范标准格式；旧式 <@openid> 已弃用、出站被平台转义为裸文本，
/// markdown/纯文本两通道均实证 2026-09-06）；openid 缺失 → 退化为纯文本 @「用户名」。
fn at_user(openid: &str, username: &str) -> String {
    if !openid.is_empty() {
        format!("<qqbot-at-user id=\"{openid}\" />")
    } else {
        // ⚠️ **没有 QQ 身份就输出空串**（2026-11-03 用户定案，与 Telegram 侧一致）。
        //
        // 旧实现退化成 `@「{username}」`——在「纪录持有者是 Telegram 用户」的场景下
        // 会多出一行 `@「测试」`，既不是真提及（QQ 不解析这个语法），又占一行。
        // `{{at_me}}` 的语义是「@ 这个人」，没有可 @ 的身份就不该产出任何内容。
        let _ = username;
        String::new()
    }
}

/// 渲染结果：正文 + 需要 @ 的区间（Telegram 的 `text_mention` 实体用）。
///
/// QQ 侧 `mentions` 恒为空——它的 @ 是内联 XML 标签，已经写进 `text` 里。
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    pub text: String,
    /// 实体区间（仅 Telegram 用；QQ 的格式走 markdown 通道，不需要实体）
    pub entities: Vec<Entity>,
}

/// 一个实体区间。偏移是**字节**（相对 `Rendered::text`），
/// 转成 Telegram 要的 UTF-16 code unit 由 `telegram_bot` 负责。
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub kind: EntityKind,
    pub off: usize,
    pub len: usize,
}

/// 支持的实体类型。QQ 的 markdown 语法只有两种格式，这里一一对应：
///   `***x***` → Bold
///   `# 标题`  → Bold（井号剥离）
///   `@某人`   → Mention
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntityKind {
    Bold,
    /// `text_mention`：蓝色自定义文本，点击进对的人。只需 user id。
    Mention(i64),
    /// `mention`：`@username` 形式。**正文里已经含 `@username` 文本**，
    /// 实体区间覆盖这段文本即可（不需要 user id）。
    UsernameMention,
}

/// `{{at_me}}` 在 Telegram 侧的临时占位符。用 Unicode 私用区字符，
/// 正常文案里不可能出现；渲染完成后会被剥离并转成实体区间。
///
/// 格式：`\u{E000}<字节长度>\u{E001}<显示名>`
/// ⚠️ **必须把长度编码进标记**：显示名可能含空格（"John Smith"），也可能
/// 紧跟中文标点（「张三」号车手）——靠「取到空白为止」的启发式必然切错。
const AT_MARK: char = '\u{E000}';
/// 长度字段与名字之间的分隔符（同为私用区）。
const AT_SEP: char = '\u{E001}';

/// 被动回复渲染变量。
/// `{{qq_name}}`（群昵称/显示名）、`{{paddock_name}}`（围场用户名）、
/// `{{paddock_id}}`（车手 ID）、`{{code}}`（校验码/重置码）、`{{at_me}}`（@ 车手）。
pub struct ReplyVars {
    pub qq_name: String,
    pub paddock_name: String,
    pub paddock_id: String,
    pub code: String,
    /// QQ 的 member_openid（可为空）
    pub qq_openid: String,
    /// Telegram 的 user id（可为空）
    pub telegram_id: Option<i64>,
    /// Telegram 的 @username（可为空；有它才能用 `@xxx` 形式提及）
    pub telegram_username: Option<String>,
}

/// 主动播报渲染变量。
pub struct BroadcastVars {
    pub track: String,
    pub lap: String,
    pub version: String,
    pub paddock_name: String,
    pub paddock_id: String,
    /// 纪录持有者的 QQ member_openid（可为空）
    pub qq_openid: String,
    /// 纪录持有者的 Telegram user id（可为空）
    pub telegram_id: Option<i64>,
    /// 纪录持有者的 Telegram @username（可为空；有它才能用 `@xxx` 形式提及）
    pub telegram_username: Option<String>,
    /// 这条纪录的圈是否零辅助（TC 与 ABS 全关）——决定用 `template` 还是
    /// `gold_template`。配置缺失（旧模块上传/旧记录）一律 false。
    pub is_gold: bool,
}

fn render(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// 按平台渲染被动回复模板。
///
/// - **QQ**：`{{at_me}}` 内联成 `<qqbot-at-user id=".." />`，`mentions` 为空。
/// - **Telegram**：`{{at_me}}` 先替换成「私用区标记 + 显示名」，渲染完成后
///   扫描标记、剥离并转成 `(字节偏移, 字节长度, user_id)`，供 `text_mention` 实体使用。
///   身份缺失时退化为纯文本 `@名字`（无可点击实体，与 QQ 的退化路径同构）。
pub fn render_for(platform: Platform, template: &str, vars: &ReplyVars) -> Rendered {
    let at_me = at_me_for(
        platform,
        vars.telegram_id,
        vars.telegram_username.as_deref(),
        &vars.qq_openid,
        &vars.paddock_name,
    );
    let text = render(template, &[
        ("qq_name", vars.qq_name.as_str()),
        ("paddock_name", vars.paddock_name.as_str()),
        ("paddock_id", vars.paddock_id.as_str()),
        ("code", vars.code.as_str()),
        ("at_me", at_me.as_str()),
    ]);
    finish(platform, collapse_blank_lines(&text), vars.telegram_id)
}

/// 按平台构造 `{{at_me}}` 的替换值。
///
/// - QQ：内联 XML 标签（`<qqbot-at-user id=".." />`），缺失退化 `@「用户名」`
/// - TG：`<AT_MARK><名字字节长度><AT_SEP><显示名>`，渲染后由 [`finish`] 转成实体
/// - TG 无 user id：**输出空串**（`{{at_me}}` 语义是「@ 这个人」，无身份即无内容）
fn at_me_for(
    platform: Platform,
    telegram_id: Option<i64>,
    telegram_username: Option<&str>,
    qq_openid: &str,
    username: &str,
) -> String {
    match platform {
        Platform::Qq => at_user(qq_openid, username),
        Platform::Telegram => {
            if username.is_empty() {
                return String::new();
            }
            // 优先 `@username` 形式（用户 2026-11-03 定案：与 Telegram 客户端
            // 「右键提及」一致）。正文直接写 `@xxx`，实体区间覆盖它。
            if let Some(un) = telegram_username.filter(|u| !u.is_empty()) {
                return format!("@{un}");
            }
            // 没有 username（用户未设置）：退回 `text_mention` 蓝字形式，
            // 需要 user id 才能点击定位。
            if telegram_id.is_some() {
                return format!("{AT_MARK}{}{AT_SEP}{username}", username.len());
            }
            // 既无 username 也无 id：整个 `{{at_me}}` 输出空串
            // （`{{at_me}}` 语义是「@ 这个人」，没有可 @ 的身份就不该产出内容）。
            String::new()
        }
    }
}

/// 渲染收尾：QQ 直接返回；TG 剥离标记并抽出实体区间。
fn finish(platform: Platform, text: String, telegram_id: Option<i64>) -> Rendered {
    match platform {
        // QQ 的 `***` / `# ` 由 markdown 通道（msg_type=2）渲染，原样保留即可
        Platform::Qq => Rendered { text, entities: Vec::new() },
        Platform::Telegram => parse_entities(text, telegram_id),
    }
}

/// 按平台渲染播报模板（变量集与被动回复不同：无 qq_name / code）。
pub fn render_broadcast_for(platform: Platform, template: &str, vars: &BroadcastVars) -> Rendered {
    let at_me = at_me_for(
        platform,
        vars.telegram_id,
        vars.telegram_username.as_deref(),
        &vars.qq_openid,
        &vars.paddock_name,
    );
    let text = render(template, &[
        ("track", vars.track.as_str()),
        ("lap", vars.lap.as_str()),
        ("version", vars.version.as_str()),
        ("paddock_name", vars.paddock_name.as_str()),
        ("paddock_id", vars.paddock_id.as_str()),
        ("at_me", at_me.as_str()),
    ]);
    finish(platform, collapse_blank_lines(&text), vars.telegram_id)
}

/// 收掉因变量为空而留下的空行。
///
/// 播报模板里 `{{at_me}}` 独占一行（金标模板就是 `# 纪录快讯\n{{at_me}}\n...`），
/// 替换成空串后会残留一行空白，在 Telegram 上表现为标题下方多一个空档。
/// 只处理**整行都是空白**的情况——正文内部的空行（用户刻意留的段落间隔）不动。
fn collapse_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let kept: Vec<&str> = lines
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    kept.join("\n")
}

/// 把渲染后的文本解析成「纯正文 + 实体列表」。
///
/// 处理三类标记（**一次扫描完成**，保证各实体偏移互不错位）：
///   1. `AT_MARK<字节长度>AT_SEP<显示名>` → `Mention` 实体，正文只留显示名
///   2. `***文本***` → `Bold` 实体，正文剥掉六个星号
///   3. 行首 `# 标题` → `Bold` 实体，正文剥掉井号与空格
///
/// ⚠️ **为什么不用 `parse_mode=MarkdownV2`**：那套语法要求正文里 18 个字符
/// （`_ * [ ] ( ) ~ ` > # + - = | { } . !`）全部反斜杠转义，且规则是**上下文相关**
/// 的（正文 18 个、`code` 内 2 个、URL 内 2 个）——一个全局 escape 函数必然出错，
/// 漏一个整条消息就 400 丢失。走 entities 完全绕开转义解析。
fn parse_entities(text: String, user_id: Option<i64>) -> Rendered {
    let mut out = String::with_capacity(text.len());
    let mut entities = Vec::new();
    let mut rest = text.as_str();
    let mut at_line_start = true;

    while !rest.is_empty() {
        // ---- 1. @ 提及标记 ----
        if let Some(after) = rest.strip_prefix(AT_MARK) {
            match after.find(AT_SEP) {
                Some(sep) => {
                    let len_str = &after[..sep];
                    let name_and_rest = &after[sep + AT_SEP.len_utf8()..];
                    if let (Ok(name_len), Some(uid)) = (len_str.parse::<usize>(), user_id) {
                        let name_len = name_len.min(name_and_rest.len());
                        let start = out.len();
                        out.push_str(&name_and_rest[..name_len]);
                        entities.push(Entity { kind: EntityKind::Mention(uid), off: start, len: name_len });
                        rest = &name_and_rest[name_len..];
                        at_line_start = false;
                        continue;
                    }
                    // 解析失败：丢弃标记，保住正文
                    tracing::warn!("[bot] AT 标记解析失败（len={len_str:?}），已丢弃");
                    rest = name_and_rest;
                    continue;
                }
                None => {
                    tracing::warn!("[bot] AT 标记缺少分隔符，已丢弃");
                    rest = after;
                    continue;
                }
            }
        }

        // ---- 2. ***粗体*** ----
        if let Some(after) = rest.strip_prefix("***") {
            if let Some(end) = after.find("***") {
                let inner = &after[..end];
                let start = out.len();
                out.push_str(inner);
                if !inner.is_empty() {
                    entities.push(Entity { kind: EntityKind::Bold, off: start, len: inner.len() });
                }
                rest = &after[end + 3..];
                at_line_start = false;
                continue;
            }
            // 没有配对：当普通字符处理（避免吞掉正文）
        }

        // ---- 3. 行首 `# 标题` ----
        if at_line_start && rest.starts_with("# ") {
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let inner = &rest[2..line_end];
            let start = out.len();
            out.push_str(inner);
            if !inner.is_empty() {
                entities.push(Entity { kind: EntityKind::Bold, off: start, len: inner.len() });
            }
            rest = &rest[line_end..];
            at_line_start = true; // 若 line_end 指向 '\n'，下一轮仍从行首开始
            continue;
        }

        // ---- 4. `@username` 提及 ----
        // ⚠️ 只识别 Telegram 合法用户名形态（字母数字下划线，1–32 位，首字符必须是字母）。
        // 这样围场用户名里若含中文/空格就不会被误判成提及。
        if rest.starts_with('@') {
            let name: String = rest[1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let first_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
            if first_ok && !name.is_empty() && name.len() <= 32 {
                let lit = format!("@{name}");
                let start = out.len();
                out.push_str(&lit);
                entities.push(Entity {
                    kind: EntityKind::UsernameMention,
                    off: start,
                    len: lit.len(),
                });
                rest = &rest[lit.len()..];
                at_line_start = false;
                continue;
            }
        }

        // ---- 5. 普通字符 ----
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        at_line_start = ch == '\n';
        rest = &rest[ch.len_utf8()..];
    }

    Rendered { text: out, entities }
}

// ---------- 播报 ----------

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
    let (username, reg_seq, holder_openid, holder_tg, holder_tg_user) =
        holder_identity(state, holder).await;
    let is_gold = is_zero_assist_lap(state, gp_index, Some(version_code), holder, lap_ms).await;
    broadcast(
        state,
        event,
        &BroadcastVars {
            track: track_display_name_compact(gp_index),
            lap: crate::api::leaderboard::format_lap_ms(lap_ms),
            version: crate::api::laps::version_display(version_code),
            qq_openid: holder_openid,
            telegram_id: holder_tg,
            telegram_username: holder_tg_user,
            paddock_name: username,
            paddock_id: reg_seq.to_string(),
            is_gold,
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
    let (username, reg_seq, holder_openid, holder_tg, holder_tg_user) =
        holder_identity(state, holder_id).await;
    let event = if alltime_hit { "record_alltime" } else { "record_version" };
    // 金标判定：历史纪录跨版本、records 不存 alltime 的版本号 → 传 None 按
    // (gp, user, lap_ms) 定位；版本纪录传具体版本更精确。
    let is_gold = is_zero_assist_lap(
        state,
        gp_index,
        if alltime_hit { None } else { Some(version_code) },
        holder_id,
        holder_ms,
    )
    .await;
    broadcast(
        state,
        event,
        &BroadcastVars {
            track: track_display_name_compact(gp_index),
            lap: crate::api::leaderboard::format_lap_ms(holder_ms),
            version: crate::api::laps::version_display(version_code),
            qq_openid: holder_openid,
            telegram_id: holder_tg,
            telegram_username: holder_tg_user,
            paddock_name: username,
            paddock_id: reg_seq.to_string(),
            is_gold,
        },
    )
    .await;
}

/// 取用户的围场名 + 车手 ID + 两平台身份（缺失各自为空）。
type Holder = (String, i64, String, Option<i64>, Option<String>);

async fn holder_identity(state: &App, user_id: Uuid) -> Holder {
    sqlx::query_as::<_, Holder>(
        "SELECT username, reg_seq, COALESCE(member_openid,''), telegram_id, telegram_username \
         FROM users WHERE id=$1",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((String::new(), 0, String::new(), None, None))
}

/// 判定"刷新纪录的那条圈"是否零辅助（TC 与 ABS 均关）——决定播报用普通模板还是金标模板。
///
/// 数据源是 `laps` 全量留档（那里存原始五维），不是 best_laps（那是"该维度最快圈"，
/// 与"本条纪录圈"在删除/易主场景可能不是同一条）。配置缺失（旧模块上传）→ false。
///
/// `version_code = None` 用于历史（alltime）纪录：records 表不存 alltime 行的版本，
/// 且跨版本比对时上传方的 version_code 未必是创纪录那条圈的版本——用
/// (gp, user, lap_ms) 三键定位更稳，同值同人的歧义由 ORDER BY created_at DESC 兜住。
async fn is_zero_assist_lap(
    state: &App,
    gp_index: i16,
    version_code: Option<i32>,
    user_id: Uuid,
    lap_ms: i32,
) -> bool {
    let row: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> =
        match version_code {
            Some(vc) => {
                sqlx::query_as(
                    "SELECT tc_mode, tc_strength, abs_mode, abs_strength FROM laps \
                     WHERE gp_index=$1 AND version_code=$2 AND user_id=$3 AND lap_ms=$4 \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(gp_index)
                .bind(vc)
                .bind(user_id)
                .bind(lap_ms)
                .fetch_optional(&state.pool)
                .await
                .ok()
                .flatten()
            }
            None => {
                sqlx::query_as(
                    "SELECT tc_mode, tc_strength, abs_mode, abs_strength FROM laps \
                     WHERE gp_index=$1 AND user_id=$2 AND lap_ms=$3 \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(gp_index)
                .bind(user_id)
                .bind(lap_ms)
                .fetch_optional(&state.pool)
                .await
                .ok()
                .flatten()
            }
        };
    let Some((tcm, tcs, absm, abss)) = row else {
        return false;
    };
    let f = crate::api::laps::flags_of(
        None,
        tcm.as_deref(),
        tcs.as_deref(),
        absm.as_deref(),
        abss.as_deref(),
    );
    f.is_zero_assist()
}

/// 主动播报：把事件广播到两个平台各自勾选的目标（broadcast 规则渲染后逐目标发送）。
/// 无勾选目标时静默（tracing 记录），不影响主流程。
///
/// ⚠️ 两个平台的渲染必须各自做：`{{at_me}}` 的语法不同（QQ 内联标签 / TG 实体）。
/// 规则集与金标判定共用——这是「交互逻辑一致」的落点。
pub async fn broadcast(state: &App, event: &str, vars: &BroadcastVars) {
    let qq_targets = crate::qq_bot::broadcast_groups(&state.pool).await;
    let tg_targets = crate::telegram_bot::broadcast_chats(&state.pool).await;
    if qq_targets.is_empty() && tg_targets.is_empty() {
        return;
    }
    let rules = load_rules(&state.pool).await;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "broadcast") {
        if !rule_matches(r, "", event) {
            continue;
        }
        // 金标模板：刷新纪录的圈为零辅助时改用它（留空 = 不区分，仍用 template）。
        let tpl = if vars.is_gold && !r.gold_template.trim().is_empty() {
            &r.gold_template
        } else {
            &r.template
        };
        for g in &qq_targets {
            let content = render_broadcast_for(Platform::Qq, tpl, vars).text;
            // 主动消息：不带 msg_id（未认证额度极低，失败只记日志）
            crate::qq_bot::send_direct_group(state, g, &content).await;
        }
        for c in &tg_targets {
            crate::telegram_bot::send_direct_chat(state, c, tpl, vars).await;
        }
    }
}

// ---------- 失败文案 ----------

/// 失败类型：对应 BotRule 的每类独立模板字段。
#[derive(Clone, Copy)]
pub enum FailType {
    InvalidCode,
    DupOpenid,
    NoIdentity,
    NoUser,
}

impl FailType {
    /// 该失败类型对应的 BotRule 字段名（与 action_metas().failure_fields 的 key 一致）。
    fn field_key(self) -> &'static str {
        match self {
            FailType::InvalidCode => "invalid_code_template",
            FailType::DupOpenid => "dup_openid_template",
            FailType::NoIdentity => "no_identity_template",
            FailType::NoUser => "no_user_template",
        }
    }
}

/// 把内置默认文案里的 QQ 措辞按平台适配。
///
/// ⚠️ **只作用于内置默认**（或与默认逐字相同的值），用户改过的文案原样输出——
/// 擅自改写用户文案会让管理员困惑「我写的怎么变了」。
fn platform_adapt(platform: Platform, text: String) -> String {
    match platform {
        Platform::Qq => text,
        Platform::Telegram => text
            .replace("该 QQ 身份", "该 Telegram 身份")
            .replace("QQ 身份", "Telegram 身份")
            .replace(
                "请确认已在 QQ 群设置中允许机器人获取群信息",
                "请确认已关闭机器人隐私模式（BotFather /setprivacy）或将机器人设为群管理员",
            ),
    }
}

/// 各失败类型的内置默认文案：委托 action_metas()（唯一事实源），再按平台适配措辞。
/// 模板变量 {{code}}（校验码）/ {{name}}（用户名）——动态部分经变量注入。
/// 该动作未声明此类失败字段时（如普通回复依赖身份却查不到账号），
/// 用一条人类可读的通用兜底，绝不吐字段名给群成员看。
fn builtin_reason(platform: Platform, action: &str, key: &str, code: &str) -> String {
    let tpl = fail_default(action, key);
    if tpl.is_empty() {
        let fallback = match key {
            "no_identity_template" => "无法识别你的群身份，请确认机器人能读取群内消息。",
            "no_user_template" => "找不到该身份对应的车手账号，请先注册围场通行证。",
            _ => "操作未能完成，请稍后重试。",
        };
        return platform_adapt(platform, fallback.to_string());
    }
    platform_adapt(platform, tpl.replace("{{code}}", code))
}

/// 动作失败文案：每类失败独立模板优先 → fail_template → 内置默认原文。
/// 模板内可用 {{code}}/{{name}}；无任何隐藏占位符。
pub fn fail_type_reply(
    platform: Platform,
    rule: &BotRule,
    t: FailType,
    code: &str,
    username: &str,
) -> String {
    let key = t.field_key();
    let specific = rule_field(rule, key);
    let default = fail_default(&rule.action, key);
    let tpl = if !specific.trim().is_empty() {
        // 与内置默认逐字相同 = 用户没改过 → 允许按平台适配措辞
        if specific == default {
            platform_adapt(platform, specific.to_string())
        } else {
            specific.to_string()
        }
    } else if !rule.fail_template.trim().is_empty() {
        rule.fail_template.clone()
    } else {
        return builtin_reason(platform, &rule.action, key, code);
    };
    tpl.replace("{{code}}", code)
       .replace("{{name}}", username)
       .replace("{{qq_name}}", "") // 失败场景 qq_name 多数无效，留空避免渲染残留
       .replace("{{at_me}}", "")   // 同上：失败文案无法定位群身份，@ 标签留空避免残留
}

// ---------- 三个内置动作（返回回复文本；None = 静默不回复） ----------
//
// ⚠️ 返回 `Option<String>` 而不是 `String`：`reg_code` 有一条「提不出 #码就静默」
// 的路径。若用空串表示静默，适配层会照常发一条**空消息**，并消耗 QQ 的被动回复窗
// （每条消息最多 5 次）。`None` 才是「什么都不做」。

/// 注册校验（action=reg_code）：从触发词后提取校验码 → pending_regs 定位会话 →
/// 直接建号 → 返回成功/失败文案。提不出 #码 → `None`（静默）。
/// 约束（PADDOCK_PLAN §6 S4）：一条码一身份；重复绑定他人码会被唯一约束拦。
pub async fn handle_reg_code(
    state: &App,
    platform: Platform,
    rule: &BotRule,
    content: &str,
    qq_name: &str,
    identity_id: &str,
) -> Option<String> {
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
    if code.is_empty() {
        // 锚点后没有 #xxx：静默——不响应裸关键词（防误触发），不消耗回复窗
        tracing::info!("[bot] reg_code 关键词命中但提不出校验码，静默: {content}");
        return None;
    }

    let reply = if identity_id.is_empty() {
        fail_type_reply(platform, rule, FailType::NoIdentity, "", "")
    } else {
        let n: Result<i64, _> = sqlx::query_scalar(
            "SELECT count(*) FROM pending_regs WHERE reg_code = $1 AND expires_at > now()",
        )
        .bind(&code)
        .fetch_one(&state.pool)
        .await;
        match n {
            Ok(0) | Err(_) => fail_type_reply(platform, rule, FailType::InvalidCode, &code, ""),
            Ok(_) => {
                // 该身份已被其他在途会话占用：直接拒绝（防一人多账号）。
                // ⚠️ 这里查的是 `users`（已建号者），不是 pending_regs——后者
                // 的 member_openid 列恒空（从未被写入），旧实现查它等于死逻辑。
                let already_bound = lookup_identity(&state.pool, platform, identity_id).await.is_some();
                if already_bound {
                    fail_type_reply(platform, rule, FailType::DupOpenid, &code, "")
                } else {
                    // 建号事务：DELETE pending RETURNING 锁存数据 → INSERT users。
                    // 失败时 pending 已被事务回滚恢复，用户可重试。
                    match auth_handlers::create_user_from_pending(&state.pool, &code, platform, identity_id).await {
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
                                format!("注册成功：{username}（车手 ID {reg_seq}，经 {} 群内校验码建号）", platform.label()),
                                json!({"reg_seq": reg_seq, "code": code, "platform": platform.label()}),
                            );
                            let vars = ReplyVars {
                                qq_name: qq_name.to_string(),
                                paddock_name: username,
                                paddock_id: reg_seq.to_string(),
                                code: code.to_string(),
                                qq_openid: if platform == Platform::Qq { identity_id.to_string() } else { String::new() },
                                telegram_id: if platform == Platform::Telegram { identity_id.parse().ok() } else { None },
                                telegram_username: None,
                            };
                            render_for(platform, &rule.template, &vars).text
                        }
                        Err((status, msg)) => {
                            tracing::warn!("建号失败 {status}: {msg}（code={code}）");
                            // 建号事务内两失败分支：404=码被并发用掉（InvalidCode）、409=已绑定（DupOpenid）
                            let ft = if status == axum::http::StatusCode::CONFLICT {
                                FailType::DupOpenid
                            } else {
                                FailType::InvalidCode
                            };
                            fail_type_reply(platform, rule, ft, &code, "")
                        }
                    }
                }
            }
        }
    };
    Some(reply)
}

/// 密码重置（action=reset_password）：消息严格等于触发词才进入本函数。
/// 不提取用户名——账号按发言者平台身份反查（注册校验绑定时的身份）。
pub async fn handle_reset_password(
    state: &App,
    platform: Platform,
    rule: &BotRule,
    qq_name: &str,
    identity_id: &str,
) -> Option<String> {
    let reply = if identity_id.is_empty() {
        fail_type_reply(platform, rule, FailType::NoIdentity, "", "")
    } else {
        let Some((username, reg_seq)) = lookup_identity(&state.pool, platform, identity_id).await
        else {
            return Some(fail_type_reply(platform, rule, FailType::NoUser, "", ""));
        };
        if username.is_empty() {
            fail_type_reply(platform, rule, FailType::NoUser, "", "")
        } else {
            match auth_handlers::create_reset_code(&state.pool, &username).await {
                Ok(code) => {
                    crate::applog::log_event(
                        &state.pool, "info", "auth", "reset_code_issued", &username,
                        "签发密码重置码（群内申请，30 分钟有效）".to_string(),
                        json!({"platform": platform.label()}),
                    );
                    let vars = ReplyVars {
                        qq_name: qq_name.to_string(),
                        paddock_name: username,
                        paddock_id: reg_seq.to_string(),
                        code,
                        qq_openid: if platform == Platform::Qq { identity_id.to_string() } else { String::new() },
                        telegram_id: if platform == Platform::Telegram { identity_id.parse().ok() } else { None },
                        telegram_username: None,
                    };
                    render_for(platform, &rule.template, &vars).text
                }
                Err(_) => fail_type_reply(platform, rule, FailType::NoUser, "", &username),
            }
        }
    };
    Some(reply)
}

/// 查询用户名（action=query_username）：消息严格等于触发词才进入本函数。
/// 按发言者平台身份反查账号（与重置密码同源语义，不提取用户名），
/// 查不到走 no_user 失败文案。纯查询、无副作用。
pub async fn handle_query_username(
    state: &App,
    platform: Platform,
    rule: &BotRule,
    qq_name: &str,
    identity_id: &str,
) -> Option<String> {
    let reply = if identity_id.is_empty() {
        fail_type_reply(platform, rule, FailType::NoIdentity, "", "")
    } else {
        let Some((username, reg_seq)) = lookup_identity(&state.pool, platform, identity_id).await
        else {
            return Some(fail_type_reply(platform, rule, FailType::NoUser, "", ""));
        };
        if username.is_empty() {
            fail_type_reply(platform, rule, FailType::NoUser, "", "")
        } else {
            let vars = ReplyVars {
                qq_name: qq_name.to_string(),
                paddock_name: username,
                paddock_id: reg_seq.to_string(),
                code: String::new(),
                qq_openid: if platform == Platform::Qq { identity_id.to_string() } else { String::new() },
                telegram_id: if platform == Platform::Telegram { identity_id.parse().ok() } else { None },
                telegram_username: None,
            };
            render_for(platform, &rule.template, &vars).text
        }
    };
    Some(reply)
}

/// 普通回复：模板依赖身份却查不到账号时走失败文案（绝不渲染空串）。
pub async fn handle_plain_reply(
    state: &App,
    platform: Platform,
    rule: &BotRule,
    qq_name: &str,
    identity_id: &str,
) -> String {
    let found = lookup_identity_full(&state.pool, platform, identity_id).await;
    let (name, seq, tg_user) = match found {
        Some(h) => (h.paddock_name, h.reg_seq, h.telegram_username),
        None => (String::new(), 0, None),
    };
    if template_needs_identity(&rule.template) && name.is_empty() {
        let ft = if identity_id.is_empty() { FailType::NoIdentity } else { FailType::NoUser };
        return fail_type_reply(platform, rule, ft, "", "");
    }
    let vars = ReplyVars {
        qq_name: qq_name.to_string(),
        paddock_name: name,
        paddock_id: seq.to_string(),
        code: String::new(),
        qq_openid: if platform == Platform::Qq { identity_id.to_string() } else { String::new() },
        telegram_id: if platform == Platform::Telegram { identity_id.parse().ok() } else { None },
        telegram_username: tg_user,
    };
    render_for(platform, &rule.template, &vars).text
}

/// 一条消息的完整决策：遍历规则 → 命中第一条即产出回复文本。`None` = 静默。
///
/// ⚠️ **一条消息只命中一条规则**（命中即返回，不继续匹配后续规则）。
/// 这是原实现的语义，必须保留：`reg_code` 提不出码时整体结束，不会落到下一条规则。
pub async fn decide_reply(
    state: &App,
    platform: Platform,
    content: &str,
    qq_name: &str,
    identity_id: &str,
) -> Option<String> {
    let normalized = content.replace('＃', "#");
    let rules = load_rules(&state.pool).await;
    for r in rules.iter().filter(|r| r.enabled && r.kind == "reply") {
        if !rule_matches(r, &normalized, "") {
            continue;
        }
        return match r.action.as_str() {
            "reg_code" => handle_reg_code(state, platform, r, &normalized, qq_name, identity_id).await,
            "reset_password" => handle_reset_password(state, platform, r, qq_name, identity_id).await,
            "query_username" => handle_query_username(state, platform, r, qq_name, identity_id).await,
            _ => Some(handle_plain_reply(state, platform, r, qq_name, identity_id).await),
        };
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(qq_openid: &str, tg: Option<i64>, name: &str) -> ReplyVars {
        ReplyVars {
            qq_name: "群昵称".into(),
            paddock_name: name.into(),
            paddock_id: "42".into(),
            code: "ABCD1234".into(),
            qq_openid: qq_openid.into(),
            telegram_id: tg,
            telegram_username: None,
        }
    }

    /// QQ 与 Telegram 渲染出的**纯文本**必须逐字相同（@ 语法除外）。
    #[test]
    fn render_text_identical_across_platforms() {
        let tpl = "车手「{{paddock_name}}」，您的重置码已生成：{{code}}";
        let qq = render_for(Platform::Qq, tpl, &vars("OPENID", Some(123), "张三"));
        let tg = render_for(Platform::Telegram, tpl, &vars("OPENID", Some(123), "张三"));
        assert_eq!(qq.text, tg.text);
        assert_eq!(qq.text, "车手「张三」，您的重置码已生成：ABCD1234");
        assert!(qq.entities.is_empty(), "QQ 的格式走 markdown 通道，不产生实体");
    }

    /// QQ 的 at_me 是内联 XML 标签（走 markdown 通道）。
    #[test]
    fn qq_at_me_inlines_xml_tag() {
        let r = render_for(Platform::Qq, "{{at_me}} 刷新纪录", &vars("OPENID", None, "张三"));
        assert!(r.text.contains(r#"<qqbot-at-user id="OPENID" />"#));
        assert!(r.entities.is_empty());
    }

    /// ⚠️ QQ 侧无 openid 时 `{{at_me}}` 也必须输出空串（2026-11-03 用户定案）。
    /// 旧实现退化 `@「张三」`，在「持有者是 Telegram 用户」时多出一行假提及。
    #[test]
    fn qq_at_me_is_empty_without_openid() {
        let r = render_for(Platform::Qq, "前缀 {{at_me}} 后缀", &vars("", None, "张三"));
        assert_eq!(r.text, "前缀  后缀");
        assert!(!r.text.contains('@'), "不得残留 @：{}", r.text);
        assert!(!r.text.contains('「'), "不得残留书名号：{}", r.text);
    }

    /// Telegram 的 at_me 剥离成实体区间，正文里只剩显示名。
    #[test]
    fn telegram_at_me_becomes_mention_entity() {
        let r = render_for(Platform::Telegram, "{{at_me}} 刷新纪录", &vars("", Some(999), "张三"));
        assert_eq!(r.text, "张三 刷新纪录");
        assert_eq!(r.entities.len(), 1);
        let e = &r.entities[0];
        assert_eq!(e.kind, EntityKind::Mention(999));
        assert_eq!(&r.text[e.off..e.off + e.len], "张三");
    }

    /// 名字里含空格时，实体区间必须精确覆盖整个名字（不能取到第一个空白为止）。
    #[test]
    fn telegram_at_me_handles_spaces_in_name() {
        let r = render_for(
            Platform::Telegram,
            "{{at_me}} wins",
            &vars("", Some(7), "John Smith"),
        );
        assert_eq!(r.text, "John Smith wins");
        let e = &r.entities[0];
        assert_eq!(&r.text[e.off..e.off + e.len], "John Smith");
    }

    /// ⚠️ **无 Telegram id 时 `{{at_me}}` 必须输出空串**（2026-11-03 用户定案）。
    ///
    /// 两轮教训：① 曾写成 `@{username}` → @ 到陌生人（围场名与 TG 用户名不同命名空间）；
    /// ② 改成"只显示名字" → 播报里多出一行光秃秃的名字。语义是「@ 这个人」，
    /// 没有可 @ 的身份就不该产出任何内容。
    #[test]
    fn telegram_at_me_is_empty_without_identity() {
        let r = render_for(Platform::Telegram, "前缀 {{at_me}} 后缀", &vars("", None, "张三"));
        assert_eq!(r.text, "前缀  后缀", "无身份时 at_me 为空串");
        assert!(!r.text.contains('@'), "绝不能出现裸 @");
        assert!(!r.text.contains("张三"), "不得残留名字");
        assert!(r.entities.is_empty(), "无 id 不得产生提及实体");
    }

    /// 有 @username 时必须用 `@xxx` 形式（与 Telegram 客户端「右键提及」一致）。
    #[test]
    fn telegram_prefers_username_mention() {
        let mut v = vars("", Some(123), "张三");
        v.telegram_username = Some("zhangsan".into());
        let r = render_for(Platform::Telegram, "{{at_me}} 好", &v);
        assert_eq!(r.text, "@zhangsan 好", "必须显示 @username 形式");
        assert_eq!(r.entities.len(), 1);
        let e = &r.entities[0];
        assert_eq!(e.kind, EntityKind::UsernameMention);
        assert_eq!(&r.text[e.off..e.off + e.len], "@zhangsan");
    }

    /// 没有 username 时退回蓝字 text_mention（只需 user id）。
    #[test]
    fn telegram_falls_back_to_text_mention() {
        let mut v = vars("", Some(123), "张三");
        v.telegram_username = None;
        let r = render_for(Platform::Telegram, "{{at_me}} 好", &v);
        assert_eq!(r.text, "张三 好");
        let e = &r.entities[0];
        assert_eq!(e.kind, EntityKind::Mention(123));
    }

    /// 围场用户名含中文时不得被误判成 @username 提及。
    #[test]
    fn chinese_name_not_treated_as_username() {
        let r = parse_entities("车手 @张三 你好".to_string(), None);
        assert!(
            !r.entities.iter().any(|e| e.kind == EntityKind::UsernameMention),
            "中文名不是合法 Telegram 用户名"
        );
    }

    /// `{{at_me}}` 独占一行且为空时，不得留下空行。
    #[test]
    fn empty_at_me_line_is_collapsed() {
        let tpl = preset_by_id("preset-bc-alltime").gold_template;
        let r = render_broadcast_for(
            Platform::Telegram,
            &tpl,
            &BroadcastVars {
                track: "🇮🇹蒙扎".into(),
                lap: "1:23.456".into(),
                version: "8.0.6".into(),
                paddock_name: "张三".into(),
                paddock_id: "42".into(),
                qq_openid: String::new(),
                telegram_id: None, // 无 TG 身份
                telegram_username: None,
                is_gold: true,
            },
        );
        assert!(!r.text.contains("\n\n"), "不得残留连续空行：{:?}", r.text);
        assert!(!r.text.starts_with('\n'), "不得以空行开头：{:?}", r.text);
        // 标题下方应直接是车手号那一行
        let lines: Vec<&str> = r.text.lines().collect();
        assert_eq!(lines[0], "纪录快讯");
        assert_eq!(lines[1], "42 号车手", "空 at_me 行必须收掉");
    }

    /// `***x***` 必须转成 Bold 实体，星号从正文剥掉。
    #[test]
    fn telegram_bold_markup_becomes_entity() {
        let r = render_for(
            Platform::Telegram,
            "车手 ***张三*** 刷新",
            &vars("", None, "张三"),
        );
        assert_eq!(r.text, "车手 张三 刷新", "星号必须剥离");
        assert_eq!(r.entities.len(), 1);
        let e = &r.entities[0];
        assert_eq!(e.kind, EntityKind::Bold);
        assert_eq!(&r.text[e.off..e.off + e.len], "张三");
    }

    /// 行首 `# 标题` 必须转成 Bold 实体，井号从正文剥掉。
    #[test]
    fn telegram_hash_heading_becomes_entity() {
        let r = render_for(
            Platform::Telegram,
            "# 纪录快讯\n正文内容",
            &vars("", None, "张三"),
        );
        assert_eq!(r.text, "纪录快讯\n正文内容", "井号必须剥离");
        assert_eq!(r.entities.len(), 1);
        let e = &r.entities[0];
        assert_eq!(e.kind, EntityKind::Bold);
        assert_eq!(&r.text[e.off..e.off + e.len], "纪录快讯");
    }

    /// 完整金标模板：标题 + @ + 粗体三者在同一条消息里都要正确。
    #[test]
    fn telegram_full_gold_template() {
        let tpl = preset_by_id("preset-bc-alltime").gold_template;
        let r = render_broadcast_for(
            Platform::Telegram,
            &tpl,
            &BroadcastVars {
                track: "🇮🇹蒙扎".into(),
                lap: "1:23.456".into(),
                version: "8.0.6".into(),
                paddock_name: "张三".into(),
                paddock_id: "42".into(),
                qq_openid: String::new(),
                telegram_id: Some(123),
                telegram_username: None,
                is_gold: true,
            },
        );
        // 正文里不应残留 QQ markdown 标记
        assert!(!r.text.contains("***"), "不得残留三星号：{}", r.text);
        assert!(!r.text.starts_with('#'), "不得残留井号标题：{}", r.text);
        // 实体：1 个 Mention + 2 个 Bold（标题 + 车手名）
        let mentions: Vec<_> = r.entities.iter().filter(|e| matches!(e.kind, EntityKind::Mention(_))).collect();
        let bolds: Vec<_> = r.entities.iter().filter(|e| e.kind == EntityKind::Bold).collect();
        assert_eq!(mentions.len(), 1, "应有一个 @ 实体");
        assert_eq!(bolds.len(), 2, "标题 + 粗体车手名，共两个 Bold 实体");
    }

    /// 内置失败文案里的「QQ」措辞必须按平台替换。
    #[test]
    fn builtin_reason_adapts_platform_wording() {
        let rule = preset_by_id("preset-reg");
        let qq = fail_type_reply(Platform::Qq, &rule, FailType::NoIdentity, "", "");
        let tg = fail_type_reply(Platform::Telegram, &rule, FailType::NoIdentity, "", "");
        assert!(qq.contains("QQ"), "QQ 侧保留原措辞");
        assert!(!tg.contains("QQ"), "TG 侧不得出现 QQ 字样：{tg}");
        assert!(tg.contains("隐私模式"), "TG 侧给出可操作提示：{tg}");
    }

    /// 用户改过的文案不按平台改写（擅自改写会让管理员困惑）。
    #[test]
    fn user_customized_text_not_rewritten() {
        let mut rule = preset_by_id("preset-reg");
        rule.no_identity_template = "我自定义的失败文案".into();
        let tg = fail_type_reply(Platform::Telegram, &rule, FailType::NoIdentity, "", "");
        assert_eq!(tg, "我自定义的失败文案");
    }

    /// 规则匹配：动作规则忽略 conditions，只看 keyword。
    #[test]
    fn action_rule_ignores_conditions() {
        let mut r = preset_by_id("preset-reg");
        r.conditions = vec![RuleCond {
            field: "content".into(),
            op: "equals".into(),
            value: "永远不匹配".into(),
        }];
        assert!(rule_matches_public(&r, "申请围场通行证#ABC", ""));
    }

    /// exact 模式的规则：闲聊里含触发词不命中。
    #[test]
    fn exact_rule_rejects_chitchat() {
        let r = preset_by_id("preset-reset");
        assert!(!rule_matches_public(&r, "我想我需要重置密码", ""));
        assert!(rule_matches_public(&r, "  我需要重置密码  ", ""));
    }

    /// contains 模式的规则：闲聊里含触发词会进入 handler（由取码步骤拦）。
    #[test]
    fn contains_rule_enters_handler_for_chitchat() {
        let r = preset_by_id("preset-reg");
        assert!(rule_matches_public(&r, "我想问问申请围场通行证怎么办", ""));
    }

    /// 全角 ＃ 归一化在 decide_reply 入口做——这里验证归一化后的匹配。
    #[test]
    fn fullwidth_hash_normalized() {
        let r = preset_by_id("preset-reg");
        let normalized = "申请围场通行证＃ABC".replace('＃', "#");
        assert!(normalized.contains("申请围场通行证#"));
        assert!(rule_matches_public(&r, &normalized, ""));
    }
}
