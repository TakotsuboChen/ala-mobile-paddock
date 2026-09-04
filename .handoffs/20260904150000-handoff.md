# HANDOFF — 读全文再开始干活

生成时间: 2026-09-04T11:43:00+08:00 · Git HEAD: `4c9ea1e`（paddock 仓；模块仓 `f3ad5bf` 本会话零改动）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock 仓 `main` @ `4c9ea1e`（2026-09-04）；模块仓 `main` @ `f3ad5bf`
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `4c9ea1e`——HEAD 必是本次 handoff 提交，其 parent 才是文档记录的 SHA；不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记
- 先读: `README.md`（群名覆盖/日志页 tab 两条目）+ 模块仓 `docs/PADDOCK_PLAN.md`（契约源，未变更）

## 1. 当前目标
**两项用户反馈落地**（用户网页验证日志页后提出）：① 播报目标群 OpenID 支持自定义显示名（11253 白名单拿不到 QQ 群名时无法辨认）② 日志页业务事件点击筛选跳回运行日志（tab 状态未入 URL）。已完成并部署 **v32** 线上。

## 2. 已验证状态 — 工作实际停在哪
- [V] 线上 **v32** 运行：`sudo docker logs paddock-app-1` → `listening on 0.0.0.0:8080`；`/admin` 200、`/v1/health` 200；compose 已指向 `paddock-api:v32`；`/admin/logs?tab=evt` 未登录 303（预期重定向）
- [V] 三个切片已推送：工作 `3f4b0dd`（4 文件 +125/−17）→ README `4c9ea1e` → 本 handoff
- [V] jsdom 门禁 **20/20 过 EXIT=0**（tab 初始态/轮询启停/筛选与分页 URL 带 tab/groupLabel 三层优先级/rename API 全链路/空名删覆盖）
- [V] 模块仓本会话零改动、零构建
- [?] **v32 新功能网页端到端未验证**（管理端密码只有用户知道）：设置页群行"改名"按钮 + 弹窗保存后显示自定义名；日志页点筛选/翻页/刷新应停在业务事件 tab
- [?] **运行日志网页端到端未验证**（继承自 v31，`/admin/api/runtime-logs` 在鉴权域内无法 curl 验证）

### 测试/build 输出（真实退出码）
```
cargo check EXIT=0（2 warnings 均既有遗留：laps.rs unused_assign / qq_bot.rs unused e）
node verify-v32.mjs EXIT=0（20 项 OK，脚本在 /tmp/jsdom-check/，WSL 未重启时可用）
docker build v32 → DONE；线上 /admin 200
```

## 3. 决策与理由
- **群名三层优先级**（自定义 > QQ API 缓存 > 裸 openid）[V]：覆盖层设计，人工兜底不破坏 API 拉取；`bot_group_names_custom` 独立 config 键（openid→名 JSON），空名=删覆盖回落。否决：改写 `bot_group_names` 缓存——会污染 API 拉取逻辑且下次 fetch_group_info 覆盖回去。
- **tab 状态入 URL** [V]：tab 原来只活在前端 DOM，筛选是 GET 整页刷新必丢——提升为 `?tab=evt` 服务端渲染回显，顺带刷新/分享链接也保持；服务端只认 `evt` 其余回落。否决：筛选改 fetch 局部刷新——本次最小改动原则，且整页刷新与用户/成绩页同构。
- **业务事件 tab 下不启动 runPoll** [V]：修隐患，evt 下 2s 轮询是空转。
- 继承：日志双轨（app_logs 表 vs 内存环形缓冲）/ MakeWriter and() 双写 / 游标轮询 / 圈速破纪录才入日志 / 两进程共用代码只用 Logger / token daemon / order==2 挂圈 / 拦截器零浮点。

## 4. 失败的尝试 — 不要再试
> 全部前向搬运，永不丢弃。本轮新增 [V] 已亲证；继承死路见 `.handoffs/20260904120000-handoff.md` §4。

### 本轮新增（jsdom 验证脚本层面，写模板 JS 测试时都会再遇到）
- jsdom 里 `location.href = ...` 赋值 → 真实导航中止进程（EXIT=13 unsettled top-level await）[X]——测 goPage 这类跳转函数要用 node `vm` 沙箱 stub location，不进 JSDOM。
- JSDOM 页面脚本里 `const P = {...}` / `let KNOWN` → 词法变量不挂 `window` [X]——外部断言要用 `window.eval('P.tab')`，不能 `window.P`。
- stub fetch 立即 resolve 时，loadGroups 的重赋值会覆盖 renameGroup 刚写入的 GNAMES_CUSTOM → 假失败 [X]——先等一个 setTimeout 让 loadGroups 完成再测 rename；真实页面无此时序（非产品 bug）。
- 测试 Promise 里只调 checks 不调 resolve → 顶层 await 永挂 [X]——`setTimeout(() => { checks(w); resolve(); })`。

### 继承死路（全部 [X] 前向有效，详见 .handoffs/20260904120000-handoff.md §4）
- SSH：root 拒 / 端口 22 kex 拒 / 默认密钥不在 authorized_keys——正确姿势已入记忆 `paddock-vps-ssh-port-4142`（`ssh -i ~/.ssh_paddock/id_rsa -p 4142 takotsubo@8.134.50.222`，docker 需 sudo，compose 在 `~/paddock/docker-compose.vps.yml`）。
- `RingWriter` 透传 stderr → 双重输出 / 级别着色匹配 `[ERROR]` 带方括号 → 永不命中 / jsdom mock fetch 在 JSDOM 构造后注入 → `beforeParse` 钩子才行 / 用 `.env` 的 `PADDOCK_ADMIN_PASS` 登录管理端 → 401（用户改过密码）。
- fetchMe 漏 Authorization → 401 自动登出 / Crossfade key 绑筛选 / navigationIcon 只画不绑事件 / 两进程共用代码引 AlaMobileModule.logX → NoClassDefFoundError / externalFilesDir 跨进程不可见 / CropImageActivity 主主题闪退 / sqlx HRTB / axum 0.8 `:param` panic / VPS cargo build OOM（本地 build+scp）。
- lap_hook：圈完成等 2→0 回绕 / sectorOrder 1-based / trackToRace 当赛道名 / 拦截器浮点操作（FPSIMD 污染）/ proxy_shift 日志洪水 / IL2CPP dlopen / 后台线程调 Unity API。
- QQ bot：bearer_auth 调 QQ API / 被动回复用 event id 非 d.id / member_openid 在 d.author / "平台不推全量消息"实为手机QQ群内授权未开。
- 排行榜（模块仓）：圈速钳位保底 / Card forEach 渲染 205 行 / 手动 Animatable snapTo / 换源直接读筛选状态 / delay 排在伸缩后 / 行级 items 丢尾部留白 / SQL 直插缺 `::uuid` / adb 端口漂移。

## 5. 已知坑
- ⚠️ **`PADDOCK_ADMIN_PASS` 环境变量与实际管理端密码不符** [V]（继承）——seed 只在 admins 表空时生效，但清库重建会退化回 .env 旧值；别用 .env 密码做线上验证。
- ⚠️ **sqlx::migrate! 编译期嵌入** [V]（继承）——改/增迁移文件必须 `touch src/main.rs` 重编译，否则旧二进制跑旧迁移。
- ⚠️ **Garage 206 个测试/遗留头像对象** [?]（继承）——无害；无 shell/aws cli 清不掉。
- ⚠️ **双仓赛道中文名两份硬编码** [?]（继承）——契约源 PADDOCK_PLAN §5（`Shangai` 单 a）；改必须两边同改。
- ⚠️ **模块侧 Toast 文案"蒙扎国家赛车场 的"多空格** [?]（继承）——模块侧 parseToast 模板 `$track 的`；纯文案瑕疵，用户未点名修。
- ⚠️ **排行榜无实时刷新** [?]（继承，用户拍板暂不做）。
- ~~群全量消息根因~~ [V] 已解决；~~业务事件筛选跳 tab~~ [V] 本会话已修复（tab 入 URL）——不再搬运。

## 6. 下一步（有序）
1. **用户网页验证**（管理端密码只有用户知道）：a) v32 新功能——设置页群"改名"+日志页筛选不跳 tab；b) 继承 v31——`/admin/logs` 运行日志 tab 实时滚动（群内 @bot 发消息 payload 2s 内出现）+ 业务事件 30 条回填审计。
2. **群内 bot 新文案复测**（继承）：@bot 发申请码 / 不带码 / 私聊静默 / 播报实测——可对照日志页两个 tab 闭环验证。
3. **重构**（用户定案"下个版本重构模块和服务端的一堆东西"）——范围待用户指定，可能涉及模块仓。
4. （可选继承）排行榜实时刷新（暂不做）/ Toast 文案空格 / Garage 头像清理 / lint baseline 重生成。

## 7. 留给用户的开放问题
- 播报受主动消息额度限制（未认证约 4 条/月/群）：接受 / 申请认证？
- 单聊"支持指令"兜底已移除——是否需要私聊帮助指令？
- 计时赛积分卡"暂无"与"加载失败"是否区分显示？
- 重构范围清单待定。
