# HANDOFF — 读全文再开始干活

生成时间: 2026-09-04T17:58:40+08:00 · Git HEAD: `a4920dd`（paddock 仓；模块仓 `bcd545e`）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock 仓 `main` @ `a4920dd`（2026-09-04）；模块仓 `main` @ `bcd545e`
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `a4920dd`——HEAD 必是本次 handoff 提交，其 parent 才是文档记录的 SHA；不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记
- 先读: `README.md`（注册流 v3/bot 严格匹配/成绩页筛选三条目）+ 模块仓 `docs/PADDOCK_PLAN.md`（契约源）

## 1. 当前目标
**用户两轮共 10 条反馈全部落地并部署 v33–v35**：注册幂等恢复、车手 ID 建号时按最小空缺分配、重置密码口令「我需要重置密码」严格匹配+按群身份反查、reg_code 提不出码静默、播报模板等值迁移加空格、圈时改圈速、成绩页三组可组合筛选。线上 v35 运行。

## 2. 已验证状态 — 工作实际停在哪
- [V] 线上 **v35** 运行：`/v1/health` 200、`/admin` 200；compose 指向 `paddock-api:v35`；`docker logs` → listening on 0.0.0.0:8080
- [V] 迁移 0007 已执行（`_sqlx_migrations` 7 行），`pending_regs.reg_seq` 列已删；`user_reg_seq` 序列弃用留库（无引用，不删）
- [V] 车手号：`Takotsubo` 已 UPDATE 到 **1 号**（UPDATE RETURNING 确认）；发号 SQL 三场景验证：占{1,2,5,6}→发3、占{1,2,3}→发4、生产库现状→发1
- [V] 三个切片已推送：工作 `3c883e5`（6 文件 +251/−129 + 迁移 0007）→ README `a4920dd` → 本 handoff
- [V] jsdom 门禁 15/15 过 EXIT=0（`/tmp/jsdom-check/verify-laps-v34.mjs`：版本/圈速下拉渲染回显、goPage vm 沙箱带参、播报模板迁移字符串逐字比对）
- [V] 模块仓切片 `bcd545e` 已推送：密码圆点+显隐图标 / Toast 去赛道名 / 重置弹窗文案；lint EXIT=0、release APK 已装 MEIZU_20
- [?] **网页端到端未验证**（管理端密码只有用户知道）：成绩页三筛选组合+翻页保持、设置页播报模板新空格文案、注册重复点重弹
- [?] **群内实测未做**：新重置口令严格匹配、裸关键词静默、带码注册车手号=最小空缺

### 测试/build 输出（真实退出码）
```
cargo check EXIT=0（1 warning 既有遗留：laps.rs unused_assign）
node verify-laps-v34.mjs EXIT=0（15 项 OK）
docker build v35 → DONE；线上 health/admin 200
模块仓：./gradlew :app:lint EXIT=0；assembleRelease + adb install Success
```

## 3. 决策与理由
- **车手号=最小未占用正整数**（用户定案 v4）[V]：`MIN(generate_series(1, max+1) WHERE NOT IN (users.reg_seq))` 一条 SQL 找空洞，`COALESCE(...,1)` 兜底；弃号立即回收。并发竞态靠 reg_seq 唯一约束兜底（撞号建号失败可重试）。否决：`user_reg_seq` 序列——空洞永不回收，用户反复弃审越推越后（实测踩坑）。
- **register-request 幂等恢复** [V]：同名在途+密码一致（verify_password）→ 返回原 reg_code（200），模块端零改动重弹弹窗；密码不一致 409——防用户名抢占攻击（否则攻击者可改受害者 pending 密码再用自己的码抢建）。
- **重置密码按 member_openid 反查**（用户定案）[V]：member_openid 按群隔离——用户换群发指令匹配不到，NoUser 文案已引导"在注册时的群发"；单聊 user_openid 两体系不互通，一律引导回群。旧"重置密码 用户名"提取式已删。
- **读取时等值迁移**（`load_rules`）[V]：reset 关键词旧默认词→新词、播报旧模板→新模板，均只升级与旧默认**逐字相等**的规则，用户改过的不动；不改写库，回滚代码即还原旧语义。
- **成绩页筛选用 EXISTS 判定** [V]：个人最快=best_laps 同键同值、全服最快=records 持有者匹配（alltime/version 双分支）；EXISTS 不扩行，与分页 LIMIT 兼容。动态 WHERE 空段用 TRUE 占位，参数化绑定。
- **askama 模板坑**：循环变量是 `&i64`，比较要 `*v`；`|string` filter 不存在（derive 报 `cannot find module filters` 极具迷惑性）→ 模板字段统一 i64 数值比较。
- 继承：日志双轨/群名三层优先级/两进程共用代码只用 Logger/token daemon/order==2 挂圈/拦截器零浮点。

## 4. 失败的尝试 — 不要再试
> 全部前向搬运，永不丢弃。本轮新增死路见 paddock `.handoffs/20260904120000-handoff.md` §4 与下方。

### 本轮新增
- askama `{% if v|string == version %}` → E0433 `cannot find module filters` [X]——askama 无该 filter；数值字段直接 `==` 比较（循环变量需 `*v` 解引用）。
- jsdom 断言"全仓不含旧模板字符串"→ 假失败 [X]——迁移分支必须保留旧模板原文（等值迁移依赖），断言范围应限定 `preset_rules()` 函数体。
- WSL 里 `adb connect` 旧端口 → 拒绝连接 [X]——无线 adb 端口每次漂移，必须 `adb mdns services` 重新发现（已入模块仓记忆 `wireless-adb-mdns-rediscover`）；mdns 双条目取活的那条（连不上换下一个）。

### 继承死路（全部 [X] 前向有效，详见 .handoffs/）
- SSH：root 拒/端口 22 kex 拒——`ssh -i ~/.ssh_paddock/id_rsa -p 4142 takotsubo@8.134.50.222`，docker 需 sudo，compose 在 `~/paddock/docker-compose.vps.yml`。
- jsdom：`location.href=` 赋值真导航中止进程（用 vm 沙箱）/ 词法变量不挂 window（用 window.eval）/ fetch stub 要 beforeParse / loadGroups 时序假失败。
- `PADDOCK_ADMIN_PASS` 与实际密码不符 / sqlx::migrate! 编译期嵌入（改迁移必须 touch main.rs）/ fetchMe 漏 Authorization → 401 / CropImageActivity 主主题闪退 / sqlx HRTB / axum 0.8 `:param` panic / VPS cargo build OOM（本地 build+scp）。
- lap_hook：圈完成等 2→0 回绕 / sectorOrder 1-based / 拦截器浮点操作 / proxy_shift 日志洪水 / IL2CPP dlopen / 后台线程调 Unity API。
- QQ bot：bearer_auth 调 QQ API / 被动回复用 event id 非 d.id / member_openid 在 d.author / "平台不推全量消息"实为手机QQ群内授权未开。
- 排行榜（模块仓）：圈速钳位保底 / Card forEach 渲染 205 行 / 手动 Animatable snapTo / 换源直接读筛选状态 / 行级 items 丢尾部留白 / SQL 直插缺 `::uuid`。

## 5. 已知坑
- ⚠️ **线上 configs 存的旧规则 JSON** [?]——reset 规则 keyword/播报模板靠 `load_rules` 读取时等值迁移生效，库内原文未改（设计如此）；用户在管理端手动保存后即固化新值。
- ⚠️ **sqlx::migrate! 编译期嵌入** [?]（继承）——改/增迁移文件必须 touch src/main.rs 重编译。
- ⚠️ **Garage 206 个测试/遗留头像对象** [?]（继承）——无害；无 shell/aws cli 清不掉。
- ⚠️ **双仓赛道中文名两份硬编码** [?]（继承）——契约源 PADDOCK_PLAN §5（`Shangai` 单 a）。
- ⚠️ **模块侧 Toast 模板已去赛道名** [V] 本会话修复——不再搬运；~~播报模板无空格~~ [V] 已修复。
- ⚠️ **排行榜无实时刷新** [?]（继承，用户拍板暂不做）。

## 6. 下一步（有序）
1. **用户网页验证 v34/v35**：成绩页三筛选（含组合+翻页保持）、设置页播报模板空格文案、模块注册重复点重弹、车手号显示 1。
2. **群内 bot 复测**：「我需要重置密码」严格匹配（多字/漏字/带空格应静默）、「申请围场通行证」裸关键词静默、带码注册车手号=2（1 已占用）、破纪录播报新空格格式。
3. **重构**（用户定案"下个版本重构模块和服务端的一堆东西"）——范围待用户指定。
4. （可选继承）排行榜实时刷新 / Garage 头像清理 / lint baseline 重生成。

## 7. 留给用户的开放问题
- 播报受主动消息额度限制（未认证约 4 条/月/群）：接受 / 申请认证？
- 单聊"支持指令"兜底已移除——是否需要私聊帮助指令？
- 计时赛积分卡"暂无"与"加载失败"是否区分显示？
- 重构范围清单待定。
