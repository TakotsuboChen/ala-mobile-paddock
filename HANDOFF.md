# HANDOFF — 读全文再开始干活

生成时间: 2026-09-03T08:05:00+08:00 · Git HEAD: `9ef6fb3`（paddock 仓；模块仓本会话无改动，仍在 `aeb5944`）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock 仓 `main` @ `9ef6fb3`（2026-09-03，本 handoff 提交的 parent 是工作切片 `8a8c580` + README `9ef6fb3`）；模块仓 `main` @ `aeb5944` 未动
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `9ef6fb3`；不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记
- 先读: `docs/BOT_RULES_ANALYSIS.md`（消息规则引擎现状与设计边界）+ `../ala-mobile-tool/docs/PADDOCK_PLAN.md`（契约源）

## 1. 当前目标
**Web 管理端 v2 全面重做 + QQ bot 消息规则引擎**。本会话（跨 v13–v28 共 16 次部署）：管理端从表单 POST 重定向改为页内弹窗 fetch JSON 化 → 品牌名/Logo（含标签页 favicon）→ 消息规则引擎（条件与或 + action 分发 + 失败文案全可编辑）→ 播报事件分立（alltime/version）→ 引用回复 → 分页/搜索/combobox 多轮交互打磨 → best_laps 缺行存量 bug 修复（0005 迁移）。完成定义已达成：用户全部 16 条反馈落地，线上 v28 运行。

## 2. 已验证状态 — 工作实际停在哪
- [V] **paddock 仓工作已提交推送**：`8a8c580`（10 文件 +2577/−589）+ README `9ef6fb3`；工作区干净
- [V] **线上 v28 运行中**（paddock.takotsubo.cloud，镜像本地 build→scp→load，compose 已指向 v28）：`/admin` 200、`/v1/leaderboard/points` 200
- [V] 生产库 0005 迁移已执行：`_sqlx_migrations` 最大版本 = 5；Takotsubo 巴林圈 `gp_index=2, lap_ms=85010` 在 best_laps 有行（用户确认模块端可查）
- [V] `cargo check` EXIT=0（2 warnings：laps.rs server_best 赋值顺序 + 1 个旧残留）
- [V] 模板 JS 用 jsdom 真实执行验证（三页面无 ReferenceError、规则列表 4 卡渲染、combobox 交互矩阵 6 场景全过）
- [V] 模块仓（ala-mobile-tool）本会话零改动，HEAD 仍 `aeb5944`

### 测试/build 输出（真实退出码）
```
cargo check → EXIT=0（dev profile）
docker build -t paddock-api:v28 . → DONE
线上 https://paddock.takotsubo.cloud/admin → 200；/v1/leaderboard/points → 200
生产库迁移版本 → 5；best_laps 左联 laps 完整性 → 10/10 行匹配
```

## 3. 决策与理由
- **管理端 v2 架构：GET 页面渲染骨架 + `/admin/api/*` JSON + fetch + DOM 局部替换**——SPA 手感零框架；refreshPage() 重拉页面替换 `<main>`。
- **规则引擎"配置管变的、代码管不变的"**：触发词/条件/全部文案入 configs JSON（可编辑）；建号事务/防重/过期/发码是安全边界留在代码（action 分发）。执行链不数据化（否决：编排能力开放到面板复杂度爆炸且无需求）。
- **内置动作（reg_code/reset_password）触发词即条件**，无可编辑条件行——面板再显条件是冗余；rule_matches 对动作规则特判"消息含触发词"，空 conditions 不再恒命中（否则每条群消息都会触发建号流程）。
- **播报事件分立 record_alltime / record_version**（原单一 record_refresh）——"历史优先于版本"在事件产生侧裁决（同破只发 alltime），规则引擎只消费；触发源统一走 `broadcast_lap_change`（模块上传 + 管理端加/改/删成绩四挂点）。
- **删除成绩"变了才播报"**：recalc_delete 返回删前两维度纪录快照，删后比对，值与持有者都没变就静默（修"删非最快圈重复播报"）。
- **{{reason}} 废除**：文案必须完全可编辑，输入框里见到的就是发出去的；动态部分用显式 {{code}}/{{name}}。失败文案每类独立字段（no_code/invalid_code/dup_openid/no_identity/no_user），预填完整实文。
- **C2C 单聊接入同一规则引擎**（原三段硬编码全废）：未命中规则=静默（与群聊一致，原"支持指令"兜底按此原则移除）；reg_code 在单聊强制引导回群（无 member_openid 无法建号，安全语义留代码）。
- **回复一律引用形式**：SendJob.ref_msg_id（`message_scene.ext` 的 `msg_idx=REFIDX_`）→ `message_reference`；@ 前缀从默认模板全删（引用自带上下文）。
- **变量芯片=复制到剪贴板**（非插入输入框）——多 textarea 场景插入目标二义；qq_name（@QQ 群用户名）变量废除（芯片与后端校验一起清，运行时 ReplyVars 保留供预设渲染）。
- **combobox 文字选择完全放行**：mousedown 永不 preventDefault，下拉"只开不关"（blur/Escape 才收）——toggle 与拖选在 mousedown 层物理互斥，裁决=文字操作无条件优先。
- 继承：注册流 v2 / token daemon / 全放行防伪 / order==2 挂圈 / 拦截器零浮点 / miuix preference 全盘照搬。

## 4. 失败的尝试 — 不要再试
> 全部前向搬运，永不丢弃。完整历史见 `.handoffs/`（模块仓侧）。

### 本轮新增（管理端 v2 + 规则引擎迭代）
- [X] **query_scalar 加 `.map(|(u,):(String,)| u)` 元组标注** → sqlx 解码反推成元组、运行时静默失败 → 添加成绩用户下拉空白。修复：去掉标注留给标量上下文推断 + 注释防回归。
- [X] **askama 模板编辑残留旧块**（`userEdit` 函数体后跟孤儿 `<script>`、`dragSelecting` 重复 let、makeCombo 括号失衡）→ 页内脚本整段语法错误，所有按钮失效/规则列表空白。修复：`node --check` 渲染产物脚本作为每次模板改动的**必跑门禁**。
- [X] **公共 JS 块放 `<main>` 之后** → 页内脚本顶层调 varTag 抛 ReferenceError → 设置页规则列表空白（v25"一条规则都没了"事故）。修复：公共脚本移到 content 之前；jsdom 验证发现。
- [X] **grep 关键词当冒烟**（preset-reg、ruleAdd 等静态字符串）→ JS 执行失败时 grep 照样全绿。修复：jsdom runScripts:'dangerously' 验证渲染产物（rule-list 子元素数）。
- [X] **recalc_dims 的 best_laps 重算用 UPDATE-only** → 管理端补录新维度成绩时行不存在、静默 0 行 → laps 有圈而积分榜/赛道榜无成绩（用户："手动添加的巴林圈看不到"）。修复：改 upsert + 0005 迁移回填存量。
- [X] **`sqlx::migrate!` 增迁移文件后 touch 迁移文件本身** → 宏是编译期嵌入，增量编译不重跑 → 本地库迁移没执行。修复：touch src/main.rs 触发重编译（Docker 全新构建天然正确）。
- [X] **删非最快圈按"records 有行就播报"** → 删除后纪录没变也播（用户重复收到上赛播报）。修复：删前快照+删后比对，变化才播。
- [X] **combobox mousedown 无条件 preventDefault** → 输入框无法长按拖选文字（用户只能逐字删）。修复：文字选择放行+开合与选择解耦（mousedown 只开不关）。
- [X] **combobox toggle 开合（focus/click 对撞 + preventDefault）** → 首次点击闪现、已聚焦点击行为混乱（三轮迭代失败：justFocused 标记、click toggle、focusedSelf 均不完全）。修复：放弃 toggle，"只开不关"。
- [X] **combo-list 用 position:absolute** → 被弹窗 overflow:auto 裁剪（用户："下拉框被框死在弹窗里"）。修复：position:fixed + getBoundingClientRect 定位 + 滚动重定位（非收起）。
- [X] **scroll 捕获监听无条件收起下拉** → 下拉列表自身滚动也触发收起（用户："一滚动就收回去"）。修复：`list.contains(e.target)` 排除 → 后又改为"滚动重定位"策略。
- [X] **变量芯片点击插入输入框**（getElementById 全局找 m-template）→ 永远填到成功模板开头（用户点名改复制）。修复：clipboard.writeText + 降级 execCommand。
- [X] **预设播报模板 `{{track}}` 两侧留空格** → 中文赛道名前后带空格（用户两次点名）。修复：模板串直接去空格（第一次只改了芯片侧没改模板串，教训：改模板要 grep 全部出现点）。

### 继承死路（全部 [X] 前向有效，详见模块仓 .handoffs/20260903080000-handoff-module-repo.md §4）
- fetchMe 无鉴权 getJson 漏 Authorization / Crossfade key 绑筛选条件 / navigationIcon 只画不绑事件 / OverlaySpinnerPreference 迁移丢参数 / clickable 包名是 foundation / adb 端口漂移（mdns 重扫）。
- 两进程共用代码引 AlaMobileModule.logX → NoClassDefFoundError / externalFilesDir 跨进程不可见（token 走 daemon）/ CropImageActivity 主主题闪退 / 改代码只跑 assembleDebug 装旧 release / sqlx HRTB / axum 0.8 `:param` panic / VPS cargo build OOM（本地 build+scp）。
- lap_hook：order 2→0 回绕 / sectorOrder 1-based / 拦截器浮点操作 / proxy_shift 日志洪水 / IL2CPP dlopen / 后台线程调 Unity API。
- QQ bot：bearer_auth 调 QQ API / 被动回复用 event id 非 d.id / member_openid 在 d.author / **"平台不推全量消息"实为手机QQ群内授权未开（AstrBot PR 8838 指路）**。

## 5. 已知坑
- ⚠️ **主动消息额度极低** [V]——未认证应用约 4 条/月/群，播报可能发不出（失败只记日志不重试）；用户需在群内开"机器人主动发言"。
- ⚠️ **群名拉取可能 11253** [V]——`/v2/groups/{id}/info` 仅白名单机器人；失败回退显示 openid，日志可见。需开放平台申请权限。
- ⚠️ **msg_seq 恒为 1** [?]——同一 msg_id 被动回复第 2 次起会因 msg_id+msg_seq 去重失败；当前单回复场景未触发，做多轮回复时需递增。
- ⚠️ **Garage 206 个测试/遗留头像对象** [V]（继承）——无害（按 user_id 命名不复用），无 shell/aws cli 清不掉。
- ⚠️ **Toast 文案"蒙扎国家赛车场 的"多空格** [V]（继承）——模块侧 parseToast 模板，纯文案瑕疵。
- ⚠️ **lint baseline 13 条失效** [V]（继承）——模块仓下次重新生成。
- ⚠️ 继承：双仓赛道中文名两份硬编码（PADDOCK_PLAN §5，`Shangai` 单 a）；多指日志回传、16 赛道 12/16 未实机、门禁 champ==NULL 多语义。

## 6. 下一步（有序）
1. **群内全流程复测**：@bot 发 `申请围场通行证#码` → 预期引用回复"校验成功，欢迎 xxx 加入…第 x 位车手"；发不带码的"申请围场通行证" → 预期"未识别到校验码…"；私聊 bot 任意消息 → 预期走规则引擎（未命中静默）。
2. **播报实测**：管理端编辑/添加一条会刷新纪录的成绩 → 群里应收到播报（需播报目标群已勾选 + 主动消息额度允许）。
3. **设置页验收**：4 条预设规则完整呈现（条件/触发词/成功失败文案全可编辑）；变量芯片点击=复制。
4. **重构**（用户定案"下个版本重构模块和服务端的一堆东西"）——范围待用户指定。
5. （可选清理）Garage 测试头像、模块仓 lint baseline 重生成、Toast 文案空格。

## 7. 留给用户的开放问题
- 单聊"支持指令"兜底已按"未命中静默"移除——是否需要私聊帮助指令？
- 播报若因主动消息额度发不出：接受限制 / 申请平台认证？
- 大奖赛/娱乐匹配占位卡真做时：赛季积分/胜场数据从哪来（服务端目前只有计时赛成绩）？
- 重构范围：模块和服务端"一堆东西"具体清单待定。
