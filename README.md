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
- **Web 管理端 v2**（2026-09-03）：GET 页面只渲染骨架，所有写操作走 `/admin/api/*` JSON 端点 + 前端页内弹窗（自绘 modal + fetch + 局部刷新），无 URL 路径后缀式动作。品牌名/Logo（configs 表 `site_title`/`site_logo` data URL）作用于导航栏/登录页/浏览器标签页 title+favicon。消息规则引擎（configs 表 `bot_message_rules` JSON）：规则=类型（reply/broadcast）+action（reply/reg_code/reset_password）+触发词/条件（且或组合）+成功模板+每类失败独立文案，预设 4 条完整可编辑；播报事件 `record_alltime`/`record_version` 分立（历史优先），模块上传与管理端增删改成绩统一经 `broadcast_lap_change` 触发。播报目标群（`bot_broadcast_groups`）由 webhook 收到群消息自动登记（`bot_known_groups`）+ 群名缓存（`/v2/groups/{id}/info`，11253 白名单限制时回退 openid）。用户/成绩页分页（30/50/100）+ 按用户名搜索。combobox（自绘，fixed 定位防弹窗裁剪）用于用户/赛道选择，文字选择原生放行。JS 公共脚本块必须位于 `<main>`（content）**之前**——页内脚本顶层依赖 toast/varTag 等公共函数，顺序颠倒=整段脚本 ReferenceError 中断；模板 JS 改动用 jsdom 验证渲染产物。
- 迁移注意：`sqlx::migrate!` 编译期嵌入——改/增迁移文件后必须 `touch src/main.rs` 触发重编译，否则旧二进制跑旧迁移。0005 回填 best_laps 缺行（recalc_dims 已改 upsert，管理端补录成绩的维度不再静默丢行）。

## License

Apache-2.0