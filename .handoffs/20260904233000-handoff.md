# HANDOFF — 读全文再开始干活

生成时间: 2026-09-04T22:08:08+08:00 · Git HEAD: `a68a4a3`（paddock 仓；模块仓 `9fe3865`）
信任规则: [V] = 交接时已用命令验证；[?] = 仅记忆未复核，当线索对待；[X] = 已证伪，别用。

## 0. 复核（下一会话先做）
- 锚点: paddock 仓 `main` @ `a68a4a3`（2026-09-04）；模块仓 `main` @ `9fe3865`
- 漂移检查: `git rev-parse HEAD~1` 是否仍 = `a68a4a3`——HEAD 必是本次 handoff 提交，其 parent 才是文档记录的 SHA；不一致以 git 实际输出为准
- 待重探的 [?]: 见下方标记
- 先读: `README.md`（积分公式 v39/四筛选/规则防覆盖条目）+ 模块仓 `docs/PADDOCK_PLAN.md`（契约源，本会话已同步 v4/版本键/积分 v39）

## 1. 当前目标
**8.0.6 三端适配之服务端侧 + 四轮用户反馈落地（v36~v39 已全部上线）**：8.0.6 版本适配、成绩页筛选改造（赛道/文案/空串修复/跳页）、总榜积分改版本独立赛季、线性积分公式 v39、bot 规则防覆盖。

## 2. 已验证状态 — 工作实际停在哪
- [V] 线上 **v39** 运行：`/v1/health`=ok、compose 指向 `paddock-api:v39`、`docker logs` → listening on 0.0.0.0:8080
- [V] 两切片已推送：工作 `a9ba14a`（7 文件 +256/−90）→ README `a68a4a3`
- [V] 积分三维度线上 curl 实测：总榜 **500**（8.0.4 四赛道 400 + 8.0.6 摩纳哥 100）/版本榜 8.0.4=400、8.0.6=100——新公式+新总榜口径正确
- [V] 空串筛选修复线上验证：`GET /admin/laps?version=&gp_index=&best=&q=` 未登录返回 303→/admin（反序列化已通过，不再 400）
- [V] 规则防覆盖（v37）部署后核对：库内用户定制文案四条逐字未动
- [V] jsdom 门禁 `verify-laps-v36.mjs` 21/21 OK EXIT=0（文案/赛道下拉/版本全集/goPage 带参/跳页/补录版本）
- [?] **管理端网页端到端未验证**（凭据只有用户有）：四筛选组合、跳页、补录版本下拉、track/5?version=200150 在 UI 的显示
- [?] **群内 bot 复测未做**（继承）：重置口令严格匹配、裸关键词静默、带码注册车手号=最小空缺

### 测试/build 输出（真实退出码）
```
cargo check EXIT=0（2 既有 warning：laps.rs unused_assign / server_best）
node verify-laps-v36.mjs → 21 项 OK 0 FAIL EXIT=0
docker build v36~v39 → 全 DONE；线上 health=ok
```

## 3. 决策与理由
- **积分公式 v39**（用户定案）[V]：`round((N−rank)×100/N)`——第一 100、每名次递减 100/N、虚位第 N+1 名 0、N=1 给 100（例：2 人=100/50/虚位0）。三处统一（版本榜/总榜/`/v1/me` total_points）。否决旧式 `1+(N−rank)×99/(N−1)`（用户否）。
- **总榜=版本独立赛季**（用户定案）[V]：每 (version_code, gp_index) 独立 rank 计分后按用户 sum。两版本全赛道第一=3200。否决跨版本 min（旧 user_best CTE，用户纠正）。
- **规则防覆盖**（用户投诉"每次部署覆盖文案"驱动）[V]：`load_rules`——key 存在（哪怕 `"[]"`/解析失败）即用户数据，永不回落 preset_rules()（预设只给从未写入的 key）；解析失败宁 bot 静默+tracing::error 保原文。`api_save_rules` 覆盖审计：`load_rules_raw`（原样库值）对照改前后 template/keyword diff + removed ids 入 app_logs。**部署本身从不写 configs**——真实覆盖链=读路径回落预设→设置页全量保存（前端整个 RULES 数组回传）固化。
- **成绩页筛选** [V]：新增赛道下拉（版本前，16 赛道全量，默认"所有赛道"）；文案"版本（全部）"→"所有版本"、"圈速（全部）"→"所有圈速"；版本下拉=固定全集 KNOWN_VERSIONS ∪ 库内 distinct（显示名 Rust 侧预处理成 (code,name) 对——askama 不能调函数）。
- **空串筛选** [V]：serde `Option` 只兜字段缺失不兜空串 → `empty_str_as_none`（deserialize_with）。
- **翻页跳转** [V]：renderPager 加"跳至 [input] [跳转]"（Enter/按钮，越界不跳）——admin_base 通用函数，用户页/日志页自动受益。
- **补录版本可选** [V]：ApiAddLap 加 version_code（默认 200150，白名单 [200150,200146]）；弹窗下拉。
- **fetch_laps 绑定链式化** [V]：占位符动态顺延（$1 like→version→gp_index→LIMIT/OFFSET），绑定顺序与占位符序号一致；count_laps 拆分独立函数先钳页再查数据（修筛选后 pages 虚高的既有 bug）。
- 继承：日志双轨/群名三层优先级/读取时等值迁移/两进程共用代码只用 Logger。

## 4. 失败的尝试 — 不要再试
- **serde_qs 做单测** [X]：非依赖 E0433；serde_urlencoded 也不是直接依赖——为单测加依赖不值，jsdom+线上 curl 已覆盖。测试代码已删。
- **Python 批量替换缩进不匹配** [V]：auth_handlers.rs 的公式替换 assert 失败（多行 SQL 缩进与 leaderboard.rs 不同）——批量替换前先 grep 确认实际缩进。
- **format! 里 `${lim}`** [X]：`$` 前缀骗不过 format! 的 `{}` 解析（expected expression found `.`）——动态占位符改用 String 拼接 push_str。
- **`json::Value`** [X]：admin.rs 只 `use serde_json::json`（宏），类型是 `serde_json::Value`。
- **模板里调 version_display()** [X]：askama 无任意函数调用（E0433）——预处理成字段（继承死路本轮再踩一次，因为"顺手"）。
- **VPS cargo build** [X]（继承）：OOM——本地 build → save|gzip（~6MB）→ scp → load。
- **jsdom location.href 赋值** [X]（继承）：真导航杀进程；vm 沙箱 stub location，sandbox 需补 URLSearchParams。
- 继承（详见 .handoffs/20260904220808-handoff.md §4）：SSH 4142/播报模板迁移断言范围/askama `|string`/sqlx migrate 编译期嵌入/fetchMe 漏 token/sqlx HRTB/axum `:param` panic。

## 5. 已知坑
- ⚠️ **线上 configs 存的规则 JSON 库内原文未改** [V]（v37 后设计如此：等值迁移只在内存，用户手动保存即固化）。
- ⚠️ **sqlx::migrate! 编译期嵌入** [?]（继承）——改迁移必须 touch src/main.rs。
- ⚠️ **Garage 206 个测试/遗留头像对象** [?]（继承）——无害，无 shell/aws cli 清不掉。
- ⚠️ **管理端网页端到端验证未做** [?]——见 §2。
- ⚠️ **双仓赛道中文名两份硬编码** [?]（继承，契约源 PADDOCK_PLAN §5，`Shangai` 单 a）。
- ⚠️ **排行榜无实时刷新** [?]（继承，用户拍板暂不做）。

## 6. 下一步（有序）
1. **用户网页验证 v39**：成绩页四筛选（含组合+翻页跳转保持）、补录弹窗版本选择、围场主页积分 500（模块端）。
2. **群内 bot 复测**（继承）：「我需要重置密码」严格匹配、裸关键词静默、带码注册车手号=最小空缺、新积分播报。
3. （可选）`deploy/deploy.sh` 固化部署链（用户认可，未实施）。
4. （可选继承）排行榜实时刷新 / Garage 头像清理。

## 7. 留给用户的开放问题
- 播报受主动消息额度限制（未认证约 4 条/月/群）：接受 / 申请认证？
- 重构范围清单待定（用户提过"下个版本重构模块和服务端一堆东西"）。
- 计时赛积分卡"暂无"与"加载失败"是否区分显示？
