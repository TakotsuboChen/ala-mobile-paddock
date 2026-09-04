# HANDOFF — 读全文再开始干活

生成时间: 2026-09-04T23:56:42+08:00 · Git HEAD: `0ba46f2`（模块仓 `13fa6f8`）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock `main` @ `0ba46f2`（2026-09-04，tag `v1.0.0`）；模块仓 `main` @ `13fa6f8`（tag `v1.0.3`）
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `0ba46f2`——HEAD 必是本次 handoff 提交，其 parent 才是文档记录的 SHA；不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记
- 先读: 模块仓 `docs/PADDOCK_PLAN.md`（契约源）+ 本仓 `CHANGELOG.md`「版本约定」

## 1. 当前目标
**服务端 SemVer 1.0.0 收敛 + 重新部署**：从部署序号 v1~v39 收敛到语义化版本（Cargo.toml 单一事实源），CHANGELOG 回溯历史，重新构建部署上线。全部完成并验证。

## 2. 已验证状态 — 工作实际停在哪
- [V] 版本体系收敛：Cargo.toml version 0.1.0→**1.0.0**；`/v1/health` 从 `"ok"` 改返回 `{"status":"ok","version":env!("CARGO_PKG_VERSION")}`（auth_handlers.rs，模块端 grep 确认零依赖此端点）；deploy/docker-compose.vps.yml 镜像行 v1 死值→`paddock-api:1.0.0`
- [V] CHANGELOG.md 新建（Keep a Changelog）：1.0.0 条目回溯 v1~v39 里程碑 +「版本约定」节（服务端版本=Cargo.toml，git tag/镜像tag三处同源，1.0.0≈线上v39）；LICENSE 补 Apache-2.0 本体（Cargo.toml 声明了但文件缺失）
- [V] README 重写（中等完整版 9 节）：徽章/功能六点/架构图（单二进制+三容器+openresty反代）/Quick Start/环境变量表/部署与发版（版本规则）/运维速记 6 条（原~3600字"定案速记"压缩外移 PADDOCK_PLAN）
- [V] 重新部署上线：docker build `paddock-api:1.0.0`→gzip 5.9MB→scp（takotsubo@8.134.50.222 -p 4142 -i ~/.ssh_paddock/id_rsa）→VPS load→线上 compose 镜像行 v39→1.0.0→up
- [V] 线上验证：`curl /v1/health` → `{"status":"ok","version":"1.0.0"}`；`/v1/leaderboard/points`（Takotsubo 700 分）与 `/v1/leaderboard/track/5?version=200150`（摩纳哥）JSON 正常
- [V] 提交推送：`62d0b94`（chore(release): 服务端 v1.0.0，6 文件）→ `0ba46f2`（docs: README 重写）→ tag `v1.0.0` 已 push；工作区 clean
- [V] cargo check（全新 shell）→ EXIT=0（2 既有 warning）
- 工作区: clean，全部已推送

### 测试/build 输出（真实退出码）
```
cargo check → EXIT=0（2 warning：server_best overwritten 等既有）
docker build -t paddock-api:1.0.0 . → DONE
curl /v1/health → {"status":"ok","version":"1.0.0"}
```

## 3. 决策与理由
- **版本=SemVer 三处同源**（用户定案）[V]：vN 是人肉部署批次混用三义（部署批次/方案定案序号/镜像tag）。收敛：Cargo.toml（编译期 env! 注入 health）+ git tag `vX.Y.Z` + 镜像 tag `paddock-api:X.Y.Z`。映射起点 **1.0.0 ≈ 线上 v39**。历史文档 vNN 记法保留不改（指方案定案），CHANGELOG「版本约定」有说明。
- **health 改 JSON** [V]：加 version 字段让线上可核版本，治"线上跑的哪版靠翻 docker logs"。改前确认模块端不消费该端点，零破坏。
- **README 重写取舍** [V]：契约源禁令/bot 鉴权事实速记/迁移 touch main.rs 坑——原 README 高价值资产全保留；"定案速记"压缩为运维速记 6 条，决策全文外移 PADDOCK_PLAN（point, don't embed）。
- **migrations 0001~0007 零改动** [V]：4 位序号_描述命名与版本体系无耦合，sqlx migrate 标准形态。
- 继承：积分 v39 线性递减 / 版本独立赛季 / 规则防覆盖 / serde Option 不兜空串 / askama 模板禁调函数。

## 4. 失败的尝试 — 不要再试
- **compose 镜像行以仓内状态为准做 sed** [V]：仓内死值 v1、线上实际 v39——部署改 tag 前先 `grep` VPS 上的实际行内容，以实机为准。
- **scp 默认 root 用户** [X]：Permission denied (publickey)——必须 `takotsubo@8.134.50.222 -p 4142 -i ~/.ssh_paddock/id_rsa -o IdentitiesOnly=yes`。
- 继承（前向有效，详见 .handoffs/20260904233000-handoff.md §4）：VPS cargo build OOM（2GB 内存，只能本地 build→save→scp→load）/ serde Option 不兜空串（axum Query 前置反序列化 400）/ askama 模板禁调 Rust 函数（E0433）/ jsdom location.href 真导航 / ssh 4142 / 迁移改动必须 touch src/main.rs / bot 规则禁止回落覆盖。

## 5. 已知坑
- ⚠️ **版本三处同步无自动校验** [?]——Cargo.toml/compose 镜像行/git tag 目前靠人工保持一致；deploy.sh（用户已认可未实施）可加一致性校验。
- ⚠️ **Dockerfile 无版本 ARG** [?]——镜像 tag 是唯一版本标识（health 返回的是 Cargo.toml 版本），镜像本身不可追溯 build 时间；需要时加 `ARG APP_VERSION` + OCI label。
- ⚠️ 继承：管理端网页验证未做 / 群内 bot 复测未做 / Garage 206 测试对象 / 双仓赛道中文名两份硬编码（Shangai 单 a）/ 运行日志缓冲跨重启清零（stdout 才是持久面）。

## 6. 下一步（有序）
1. **用户复验**（继承）：管理端成绩页四筛选+跳页+补录版本；群内 bot 复测（重置口令严格匹配、带码注册车手号）。
2. （可选）`deploy/deploy.sh` 固化部署链 + 版本三处一致性校验（用户已认可提议）。
3. （可选）CI/release 工作流：目前纯手动发版，可考虑 GitHub Actions tag 触发构建（当前 VPS 部署链仍需手动 scp）。

## 7. 留给用户的开放问题
- 官版 8.0.4 用户兼容策略（模块仓问题，影响服务端 version_display 支持范围）。
- 播报主动消息额度（约 4 条/月/群）：接受 / 申请认证？
