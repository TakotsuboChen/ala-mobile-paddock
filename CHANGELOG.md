# Changelog

本文件记录 paddock 服务端的所有显著变更。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/2.0.0/)，版本遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## 版本约定

- 服务端语义化版本 = `Cargo.toml` 的 `workspace.package.version`，编译期经 `env!("CARGO_PKG_VERSION")` 注入二进制，`GET /v1/health` 可核。
- 每次发布打 git tag `v<semver>`，Docker 镜像 tag `paddock-api:<semver>`。
- 早期开发（2026-08-31 ~ 09-04）以「服务端 v1~v39」部署序号口头记录进度，**1.0.0 ≈ 线上 v39 水位**。历史文档中残留的 vNN 记法均指当时的部署批次或方案定案序号，与语义化版本无映射关系。

## [Unreleased]

### Fixed
- 积分公式 off-by-one：v39 实现为 `round((N−rank)×100/N)`，与定案例句「2 人 → 100/50/虚位0」矛盾（2 人榜给出 50/0，全体偏低 100/N）。修正为 `round((N+1−rank)×100/N)`（第一名 100、每名次递减 100/N、虚位 0、N=1 自然 100）。契约源 PADDOCK_PLAN.md 同步为 v40 定案。已入库历史成绩的展示积分随重算自动修正（best_laps 明细不动）

## [1.0.0] - 2026-09-04

服务端首个语义化版本发布，功能水位 = 部署批次 v39（8.0.6 全面适配）。

### Added
- 模块 API（`/v1`）：通行证注册（申请即设密，bot 校验幂等恢复）、登录（Argon2id + 90 天滑动 token）、按码重置密码、`/v1/me`（个人资料 + 计时赛总积分）、头像上传/查询（手写 S3 SigV4，404 → 302 客户端占位图降级）
- 计时赛排行榜：总榜（版本=独立赛季，各版本独立计分后累加）+ 赛道榜，积分公式 `round((N−rank)×100/N)`（第一名 100、每名次递减 100/N、N=1 给 100），SQL CTE 实现
- 圈速上报 `/v1/laps`：事务内写 laps 留档 → upsert best_laps → Toast 四条件判定 → 更新 records
- QQ 官方开放平台 webhook bot：Ed25519 验签 + op=13 回调验证、消息规则引擎（JSON 规则 + AND/OR 条件 + 每类失败独立文案，用户数据永不回落预设）、注册/重置流、破纪录主动播报（总榜/版本榜分立）、播报群自动登记 + 群名三层优先级、发送队列串行 + 被动窗频控
- Web 管理端（askama SSR）：登录/用户/成绩/日志/设置五页 + 16 个 JSON 端点，页内写操作全 fetch 化；成绩四筛选 + 输入跳页 + 补录可选游戏版本；双日志视图（业务事件筛选 + 运行日志终端风游标轮询）
- 完整日志系统：业务事件 app_logs 表（90 天保留，fire-and-forget 与事务内双路径）+ 运行日志内存环形缓冲 2000 行（tracing MakeWriter 双写）
- 双日志之外的三容器部署：app + postgres 17 + garage (S3)，musl 静态单二进制（opt-level=z + LTO），2GB 内存 VPS 友好

### Changed
- 8.0.6 (200150) 全面适配：`version_display` 映射、管理端补录版本下拉、排行榜版本筛选与版本键
- 成绩页筛选改造：赛道 + 版本 + 空串 `empty_str_as_none` 修复 400
- 规则防覆盖：`load_rules` key 存在即用户数据永不回落预设；`save_rules` 覆盖审计记改前后 diff
- 注册流 v3：幂等恢复 + 建号时发车手号（最小空缺分配）
- bot 指令防误触发：重置口令严格匹配、裸关键词静默

## [0.x] — 2026-08-31 ~ 09-04（部署批次 v1 ~ v38）

早期快速迭代期，未采用语义化版本。关键里程碑（按部署批次）：
- v1：三容器上线（app/postgres/garage，openresty 反代 443）
- v2：管理端 SSR 首版（登录/代绑 openid/用户列表/成绩删除重算）
- v13~v28：管理端 v2 重做（fetch JSON 化）、消息规则引擎、双日志系统、头像后端（手写 SigV4）、`GET /v1/me`
- v29~v31：运行日志终端风视图（环形缓冲 + 游标轮询）
- v33~v35：注册流 v3、车手号最小空缺分配、成绩页三组筛选、bot 指令防误触发
- v36~v39：8.0.6 适配、成绩页四筛选 + 跳页、积分 v39 线性递减 + 版本独立赛季、规则防覆盖
