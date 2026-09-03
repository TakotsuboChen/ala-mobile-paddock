# 围场 Bot 消息收发全链路 & 规则引擎差距分析（v2）

> 生成：2026-09-03 · 基于 paddock-api v16 实际代码全文核对（qq_bot.rs 937 行 / auth_handlers.rs / admin.rs / admin_settings.html）
> v2 修订：① 频控数字更正为官方文档原文；② 补齐 C2C 单聊路径——解释「私聊回复你好、群聊不回复」的代码事实；③ 发送队列与 access_token 细节补全。
> 性质：**现状文档 + 修改方案**，未做任何代码改动。

---

## 一、入站：QQ 平台 → webhook

```
手机QQ群消息 / 私聊消息
   │
   ▼
QQ平台侧判断（群消息需群内「获取群内全部消息」授权，手机QQ群机器人设置里）
   │
   ▼
POST https://paddock.takotsubo.cloud/qq/webhook
```

### 验签与分发层（`webhook` 函数，按顺序执行）

1. body 不是 JSON → 忽略，回 2xx
2. `bot_app_id` / `bot_app_secret` 未配置（管理端设置页维护）→ 忽略
3. Ed25519 验签失败 → 401 拒绝
4. payload 原文全量落日志（诊断用）
5. `op=13` → 回调地址验证应答（回 `{plain_token, signature}`），流程结束
6. `op=0` → 按事件类型 `t` 分发：
   - `GROUP_MESSAGE_CREATE`（全量）/ `GROUP_AT_MESSAGE_CREATE`（@消息）→ `handle_group_message`
   - `C2C_MESSAGE_CREATE` → `handle_c2c_message`
   - 其他事件 → 忽略
7. 无论业务结果如何，webhook 恒回 2xx（平台对非 2xx 会重试）

### payload 中实际消费的字段

| 字段 | 群聊 | 单聊 | 用途 |
|---|---|---|---|
| `d.id`（ROBOT1.0_…） | ✓ | ✓ | 被动回复凭据 |
| `d.author.member_openid` | ✓ | — | 建号绑定 + 围场变量反查 |
| `d.author.user_openid` / `author.id` | — | ✓（双取兜底） | 单聊回复目标 |
| `d.author.username` | ✓ | ✗（未取） | `{{qq_name}}` 变量 |
| `d.group_openid` | ✓ | — | 回哪个群 + 群登记 |
| `d.content` | ✓ | ✓ | 触发匹配输入 |
| `d.message_scene.ext` 的 `msg_idx=REFIDX_xxx` | ✓ | ✓ | 引用回复 |

---

## 二、出站：发送队列与 REST 调用

### 队列（`run_sender`）

- 所有出站消息先入 `mpsc` 队列（容量 64），单 worker 串行消费
- **每条消息固定间隔 2 秒**（≈30 条/分钟，注释按未认证 30/qpm 频控设计）
- 发送失败只记 `tracing::error`，不重试、不阻塞队列
- 每发一条都重新取一次 access_token（无缓存；低频下可接受，代码注释已标注可加 moka）

### `send_message` 的请求体组装

| 字段 | 逻辑 |
|---|---|
| `msg_type: 0` | 恒为纯文本 |
| `content` | 模板渲染结果 |
| `msg_seq: 1` | 恒为 1（同 msg_id 回复超过 1 次会因 seq 重复被平台去重）⚠️ |
| `msg_id` | 非空才带 → 被动回复语义 |
| `message_reference.message_id` | 非空才带 → 引用回复形态 |
| 鉴权 | `Authorization: QQBot {token}`（非 Bearer）+ `X-Union-Appid` |

### 平台频控（官方文档原文，2026-09 核对）

**主动消息**（不带 msg_id）：

| 认证类型 | Bot 维度（发送方） | 单关系维度（接收方） | 每日上限 |
|---|---|---|---|
| 企业认证 / 个人身份证认证 | 60/qpm | 20/qpm | 每群每天最多接收 1000 条 |
| 未认证 | 30/qpm | 20/qpm | 每群每天最多接收 1000 条 |

**被动消息**（带 msg_id）：群聊 5 分钟窗 / 每消息 5 次；单聊 60 分钟窗 / 每消息 4 次。
**接口频率限制**：发消息接口 100 QPS（含主动+被动）。

### 三种出站形态

| 形态 | 触发场景 | 约束 |
|---|---|---|
| 被动 + 引用（v16 默认） | 有人发消息 → bot 回复 | msg_id 窗口如上 |
| 主动裸发 | 纪录刷新播报 | 未认证 30/qpm，额度极低；用户关「机器人主动发言」则发不出 |
| 单聊被动 + 引用 | 私聊指令 | 60 分钟 / 4 次 |

---

## 三、群聊路径：`handle_group_message` 真实执行链

```
解析 GroupMessage（id/content/group_openid/author/message_scene）
  │ content 为空 或 group_openid 为空 → 静默返回
  ▼
msg_id = d.id（空则事件外层 id 兜底）；ref_id = message_scene.ext 的 msg_idx=REFIDX_
  ▼
remember_group：群登记（新群出现时异步调 /v2/groups/{id}/info 拉群名缓存，
               11253 白名单权限不足则只记日志、选择器回退显示 openid）
  ▼
归一化：全角＃ → 半角#
  ▼
load_rules：库 bot_message_rules 有配置用配置；无配置或空列表 → 2 条预设
  ▼
按顺序找第一条「enabled 且 kind=reply 且条件命中」的规则
  （conditions 按 且/或 组合，包含/不包含/等于/开头是/结尾是）
  ▼
├─ 无命中 → 【bot 静默，什么都不回】★这就是"群聊不回复你好"的原因
└─ 命中 → 按 rule.action 三选一（一条消息只执行一条规则）：
```

### action = reg_code（注册校验）判断链

| 序 | 判断 | 失败出口 | 走 fail_template？ |
|---|---|---|---|
| 0 | 提取：锚点后文本去 # 大写 = 校验码；**锚点后没有 #码 → 提取结果为空串，不单独拦截**，带空码进 1 | — | — |
| 1 | `member_openid` 为空？ | 「无法识别你的群身份…」（**写死**） | **否** |
| 2 | `pending_regs` 无此码/已过期 | 「校验码 XXXX 无效或已过期，请在围场页重新申请」 | 是 |
| 3 | 该 openid 已有其他在途会话（防一 QQ 多号） | 「该 QQ 身份已有其他注册校验在途，请勿重复申请」 | 是 |
| 4a | `create_user_from_pending` 事务内 DELETE pending RETURNING 失败（判断 2 后被并发用掉/过期） | 404「校验码无效或已过期，请重新申请」 | 是 |
| 4b | 事务内 users 表查该 openid 已绑定 | 409「该 QQ 身份已绑定过账号，不允许重复注册」 | 是 |
| 4c | 全过 → INSERT users（锁存用户名/密码哈希/车手 ID）→ **成功** | template 渲染 {{paddock_name}}/{{paddock_id}}/{{code}} | — |

`fail_reply` 逻辑：`fail_template` 非空 → 模板替换 `{{reason}}`；空 → 内置 reason 原文直出。

### action = reset_password（密码重置）判断链

| 序 | 判断 | 失败出口 |
|---|---|---|
| 0 | 提取：锚点后文本 = 用户名 | — |
| 1 | users 表无该用户名（`create_reset_code`） | 「用户名不存在」（经 fail_reply） |
| 2 | 有 → 删旧码（最后一码有效）→ 生成 8 位码 → 30 分钟有效 → **成功** | template 渲染 {{code}}/{{qq_name}}/{{paddock_name}} |

### action = reply（普通回复）执行链

按 `member_openid` 反查 users：查到 → {{paddock_name}}/{{paddock_id}} 有值；查不到 → 空串。
渲染 template → 引用回复。**无条件兜底文案。**

---

## 四、单聊路径：`handle_c2c_message` 真实执行链 ★群聊/私聊行为不一致的根源

```
解析 C2cMessage
  │ content 为空 或 user_openid 为空 → 静默返回
  ▼
msg_id / ref_id 提取（同群聊）
  ▼
★不走规则引擎，硬编码三分支：
├─ content 含「申请围场通行证」
│    → 「注册校验需要在群内完成：请在群里发送「申请围场通行证#校验码」」
│      （单聊无 member_openid，无法建号——设计如此）
├─ content 含「重置密码」
│    → 取其后文本为用户名 → create_reset_code
│      ├─ 成功 → 「重置码已生成：XXXX…」（内置文案，非规则模板）
│      └─ 失败 → 错误原文
└─ 其他任何内容（包括"你好"）
     → 「支持指令：\n重置密码 用户名 —— 获取密码重置码」★无条件兜底
  ▼
send_c2c_reply（msg_id + 引用）
```

### 「私聊回复你好、群聊不回复」的完整解释

| 场景 | 你发「你好」 | 代码原因 |
|---|---|---|
| 群聊 | **静默** | `handle_group_message` 无规则命中即静默（第三节「无命中」分支），无兜底文案 |
| 私聊 | 回「支持指令：…」 | `handle_c2c_message` 的 else 分支**无条件兜底**（第四节），且**不走规则引擎** |

即：两条路径的架构不一致——群聊 = 规则引擎驱动（v16 重构后的形态），单聊 = 早期硬编码指令（从未迁移到规则引擎，预设规则、条件、模板在单聊全部不生效，重置密码的成功文案是另一份写死的副本）。

---

## 五、播报链（kind = broadcast）

```
模块上传圈速 POST /v1/laps → 事务提交后（事务外，失败不影响上传）：
  server_best 为真（刷新 alltime 或 version 纪录）
    → broadcast("record_refresh", {track, lap, version, paddock_name, paddock_id})
       ├─ 勾选群集为空 → 不发
       ├─ 遍历 broadcast 规则：enabled 且事件条件命中
       └─ 每条命中规则 × 每个勾选群 → send_direct_group 主动裸发
```

---

## 六、设置页呈现 vs 真实逻辑：差距清单（v2 修订）

| # | 问题 | 代码事实 | 差距定性 |
|---|---|---|---|
| 1 | 「条件是敷衍的表面功夫」 | 条件引擎真实，但内置动作的真 gate 是提取结果；面板上触发词与条件行重复且条件行对动作语义零贡献 | 呈现冗余：动作规则隐藏条件编辑器 |
| 2 | 「失败模板空的，实际又不是空的」 | `fail_template=""` 时运行时用 5 条内置文案（识别失败写死绕过 fail_template + 码无效/在途/已绑定/用户名不存在 4 条经 fail_reply）。空输入框背后实际生效的文案面板不可见 | 呈现缺失：预填内置文案 |
| 3 | 「快捷变量里没有 reason」 | {{reason}} 仅 fail_reply 专用 replace；芯片行只有成功模板一排 | 呈现缺失：失败模板独立芯片行 |
| 4 | 「没看到真正的判断链」 | 判断链在代码里（第三节表格），面板零提示 | 呈现缺失：执行链摘要 |
| 5 | **「私聊回复、群聊不回复」** | 单聊硬编码无条件兜底 + 硬编码重置流程；群聊规则引擎无命中即静默。**两条路径架构不一致，单聊从未接入规则引擎** | **架构不一致**：单聊路径需迁移/对齐（见方案 D） |
| 6 | 触发词与条件重复展示 | 同 #1 | 同 #1 |

---

## 七、修改方案（待拍板）

### A. 数据模型扩展（每类失败独立模板，预填内置默认）

```
BotRule 新增：
  no_code_template:      String  // reg_code：锚点后提不出 #码（当前不单独拦截，新增此出口）
  invalid_code_template: String  // reg_code：码无效/过期（含并发 4a）
  dup_openid_template:   String  // reg_code：已有在途会话 / 已绑定（3 与 4b 合并语义）
  no_identity_template:  String  // reg_code：平台未给群身份（现在写死的那条入库）
  no_user_template:      String  // reset_password：用户名不存在
```

- 预设规则预填全部内置文案 → 面板所见即所得。
- 运行时兜底：字段空 → 用内置默认（老数据兼容）。
- {{reason}} / {{code}} 等变量在对应模板内可用。

### B. UI 分组编辑器

动作规则弹窗按语义分组：触发词（锚点说明）→ 成功模板+芯片 → 失败区（每类独立输入框，预填，独立芯片含 reason）→ 执行链摘要（3 行静态说明）。隐藏动作规则的冗余条件编辑器；普通回复/播报保留完整条件组（且/或）。

### C. 执行链不数据化（边界定案）

建号事务、openid 唯一约束、码过期是安全边界，保持代码实现；配置管文案与触发词，代码管动作与防重。

### D. 单聊路径对齐（新增，来自 v2 发现）

- 将 `handle_c2c_message` 迁移到同一规则引擎：按规则匹配 + action 分发，消除硬编码副本。
- 单聊特例（注册校验必须在群内完成）作为 reg_code 动作的 scene 判断保留在代码里（安全语义，不入配置）。
- 是否保留单聊「支持指令」兜底回复，由你定：保留 = 私聊有任何消息都回；去掉 = 与群聊一致（无命中静默）。

### 改动量

A+B+D 涉及 `qq_bot.rs`（BotRule + 两个 handler 统一）、`admin.rs`（校验）、`admin_settings.html`（分组编辑器），约 ±400 行，与 v16 同级或略大。