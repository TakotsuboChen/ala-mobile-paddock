# Ala Mobile Paddock（围场）

Ala Mobile 私服第一步：计时赛圈速排行榜后端。

> **契约源**：`../ala-mobile-tool/docs/PADDOCK_PLAN.md` —— 本仓库与 LSPosed 模块仓库
> 共用该计划文档作为唯一契约（API 形状、赛道表、积分公式），任何一侧变更必须两边同步。
> 开发会话始终开在模块仓库目录，两边同会话配合。

## 架构

单二进制 `paddock-api`（axum）= 模块 API（`/v1/*`） + Web 管理端（`/admin/*`）+ QQ bot webhook（`/qq/webhook`，Ed25519 验签）。

依赖：PostgreSQL（用户/成绩/积分/配置）+ Garage（头像 S3 存储）。HTTPS 反代由宿主机 1Panel openresty 承担（终结 443 → `127.0.0.1:8080`），**栈内不含 Caddy**。

**线上部署**（VPS）：`deploy/docker-compose.vps.yml`（三容器 app/pg/garage，`docker compose -f` 指定）。镜像在本地构建 `docker build -t paddock-api:vN .` 后 `save | zstd | scp | load`（VPS 1.8G 内存不宜 cargo build）。

```bash
cp .env.example .env   # POSTGRES_PASSWORD + PADDOCK_ADMIN_PASS（管理端播种）
docker compose -f deploy/docker-compose.vps.yml up -d
```

开发（本机直跑，需本地 Postgres）：

```bash
export DATABASE_URL=postgres://paddock:...@localhost/paddock
cargo run -p paddock-api
```

## 定案速记（详见 PADDOCK_PLAN.md）

- 防伪：**全放行**（登录态为唯一门槛，管理端事后删）， laps 全量留档供审计。
- 积分：`round(1 + (N−rank)×99/(N−1))`，每赛道×每版本独立，总榜跨版本累加。
- Toast 判定在服务端事务内完成，四条件取最高。
- 身份绑定：QQ `member_openid`（官方 bot 拿不到 QQ 号）。车手 ID = `reg_seq`（注册顺序，`user_reg_seq` 序列从 1 起）。
- 注册流 v2（2026-09-01）：申请即设密（`register-request` 收 username+password，服务端哈希+发号存 pending_regs）→ bot 群校验成功**即建号** → 用户回模块直接登录。`register-verify` 端点已删。
- 头像：手写 SigV4 + Garage S3（bucket `avatars`，凭据走 GARAGE_KEY_ID/GARAGE_SECRET 环境变量）；`POST/GET /v1/me/avatar` + 公开 `GET /v1/avatar/{user_id}`；榜单 entries 带 `avatar_url` 相对路径。
- bot 鉴权（QQ 官方 webhook，逐篇核对 2026-08）：鉴权头 `Authorization: QQBot {token}`（**非 Bearer**）；被动回复 msg_id 用事件 `d.id`（外层 id 是 `事件类型:` 前缀形态）；发送者身份在 `d.author.member_openid`（嵌套），C2C 用 `d.author.user_openid`；群被动窗 5min/5 次、单聊 60min/4 次；token 获取 `POST https://bots.qq.com/app/getAppAccessToken`；群全量消息（GROUP_MESSAGE_CREATE）需**手机QQ群内**机器人设置开启「获取群内全部消息」，开放平台开关不等于群内授权；引用回复用 `message_scene.ext` 的 `msg_idx=REFIDX_`。
- **Web 管理端 v2**（2026-09-03）：GET 页面只渲染骨架，所有写操作走 `/admin/api/*` JSON 端点 + 前端页内弹窗（自绘 modal + fetch + 局部刷新），无 URL 路径后缀式动作。品牌名/Logo（configs 表 `site_title`/`site_logo` data URL）作用于导航栏/登录页/浏览器标签页 title+favicon。消息规则引擎（configs 表 `bot_message_rules` JSON）：规则=类型（reply/broadcast）+action（reply/reg_code/reset_password）+触发词/条件（且或组合）+成功模板+每类失败独立文案，预设 4 条完整可编辑；播报事件 `record_alltime`/`record_version` 分立（历史优先），模块上传与管理端增删改成绩统一经 `broadcast_lap_change` 触发。播报目标群（`bot_broadcast_groups`）由 webhook 收到群消息自动登记（`bot_known_groups`）+ 群名缓存（`/v2/groups/{id}/info`，11253 白名单限制时回退 openid）+ 人工覆盖名（`bot_group_names_custom`，设置页"改名"弹窗，显示优先级 自定义 > API 缓存 > 裸 openid；`POST /admin/api/bot/group-name`，空名=清除覆盖）。用户/成绩页分页（30/50/100）+ 按用户名搜索。combobox（自绘，fixed 定位防弹窗裁剪）用于用户/赛道选择，文字选择原生放行。JS 公共脚本块必须位于 `<main>`（content）**之前**——页内脚本顶层依赖 toast/varTag 等公共函数，顺序颠倒=整段脚本 ReferenceError 中断；模板 JS 改动用 jsdom 验证渲染产物。
- **业务事件日志**（2026-09-04，`app_logs` 表 + 管理端"日志"页）：与 tracing 应用日志分离，只收"谁在何时做了什么"。追加写（只 INSERT），两条路径——`applog::log_event`（fire-and-forget，业务事件）与 `log_event_tx`（事务内，管理端敏感操作与动作同生共死）。覆盖：管理端登录成败/全部敏感操作、模块注册申请/登录成败/密码重置、bot 群消息/建号/重置码/发送成败、破纪录圈（普通圈不入日志防表膨胀，高频事件降采样）。管理端"日志"页支持级别（info/warn/error 徽章）+分类（管理端/认证/成绩/Bot）+关键词筛选与分页；tab 状态入 URL（`?tab=evt`，筛选/分页/刷新都停留在业务事件 tab，服务端渲染初始 active，evt 下不启动运行日志轮询）。脱敏红线：不写密码/token/secret。保留 90 天（启动时 `purge_expired` 清理）。旧 `admin_audit` 表历史行回填后已 DROP（0006 迁移）。
- **运行日志视图**（2026-09-04，`runlog.rs` + 日志页"运行日志"tab）：全量捕获——tracing fmt 层经 `MakeWriterExt::and()` 双写 stdout 与 2000 行内存环形缓冲（`runlog::read_after` 游标增量读取，`GET /admin/api/runtime-logs?after=N`，管理端鉴权域），不改任何埋点即覆盖 tower_http/QQ payload 原文/错误堆栈。fmt 层必须 `.with_ansi(false)`（ANSI 颜色码是 tty 专属，进网页=乱码、进日志聚合=污染）。前端终端风：黑底等宽、WARN 黄/ERROR 红按词匹配着色（tracing fmt 无方括号 token）、斑马纹=**文字颜色**交替按全局 seq 奇偶（跨轮询批次稳定；CSS 声明顺序 rl-even 在 rl-warn/rl-err 之前，保证专属色优先）、2s 游标轮询+自动滚底（可暂停）+清屏+视图 3000 行上限。缓冲跨重启清零（stdout 才是持久面）。
- 迁移注意：`sqlx::migrate!` 编译期嵌入——改/增迁移文件后必须 `touch src/main.rs` 触发重编译，否则旧二进制跑旧迁移。0005 回填 best_laps 缺行（recalc_dims 已改 upsert，管理端补录成绩的维度不再静默丢行）。

## License

Apache-2.0