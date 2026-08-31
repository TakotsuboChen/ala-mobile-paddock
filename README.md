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
- bot 鉴权（QQ 官方 webhook，逐篇核对 2026-08）：鉴权头 `Authorization: QQBot {token}`（**非 Bearer**）；被动回复 msg_id 用事件 `d.id`（外层 id 是 `事件类型:` 前缀形态）；发送者身份在 `d.author.member_openid`（嵌套），C2C 用 `d.author.user_openid`；群被动窗 5min/5 次、单聊 60min/4 次；token 获取 `POST https://bots.qq.com/app/getAppAccessToken`。

## License

Apache-2.0