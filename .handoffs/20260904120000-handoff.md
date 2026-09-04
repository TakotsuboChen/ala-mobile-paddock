# HANDOFF — 读全文再开始干活

生成时间: 2026-09-04T01:46:00+08:00 · Git HEAD: `8bec151`（paddock 仓；模块仓 `f3ad5bf` 本会话零改动）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock 仓 `main` @ `8bec151`（2026-09-04）；模块仓 `main` @ `f3ad5bf`
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `8bec151`——HEAD 必是本次 handoff 提交，其 parent 才是文档记录的 SHA；paddock 仓对 `8bec151`。不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记（重点是运行日志端到端网页验证）
- 先读: `README.md`（日志系统两条目）+ 模块仓 `docs/PADDOCK_PLAN.md`（契约源，本会话未变更）

## 1. 当前目标
**围场服务端完整日志系统**：管理端新开"日志"页，双视图——①运行日志（终端风黑框框，全量 raw tracing 输出含 openid/payload 原文）②业务事件（结构化表格，`app_logs` 表）。已完成定义：v31 线上运行，两个视图的数据链路全部部署；端到端网页验证待用户。

## 2. 已验证状态 — 工作实际停在哪
- [V] 线上 **v31** 运行：`sudo docker logs paddock-app-1` → `listening on 0.0.0.0:8080`（纯文本无 ANSI）；`/admin` 200、`/v1/health` 200；compose 已指向 `paddock-api:v31`
- [V] 生产库迁移版本 = **6**（0006 `app_logs` 建表 + `admin_audit` 30 行回填翻译 + DROP 旧表）
- [V] 五个工作切片已推送：日志系统 `01d80af` → 运行日志视图 `94624e0` → ANSI 关闭 `9c15744` → 斑马纹 `ab824f3`+`1ffc4a5` → README `8bec151`
- [V] jsdom 门禁 8 断言全过 EXIT=0（tab 切换/轮询/着色/分页/openid 原文可见）
- [?] **运行日志网页端到端未验证**：`/admin/api/runtime-logs` 部署在管理端鉴权域内，curl 验证时因管理端密码与 `.env` 不符拿到 401（见 §5）——需用户登录网页确认黑框框视图实时滚动
- 模块仓本会话零改动、零构建

### 测试/build 输出（真实退出码）
```
cargo check EXIT=0（2 warnings 均既有遗留）
node verify-logs-v2.mjs EXIT=0（8 项 OK）
docker build v31 → DONE；线上 /admin 200
```

## 3. 决策与理由
- **业务事件日志 ≠ 运行日志，双轨并存** [V]：业界共识（审计与应用日志分离）。业务事件=「谁做了什么」→ `app_logs` 表（追加写/结构化列/90 天保留）；运行日志=调试 raw 输出 → 内存环形缓冲（2000 行，跨重启清零，stdout 才是持久面）。
- **两条写入路径** [V]：`applog::log_event`（tokio::spawn fire-and-forget，业务事件）与 `log_event_tx`（事务内，管理端敏感操作同生共死）。
- **圈速上传降采样** [V]：普通圈不入日志，只有破纪录（toast 非空）才记——205 人每圈写库会把 90 天保留策略打穿。
- **运行日志捕获用 MakeWriterExt::and() 组合** [V]：`stdout.and(RuntimeLogWriter)` 让 fmt 层渲染好的同一条文本双写，零埋点改动即全量（tower_http/payload/堆栈都在）。
- **游标增量轮询而非 SSE** [V]：单管理员低频场景轮询（`?after=seq`）最简且无 openresty 反代缓冲 SSE 流的坑。
- **斑马纹=文字颜色按全局 seq 奇偶** [V]：批内索引会跨轮询批次闪变；全局 seq 让每行颜色终身固定。
- 否决方案：挂卷收文件/接日志栈（对轻量 VPS 过重）；tracing→DB 全量转发（混入噪音且违背日志分离）。

## 4. 失败的尝试 — 不要再试
> 全部前向搬运，永不丢弃。本轮新增死路 [V]/[X] 已亲证；继承死路见 `.handoffs/20260904014000-handoff.md` §4。

### 本轮新增
- SSH 三连错 → 正确姿势已入持久记忆 `paddock-vps-ssh-port-4142` [V]：**`ssh -i ~/.ssh_paddock/id_rsa -p 4142 takotsubo@8.134.50.222`**（root 拒绝[两者皆试过] / 端口 22 kex 阶段拒 / 默认 `~/.ssh/id_rsa` 不在 authorized_keys / docker 需 sudo / compose 是 `~/paddock/docker-compose.vps.yml`）
- `RingWriter` 透传 stderr → 双重输出 [X]——`and()` 组合下 fmt 对每个 writer 各写一份，第二路只入环不透传
- 级别着色匹配 `[ERROR]` 带方括号 → 永不命中 [X]——tracing fmt 输出无方括号，要 `\bERROR\b` 按词匹配
- jsdom mock fetch 在 JSDOM 构造后注入 → 内联脚本 `runStart()` 先执行抛 ReferenceError [X]——必须 `beforeParse` 钩子（与"公共 JS 必须在 main 之前"同构）
- 用 `.env` 的 `PADDOCK_ADMIN_PASS` 登录管理端 → 401 [V]——用户改过密码；该 env 只在 admins 表空时播种（见 §5）

### 继承死路（全部 [X] 前向有效，详见 .handoffs/20260904014000-handoff.md §4）
- fetchMe 无鉴权 getJson 漏 Authorization → 401 自动登出 / Crossfade key 绑筛选状态 / navigationIcon 只画不绑事件 / OverlaySpinnerPreference 迁移丢参数 / clickable 包名是 foundation。
- 两进程共用代码引 AlaMobileModule.logX → NoClassDefFoundError 伪装"网络错误" / externalFilesDir 跨进程不可见（token 走 daemon）/ CropImageActivity 主主题闪退 / 改代码只跑 assembleDebug 装旧 release / sqlx HRTB / axum 0.8 `:param` panic / VPS cargo build OOM（本地 build+scp）。
- lap_hook：圈完成等 2→0 回绕 / sectorOrder 1-based / trackToRace 当赛道名 / 拦截器浮点操作（FPSIMD 污染）/ proxy_shift 日志洪水 / IL2CPP dlopen / 后台线程调 Unity API。
- QQ bot：bearer_auth 调 QQ API / 被动回复用 event id 非 d.id / member_openid 在 d.author / "平台不推全量消息"实为手机QQ群内授权未开。
- 排行榜（模块仓）：圈速钳位保底 / Card forEach 渲染 205 行 / 手动 Animatable snapTo / 换源直接读筛选状态 / delay 排在伸缩后 / 行级 items 丢尾部留白 / SQL 直插缺 `::uuid` / adb 端口漂移。

## 5. 已知坑
- ⚠️ **`PADDOCK_ADMIN_PASS` 环境变量与实际管理端密码不符** [V]（本轮发现）——不影响功能（seed 只在表空时生效），但**清库重建会导致管理员密码变回 .env 旧值**；别用 .env 里的密码做线上验证。
- ⚠️ **sqlx::migrate! 编译期嵌入** [V]——改/增迁移文件后必须 `touch src/main.rs` 触发重编译，否则旧二进制跑旧迁移（README 已记）。
- ⚠️ **Garage 206 个测试/遗留头像对象** [?]（继承）——无害；无 shell/aws cli 清不掉。
- ⚠️ **模块侧 Toast 文案"蒙扎国家赛车场 的"多空格** [?]（继承）——模块侧 parseToast 模板 `$track 的`；纯文案瑕疵，用户未点名修。
- ⚠️ **双仓赛道中文名两份硬编码** [?]（继承）——契约源 PADDOCK_PLAN §5（`Shangai` 单 a）；改必须两边同改。
- ⚠️ **排行榜无实时刷新** [?]（继承，用户拍板暂不做）。
- ~~群全量消息根因~~ [V] 已解决（手机QQ群内"获取群内全部消息"开关，记忆 `qq-bot-group-full-message-config`）——不再搬运为坑。

## 6. 下一步（有序）
1. **用户网页验证日志页**（我无法登录，见 §2 [?]）：登录 `/admin/logs` → 运行日志 tab 应看到启动以来日志实时滚动（2s 轮询）→ 群内发消息 @bot，payload 原文（含 openid）应 2s 内出现；业务事件 tab 应有 30 条回填审计 + 新事件。
2. **群内 bot 新文案复测**（继承，paddock HANDOFF §6.1）：@bot 发申请码 / 不带码 / 私聊静默 / 播报实测——现在可对照日志页两个 tab 闭环验证。
3. **重构**（用户定案"下个版本重构模块和服务端的一堆东西"）——范围待用户指定，可能涉及模块仓。
4. （可选继承）排行榜实时刷新（暂不做）/ Toast 文案空格 / Garage 头像清理。

## 7. 留给用户的开放问题
- 播报受主动消息额度限制（未认证约 4 条/月/群）：接受 / 申请认证？
- 单聊"支持指令"兜底已移除——是否需要私聊帮助指令？
- 计时赛积分卡"暂无"与"加载失败"是否区分显示？
- 重构范围清单待定。
