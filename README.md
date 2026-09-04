# Ala Mobile Paddock（围场）

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-1.0.0-green)](CHANGELOG.md)
![Rust](https://img.shields.io/badge/Rust-edition%202024-orange)

[F1 手游 Ala Mobile](https://github.com/TakotsuboChen/ala-mobile-tool) 的围场（Paddock）私有服后端：为同名 LSPosed 模块提供**计时赛圈速排行榜**、**账号体系**与 **QQ 群 bot 播报**，并带一个浏览器可用的管理端。

> **契约源**：[ala-mobile-tool/docs/PADDOCK_PLAN.md](https://github.com/TakotsuboChen/ala-mobile-tool/blob/main/docs/PADDOCK_PLAN.md) —— 本仓库与模块仓库共用该计划文档作为唯一契约（API 形状、赛道表、积分公式），任何一侧变更必须两边同步。

## 功能

- **模块 API（`/v1`）** —— 通行证注册（申请即设密，QQ 群校验即建号）、登录（Argon2id，token 90 天滑动）、按码重置密码、个人资料与总积分、头像上传（S3）
- **计时赛排行榜** —— 总榜 + 分赛道榜；积分公式 `round((N+1−rank)×100/N)`（第一名 100、每名次递减 100/N、虚位第 N+1 名 0），每赛道 × 每游戏版本独立计分后累加（版本 = 独立赛季）
- **圈速上报** —— 服务端事务内完成留档、最快圈更新与四条件 Toast 判定，判定权不在客户端
- **QQ bot** —— QQ 官方开放平台 webhook 模式：Ed25519 验签、可编辑的消息规则引擎（触发词/条件 AND/OR/失败文案）、破纪录主动播报、注册与重置流、发送队列频控
- **Web 管理端（`/admin`）** —— 用户/成绩/日志/设置四页：改名、重置密码、成绩增删改与重算、四组可组合筛选 + 输入跳页 + 补录可选游戏版本；业务事件日志与运行日志双视图
- **双日志系统** —— 业务事件 `app_logs` 表（90 天保留、脱敏）+ 运行日志内存环形缓冲（不改埋点捕获全部 tracing 输出）

## 架构

```
                    ┌─ HTTPS 443（宿主机 openresty/1Panel 反代）
                    ▼
  ┌───────── 127.0.0.1:8080 ─────────┐
  │  paddock-api 单二进制（axum）     │
  │  ├─ /v1/*    模块 API            │
  │  ├─ /admin/* Web 管理端（SSR）    │
  │  └─ /qq/*    bot webhook         │──▶ QQ 官方开放平台
  └──────────┬───────────┬───────────┘
             ▼           ▼
      postgres:17    garage (S3)
      用户/成绩/     头像存储
      积分/配置      (仅内网 3900)
```

- **单二进制** `paddock-api`：Rust axum + sqlx（编译期 SQL 校验）+ askama SSR，musl 静态链接（`opt-level="z"` + LTO + strip），2GB 内存 VPS 友好
- **三容器部署**：app + PostgreSQL 17 + Garage（S3 兼容对象存储）；头像 SigV4 为手写实现（~80 行），不引入 AWS SDK
- 数据库 schema 迁移经 `sqlx::migrate!` 编译期嵌入，启动时自动执行

## 快速开始（Docker Compose）

```bash
git clone https://github.com/TakotsuboChen/ala-mobile-paddock.git
cd ala-mobile-paddock
cp .env.example .env        # 填 POSTGRES_PASSWORD + PADDOCK_ADMIN_PASS（管理端初始密码）
docker compose up -d        # 栈内含 Caddy（443 自动 HTTPS）；纯反代环境用 deploy/docker-compose.vps.yml
```

前置要求：Docker + Docker Compose；一个 S3 兼容存储（栈内已带 Garage）；若有外部反代（如 1Panel/openresty），用 VPS 变体并将 443 反代到 `127.0.0.1:8080`。

本机开发（需本地 Postgres）：

```bash
export DATABASE_URL=postgres://paddock:...@localhost/paddock
cargo run -p paddock-api
```

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `DATABASE_URL` | ✅ | Postgres 连接串（compose 内由服务名互连） |
| `BIND_ADDR` | — | 监听地址，默认 `0.0.0.0:8080` |
| `PADDOCK_ADMIN_USER` / `PADDOCK_ADMIN_PASS` | ✅ | 管理端初始账号（仅播种） |
| `GARAGE_KEY_ID` / `GARAGE_SECRET` | ✅ | Garage S3 凭据（头像上传用） |
| `RUST_LOG` | — | tracing 过滤，如 `paddock_api=info,tower_http=warn` |

## 部署与发版

线上部署（VPS，1.8G 内存，**禁止在 VPS 上 cargo build**）：本地构建镜像 → 导出 → 传输 → 载入。

```bash
# 1. 版本号改 Cargo.toml workspace.package.version → git tag vX.Y.Z
# 2. 本地构建（tag 与 Cargo.toml 版本一致）
docker build -t paddock-api:1.0.0 .
# 3. 导出传输（zstd 压缩省流量）
docker save paddock-api:1.0.0 | zstd > paddock.tar.zst
scp paddock.tar.zst user@vps:~/
# 4. VPS 载入并重启（compose 镜像行已钉同一版本）
ssh user@vps 'docker load < ~/paddock.tar.zst && docker compose -f ~/paddock/docker-compose.yml up -d'
# 5. 验证：应返回 {"status":"ok","version":"1.0.0"}
curl https://paddock.example.com/v1/health
```

版本规则：

- 服务端版本 = `Cargo.toml` 的 `workspace.package.version`，编译期注入二进制，`/v1/health` 可核
- 每次发布打 git tag `v<semver>`，Docker 镜像 tag `paddock-api:<semver>` 与 compose 镜像行三处同步
- 变更记录见 [CHANGELOG.md](CHANGELOG.md)（Keep a Changelog 格式）；早期部署批次（服务端 v1~v39）与 1.0.0 的关系见 CHANGELOG「版本约定」

## 运维速记（详见 PADDOCK_PLAN.md）

- **防伪**：全放行（登录态为唯一门槛，管理端事后删），laps 全量留档供审计
- **身份绑定**：QQ `member_openid`（官方 bot 拿不到 QQ 号）；车手 ID = `reg_seq` 建号时分配最小未占用正整数
- **bot 鉴权要点**：`Authorization: QQBot {token}`（非 Bearer）；被动回复用事件 `d.id`；发送者身份在 `d.author.member_openid`（群）/ `user_openid`（单聊）；群全量消息需**手机 QQ 群内**开启「获取群内全部消息」
- **迁移注意**：`sqlx::migrate!` 编译期嵌入——改/增迁移文件后必须 `touch src/main.rs` 触发重编译，否则旧二进制跑旧迁移
- **管理端模板**：页内 JS 公共脚本块必须位于 `<main>` 之前；模板 JS 改动用 jsdom 验证渲染产物
- **规则防覆盖**：`load_rules` key 存在即用户数据永不回落预设；`save_rules` 覆盖审计记改前后 diff

## License

[Apache-2.0](LICENSE)
