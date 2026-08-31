# Ala Mobile Paddock（围场）

Ala Mobile 私服第一步：计时赛圈速排行榜后端。

> **契约源**：`../ala-mobile-tool/docs/PADDOCK_PLAN.md` —— 本仓库与 LSPosed 模块仓库
> 共用该计划文档作为唯一契约（API 形状、赛道表、积分公式），任何一侧变更必须两边同步。
> 开发会话始终开在模块仓库目录，两边同会话配合。

## 架构

单二进制 `paddock-api`（axum）= 模块 API（`/v1/*`） + Web 管理端（`/admin/*`，S1 后半）+ CAMDA bot webhook（`/qq/*`，S4）。

依赖：PostgreSQL（用户/成绩/积分）+ Garage（头像 S3 存储）+ Caddy（HTTPS 反代）。

```bash
cp .env.example .env   # 填好密码等
docker compose up -d   # app + postgres + garage + caddy
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
- 身份绑定：QQ `member_openid`（官方 bot 拿不到 QQ 号）。

## License

Apache-2.0