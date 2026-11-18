# 服务端构建性能：Docker cache mount 与 profile 调优

> 状态：活跃 · 最后编纂：2026-11-17（第二轮，12 agent 深度调研后重写 5 处错误结论）
> 一句话结论：**真正的构建痛点是 `docker build` 每次全量重编 301 依赖（3m42s），不是本地 `cargo build`。`RUN --mount=type=cache,target=/build/target` 把它降到 2m00s（−45%），产物字节级不变。而剩下的 2 分钟里 86% 是 fat LTO 段——这是物理下限，9 类"提速手段"已逐一实测/查证否决。**

## 1. 结论（发现）

### 1.1 两条物理隔离的构建链，必须先分清优化对象 [V]

| 链路 | target 目录 | 目标 triple | 触发场景 | 实测耗时 |
|---|---|---|---|---|
| 本地增量 | 宿主 `target/debug` | gnu | 改代码本地验证 | **2.5~2.9s** |
| 本地 release | 宿主 `target/release` | gnu | 罕见（本地验 release 行为） | 58~70s |
| **Docker 镜像** | 容器内 `/build/target` | **musl** | **每次改代码** | **3m42s → 2m00s** |

⇒ **本地 `target/` 的任何优化对 Docker 路径 100% 无效**（不同 triple、不同目录、`.cargo/` 不进镜像）。

### 1.2 各场景基线（实测，热 daemon / 热 cache）[V]

| 场景 | 耗时 | 备注 |
|---|---|---|
| `cargo check` 改一行 | 0.93s | 最快反馈 |
| `cargo build` 零改动 | 1.22s | |
| `cargo build` 改一行 | 2.5~2.9s | 已无优化空间 |
| `cargo build --release` 改一行 | 58~70s | fat LTO 单核串行 |
| **`docker build` 改一行（无 cache mount）** | **3m42s** | 301 依赖全量重编 |
| **`docker build` 改一行（有 cache mount）** | **2m00s** | 依赖全复用，只编 paddock-api |

### 1.3 依赖编译在日常成本中为 0 [V]

`cargo build --timings` 显示 395 个 unit 中**只有 1 个执行**（`paddock-api [bin]`），301 依赖全 `Fresh`。
`deps/*.rlib` 的 mtime 全部是 **2026-08-31** —— 三个月未重编。

⇒ **任何"让依赖编得更快"的手段（`[profile.dev.package.""] debug=false` 等）日常收益为零**，只在下次依赖变动时才兑现。

### 1.4 fat LTO 是单核串行，物理不可并行 [V]

CPU 采样（57 秒窗口，29 个点）：**27/29 是 `procs=1, cpu_sum=100%`**，12 核里 11 核空闲，**利用率 8.3%**。
USER/WALL 比：fat 约 1.0×（单核跑满无等待），thin 约 6.6×（多核并行）。

根因（compiler-team #1023 原文）：**fat LTO 把所有参与 crate 的 LLVM IR 合并成单个 module，然后送进一个 backend codegen job**。
⚠️ **`codegen-units = 1` 不是语义要求**（2026-11-17 修正，原表述错误）：官方 `rustc` 文档只说"设为 1 可提升生成代码性能但编译更慢"，**从未说 fat LTO 必须 cgu=1**。它是**额外的**一档优化（消除 CGU 边界让内联更彻底），是可调的时间/性能权衡 —— 只是调它的收益也很小，见 §1.7。

### 1.5 内存不是约束 [V]

release 全量构建峰值：**used 3.46GB / available 最低 6.49GB / swap 全程 0 使用**。
10GB 上限下有 6.5GB 余量。`overcommit_memory=0` 但从未接近触发。

2026-11-17 补测**全量（非增量）fat LTO 峰值**：**used 4.82GB / available 5.13GB / swap 未用**（`free -g` 实测）。
⇒ 全量比增量只高约 1.3GB，10GB 上限下仍有 5GB 余量，**原 §5 那条 [?] 结案**。

### 1.6 `docker build` 慢的真正机制 [V]

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY paddock-api ./paddock-api        ← 任何一行 .rs 改动 ⇒ 此层失效
RUN cargo build --release -p paddock-api   ← 连带重跑，容器内 target/ 是全新的
```

BuildKit 按**内容哈希**判定层缓存（`touch` 不触发重编，改内容才触发）。
Docker 层缓存对源码改动**天然无效** —— 这不是配置问题，是层模型的固有行为。

### 1.7 定量归因：86% 在 LTO 段 [V]

`-Z time-passes` 实测（rustc 1.97.1，2026-11-17）：

```
total              71.086s   100%
LLVM_fatlto        43.665s    61.4%   ← 真瓶颈
LLVM_passes        17.433s    24.5%   ← 也在 LTO 区间内
codegen_crate       5.155s     7.3%
monomorphization    3.789s     5.3%
type_check_crate    2.434s     3.4%
coherence_checking  1.708s     2.4%
MIR_borrow_checking 1.270s     1.8%
link_binary         0.525s     0.7%
run_linker          0.406s     0.6%   ← 换链接器最多救 0.4 秒
⇒ LTO 相关合计 61.038s = 85.9%
```

**区间自洽验证**：`LLVM_passes + LLVM_fatlto = 61.098s ≈ finish_ongoing_codegen 61.038s`（差 0.06s 为测量开销）⇒ 分段数据可信。

**这一个数字可以否掉一整类方案**，不必逐个研究：

| 方案 | 它想救的是 | 理论上限 |
|---|---|---|
| 换链接器（lld/mold/wild） | `run_linker` 0.406s | **0.6%** |
| parallel frontend | `type_check` 2.434s | 3.4% |
| `codegen-units` 1→16 | `codegen_crate` 5.155s | **负收益**（见 §2.1.2 实测：cgu=16 让 codegen 从 5.2s 涨到 10.4s） |
| 拆 crate | 同上，且 LTO merge 输入变多 | **负收益** |
| sccache / kache | bin crate 不缓存 | **0%** |
| Cranelift | `lto=true` 直接 `fatal()` | 不可用 |

**验证命令**：`RUSTC_BOOTSTRAP=1 cargo rustc --release -p paddock-api --bin paddock-api -- -Z time-passes`
⚠️ 必须在**没有其他构建并发**时测——实测并发干扰会让数字失真 2.5 倍（2m31s vs 基线 58~70s）。

#### 1.7.1 独立验证：并行前端确实动不了 LTO [V]

用**完全不同**的手段（`-Zthreads` 并行化前端）去攻击那 14%，实测结果：

| pass | threads=1 | threads=8 | 变化 |
|---|---|---|---|
| type_check_crate | 2.43s | **0.74s** | −70% |
| coherence_checking | 1.71s | 0.51s | −70% |
| MIR_borrow_checking | 1.27s | 0.31s | −76% |
| **前端合计** | **~5.4s** | **~1.6s** | **−3.8s** |
| **LLVM_fatlto** | 37~40s | **37~40s** | **完全不变** |
| **finish_ongoing_codegen** | 54~56s | **54~56s** | **完全不变** |

⇒ **并行前端把前端压掉了 70%，但 LTO 段一秒都没动**，总收益约 5%。这是 §1.7 那张表最直接的实验证明。

⚠️ **只有"单次运行内的 pass 分解"可信，wall-clock 总计是垃圾数据**：同一次实测里 `threads=1` 的 total 是 64.5s，而 `threads=8` 的 total 是 **121.6s（比单线程还慢一倍）** —— 那是 12 核机器 load 10~12、2~3 个 agent 同时编译同一项目的纯争用，不是并行前端变慢。pass 分解不受 wall-clock 争用影响（同一进程内自计），所以上表的方向可信。

**机制**：parallel frontend 只并行 "lexing → 生成 backend IR"（typeck/borrowck/coherence），而 **LTO 是 LLVM 后端的单模块作业**，结构上不在它的作用面内。

⚠️ 官方基准（单 crate 全新构建，8 线程）：`check` −44.2%、`opt` −27.9%，但**增量重建仅 −3~5%** —— 与本项目场景（依赖全 Fresh、只重编 1 个 crate）正好落在最差那一档。

⚠️ 并行前端**未进任何 stable**：nightly 自 `nightly-2026-11-14` 起默认开启（仅 2 线程），官方定位是"收集真实世界问题，**之后**再稳定"，无承诺版本号。

## 2. 依据（研究）

### 2.1 cache mount 的 A/B 实测 [V]

| 配置 | 改一行耗时 | 产物体积 |
|---|---|---|
| 无 cache mount | 3m42s | 5,946,592 B |
| **cache mount（fat LTO）** | **2m00s** | **5,946,592 B**（完全相同）|
| cache mount + `lto="thin"` + `cgu=16` | 2m16s（**更慢**）| 8,314,080 B（**+40%**）|

⇒ **cache mount 是唯一有效且零代价的手段。thin LTO 在 Docker 路径上纯亏。**

**为什么 thin 在 Docker 里无效**：宿主机上 thin 把 release 增量从 62s 降到 25s（多核并行），但 Docker 内瓶颈是**容器 CPU 配额 + musl 交叉编译**，不是 paddock-api 的 LTO。

#### 2.1.1 thin 的机制（2026-11-17 新增，机制 [V] / 倍数 [?]）

**机制（可信）**：thin 增量快的根本原因**不是** cgu 并行 codegen，而是 **thin LTO 不做全程序单模块合并**。
- fat LTO 必须把全部依赖的 LLVM bitcode **合并成单一模块**再优化 —— 这就是 §1.7 里那 `LLVM_fatlto 43.665s（61%）` 在做的事
- thin LTO 只做 **per-module 摘要 + 按需导入**，跳过了整个合并阶段

⇒ 这条机制与 §1.7 的分段数据完全吻合，是本轮最有价值的发现。

⚠️ **倍数不可引用**。2026-11-17 本地实测（宿主 gnu，bench-tgt）：

**最终可信数据（同窗口 3×3 对照，load ≤3.71）**：
| 配置 | 增量中位数 | 体积 | 结论 |
|---|---|---|---|
| **z + fat + cgu1（当前）** | **69.2s** | 5,807,416 B | **最快** ✅ |
| s + fat + cgu1 | 73.3s（**+6%**） | 6,043,648 B（+4.07%） | 更慢更肥 ❌ |
| 3 + fat + cgu1 | 不可引用 | 7,984,288 B（+37.5%） | 更慢更肥 ❌ |
| thin + cgu16 | 不可引用（抢多核，对负载极敏感） | 8,199,720 B（**+41.2%**） | 见 §2.1.2 time-passes |

⚠️ **`thin` 的绝对秒数不可引用**，但它的 `time-passes` 分段可信（LTO 是单核段，对负载不敏感）——见 §2.1.2。

**早期受污染数据（已废弃，仅作反例留存）**：fat 曾测出 138.2/103.7s、z 102.6/108.1s、s 80.5/87.2s —— 全部来自 load > 9 的窗口，失真最高达 1.5 倍。
**安静窗口独立复测基线**：`z` = **58.2s / 61.3s**（load 1.54 / 2.11），与文档原有 58~70s 吻合。

⚠️ **与 Docker 路径的正面矛盾**：上一轮 Docker 实测（musl + cache mount）是 thin **慢 13%**（2m16s vs 2m00s），体积 8,314,080 B —— 与本轮本地 thin 的 8,199,720 B 仅差 1.4%，确认是同一配置。
⇒ **三个环境的数字不能互相外推**（宿主 fat / 宿主 thin / 容器 thin 各有不同瓶颈）。这与 §4.4 那个「gnu 观测套 musl」是同一类错误的两个实例。
⇒ **待办**：Docker 路径的 thin vs fat 需在**无并发**条件下重测才能定案（约 8 分钟）。在重测之前，**维持"thin 在 Docker 无效"的结论不变**。

**若将来要采纳 thin**（前提：接受体积 +41%）：用独立 profile 而非改 release——
```toml
[profile.fast]
inherits = "release"
lto = "thin"
codegen-units = 16
```
`cargo build --profile fast` 输出到 `target/fast/`，与 `target/release/` **物理隔离**（RFC 2678 自定义 profile）。日常本地验证用它，**正式发版仍用 fat LTO 保体积**。

#### 2.1.2 thin 的 time-passes 证实机制（并否掉 cgu 单独优化）[V]

同一命令、同一项目、分段对比：

| 阶段 | fat+cgu1 | thin+cgu16 | 变化 |
|---|---|---|---|
| **LLVM_fatlto → LLVM_thinlto** | **43.665s (61%)** | **23.223s (54%)** | **−20.4s** |
| LLVM_passes | 17.433s | 6.476s | −11.0s |
| **codegen_crate** | 5.155s | **10.422s** | **+5.3s（变慢！）** |
| codegen_to_LLVM_IR | 1.034s | 4.394s | +3.4s |
| monomorphization | 3.789s | 5.542s | +1.8s |
| type_check_crate | 2.434s | 3.682s | +1.2s |
| run_linker | 0.406s | 0.644s | — |
| link_binary | 0.525s | 0.813s | — |
| **total** | **71.086s** | **43.225s** | **−39%** |

**两条结论**：
1. **机制确认**：`LLVM_fatlto 43.7s` 被换成 `LLVM_thinlto 23.2s`，砍掉 20.4s —— 这就是"不做全模块合并"省下的时间。
2. ⚠️ **`codegen-units = 16` 本身是负收益**：`codegen_crate` 从 5.2s **涨到** 10.4s（调度/合并开销 > 并行收益）。⇒ **单独把 cgu 改成 16 会让构建变慢**，它只在 thin 下作为配套出现。这是 §1.7 那张表里 `cgu 1→16` 一行改判"负收益"的依据。

#### 2.1.3 干净基线与负载敏感度 [V]

`cargo clean -p paddock-api` 后重建基线，以及**load < 2.2 的安静窗口独立复测**：

| 来源 | 轮次 | 耗时 | load |
|---|---|---|---|
| timing-breakdown | 零改动 | 0.23s / 0.27s | 已恢复 |
| timing-breakdown | 增量 r1 | 61.6s | 4.78 |
| timing-breakdown | 增量 r2 | 91.5s | 9.76 |
| **本人独立复测** | **增量 r1** | **58.2s** | **1.54** |
| **本人独立复测** | **增量 r2** | **61.3s** | **2.11** |
| **本人独立复测** | **增量 r3** | **66.6s** | **2.08** |

⇒ **`opt-level="z"` + fat + cgu=1 的真实基线 = 58~67s**（load < 2.2 时），与文档原有 58~70s 吻合。
⇒ 两轮独立测量（不同 agent、不同时间）都落在这个区间 ⇒ **基线可信**。

⚠️ **同一配置两轮差 1.48 倍（61.6 vs 91.5s），唯一变量是 load（4.78 vs 9.76）** ⇒ **本机任何未标 load 的绝对秒数都不可引用**。

⚠️ **但 load 也不是充分解释变量**：本人复测的 r2（load 2.11 → 61.3s）与 r3（load 2.08 → 66.6s）**load 几乎相同，耗时却差 8.6%**。
⇒ **本机存在约 ±5~9% 的不可消除噪声底**。**任何小于 10% 的"提速"在本机都无法可靠测量** —— 这条判据同样适用于 `opt-level` z→s 那个 6% 差异（它本身就贴着噪声底）。
⇒ 这反过来强化了结论：既然噪声底 ±8%，而所有候选优化都在噪声范围内或更差，**"当前配置已最优"这个判断是稳的**。

**可引用 vs 不可引用（本轮实测的划分）**：
- ✅ 可引用：fat 的 `-Z time-passes` 全部分段（区间自洽验证通过）、thin 的 `-Z time-passes`（LTO 是单核段，对负载不敏感）、**安静窗口基线 58~62s**、全部产物体积（与负载无关）
- ❌ 不可引用：所有在 load > 4 窗口测的增量墙钟秒数（fat 138/103s、thin 49/74/28/30s、opt3 106/92s、opts 80.5/87.2s）、opt-level 全量 141/177/168s

⚠️ **`opt-level="s"` 的"快 20%"已证伪**（同窗口 3×3 对照，load 全程 ≤3.71）：

| 配置 | run1 | run2 | run3 | 中位数 |
|---|---|---|---|---|
| **z + fat + cgu1（当前）** | 69.2s | 66.6s | 70.3s | **69.2s** |
| s + fat + cgu1 | 71.0s | 73.3s | 78.9s | **73.3s** |

⇒ **`z` 比 `s` 快约 6%，与 sharnoff 权威实测（`z` 41.8s < `s` 45.7s < `3` 51.0s）一致。**
早期那个 80.5s 是 load 12+ 窗口的单次结果，**同窗口的 z（102.6/108.1s）才是被污染的一方**（被抬高了约 50%），制造出"`s` 快 20%"的假象。
⇒ `s` 的体积代价是真的（+4.07%，与负载无关），但**换不来任何时间收益**。

**方法论教训**：在噪声区间里两个数相差 20%，无法区分"真差异"与"两次噪声之差"。只有**同窗口、多轮、记录 load 的对照**才能定论。

#### 2.1.4 Docker 路径 thin vs fat：同窗口交替 A/B 定案 [V]

**这是本节最重要的结论**。2026-11-18 用同窗口交替 A/B（fat→thin→fat→thin，每次真实改注释触发重编）实测：

| 配置 | 耗时 | 当时 load |
|---|---|---|
| fat r1 | **113s** | 2.31 |
| thin r1 | **119s** | **9.33** |
| fat r2 | **125s** | 2.35 |
| thin r2 | **118s** | **8.86** |

**关键观察：load 那一列不是外部干扰，是 thin 自己拉上去的。**
- fat 恒 load 2.3（单核跑满，不产生排队）
- thin 恒 load 9.0（真的在并行用多核）

⇒ **thin 用了 4 倍的 CPU，耗时却和 fat 打平（118 vs 119s）。** 并行优势被完全吃掉。

**为什么**：容器**没有 CPU 配额限制**（实测 `cpu.max = max 100000`，`nproc = 12`，12 核全可用）。⇒ 不是配额问题。
最可能的解释是 **musl 的 malloc 多线程扩展性差**：`rust:1.97-alpine` 的 rustc 本身是 musl 链接的，musl 分配器用简单全局锁，thin 的并行 codegen 线程越多、锁竞争越重，把并行收益抵消掉。fat 单核不需要多线程分配，所以两边一样快。

**⇒ Docker 路径结论：thin 无收益（打平），且有 +3.2MB 镜像代价。不采纳。**

⚠️ 这与本地（宿主 gnu）实测的「thin 快 39%」不矛盾 —— 宿主用 glibc，多线程分配无锁竞争。**三个环境（宿主 gnu / 容器 musl / 宿主 thin）不能互推**，见 §4.4 同源教训。

**镜像体积实测**：fat 21.1MB → thin 24.3MB（**+3.2MB，+15%**）。注意这比"二进制 +41%"温和得多，因为 alpine 基础层稀释了增量。

### 2.2 体积账本（避免重复计数）[V]

`target/release/deps` 726MB 的构成被数过三遍：

- rlib 399MB 中 **259MB 是 `lib.rmeta`**（ar 归档内成员）
- 而 `deps/` 下另有 **259MB 独立 `.rmeta` 文件，是同一份数据的副本**
- 真正参与 LTO 的 `.o` 目标码只有 **124MB**

⇒ 判断"LTO 输入规模"必须**拆 ar 成员**，不能看目录 `du`。

### 2.3 磁盘治理：只有 `incremental/` 可安全清理 [V]

| 目标 | 体积 | 能否删 |
|---|---|---|
| `target/debug/incremental` | 1.3GB（14 个历史 session）| ✅ **可删**，只重编自有 crate（几十秒）|
| `target/debug/deps` 旧二进制 | 611MB（5 份历史 hash）| ✅ 可删，cargo 只认最新那份 |
| `target/release/deps` | 624MB | ❌ **不能删**，就是当前在用的 301 依赖 |
| `deps/` 里多 hash 的 rlib | — | ❌ **不能删**，是 features/profile 不同的合法产物 |

**判据红线**：`cargo-sweep` 类工具按 **mtime** 判定"陈旧"，但 **rlib 的 mtime 是编译时刻，不是使用时刻**。依赖三个月没重编恰恰因为"在用且没变" —— 按时间筛会删光活跃依赖。

## 3. 实现（固化）

### 3.1 Dockerfile（当前形态）

```dockerfile
ENV SQLX_OFFLINE=true
# cache mount 让容器内 target/ 独立于层链存活
RUN --mount=type=cache,target=/build/target \
    cargo build --release -p paddock-api && \
    mkdir -p /build/out && \
    cp /build/target/release/paddock-api /build/out/paddock-api

FROM alpine:3.21
COPY --from=builder /build/out/paddock-api /usr/local/bin/paddock-api
```

⚠️ **必须把产物拷到 cache 之外**（`/build/out`）：cache mount 内容**不写回镜像层**，否则 `COPY --from=builder` 报 `not found`。

### 3.2 维护红线

| 命令 / 机制 | 对 cache mount 的影响 |
|---|---|
| `docker builder prune -f` | **清除** ❌ |
| `docker builder prune -af` | 清除 ❌ |
| `docker system prune` | 清除 ❌ |
| **BuildKit 默认 GC** | **清除** ❌（见下，最隐蔽） |

⇒ **`prune` 之后第一次构建要付一次全量（3~4 分钟）重建 cache**，之后恢复 2m00s。

⚠️ **更隐蔽的第二把刀：默认 GC 策略**（2026-11-17 本机 `docker buildx inspect --bootstrap` 实测）：

```
GC Policy rule#0:
  Filters:        type==source.local type==exec.cachemount type==source.git.checkout
  Keep Duration:  48h0m0s
  Max Used Space: 13GiB
```

⇒ **cache mount 闲置 48 小时、或 build cache 超 13GiB 就会被自动 GC 掉，完全不需要跑 prune。** 这是"明明没 prune 却退回 3~4 分钟"的机制。

**诊断命令**：
```bash
docker buildx inspect --bootstrap | grep -A8 "GC Policy"   # 看策略
docker buildx du --verbose | grep -A6 "cached mount"       # 看 Last used / Usage count
```
**日常替代 `prune -a` 的写法**（保住 cache mount，只清层缓存）：
```bash
docker buildx prune --filter "type!=exec.cachemount"
```

⚠️ **cache mount 内容没有任何官方持久化通道** [V]：`--cache-to type=local|registry|gha` **只导出层 blob，cache mount 内容永远不进导出包**（buildkit 作者 tonistiigi 明确回复，buildkit#1512 / buildx#1283）。彻底免疫只能换 `docker-container` driver 建独立 builder（缓存在命名 volume 里，`docker system prune` 打不到）。

⚠️ **`id=` 必须显式写** [V]：默认 id **就是 target 路径**（实测 `id "//build/target"`）。两个 Dockerfile 用同一挂载点会**静默共用同一份缓存**，排查时表现为"缓存被别的项目污染"。

### 3.3 本次回收的磁盘 [V]

`docker builder prune -af` 清掉 **72.41GB** 死 build cache（347 条目，`ACTIVE 0`）。
`/dev/sdd` 从 **157GB → 90GB**。
这些缓存之所以全死：每次源码改动都产生全新的层链，旧链永久滞留且 cargo 无自动 GC。

## 4. 已否决的方案

### 4.1 sccache [X]
**否决理由**：三大卖点在本项目全部不成立 ——
- 跨项目复用：**只有一个 Rust 仓库**
- CI 缓存：**本项目无 CI**（`.github/` 不存在）
- 切分支复用：**只有 main 一个分支**

唯一成立场景是 `cargo clean` 后全量重建。且 release（`lto=true`+`cgu=1`）下实测**反慢 8%**（2m02s→2m12s）。另需 `CARGO_INCREMENTAL=0`，砍掉 debug 最大加速器。

### 4.2 `CARGO_TARGET_DIR` 共享目录 [X]
单仓库收益恒为零，只带来锁竞争、feature 污染、`cargo clean` 语义爆炸。

### 4.3 `cargo-sweep` / `cargo-cache` / `cargo-nextest` [X]
- `cargo-sweep`：mtime 判据会删活跃依赖（195 个 crate 中仅 2 个真过期），且已缺维护者
- `cargo-cache`：最多回收 212MB（0.026%），`~/.cargo/git` 不存在
- `cargo-nextest`：本仓测试全部 `#[ignore]` + 需真库且**必须串行**，与其并行优势方向相反

### 4.4 换链接器（mold / wild / lld）[X]

⚠️ **本节 2026-11-17 重写**。旧表述「rustc 1.97.1 已在用 rust-lld / 系统 lld 是 19.0，装它反而是降级」**是错的** —— 它把宿主 **gnu** 路径的观测套到了 Docker 的 **musl** 路径上。

**两条链的 linker 实际不同** [V]（在 `rust:1.97-alpine` 镜像内实测）：
| 路径 | compiler | linker | 证据 |
|---|---|---|---|
| 宿主 gnu | rustc 1.97.1 | **rust-lld**（Rust 1.90 起 gnu 默认换 lld） | 宿主观测 `linker-flavor: gnu-lld-cc` |
| **Docker musl** | rustc 1.97.1 | **GNU ld 2.45.1** | rustc 传 `-plugin .../liblto_plugin.so`（binutils 的 LTO 插件） |

镜像里 **LLD 22.1.6 是自带的**：`$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-musl/bin/rust-lld`，`-flavor gnu` 即当 ld.lld 用。
⚠️ **但它实际上用不了**：工具链自带的 `rust-lld` **编译时没开 zlib**，链接本项目会直接报错：
```
ld: error: libring-....rlib(.debug_line) is compressed with ELFCOMPRESS_ZLIB,
    but lld is not built with zlib support
```
上游已知问题（rust-lang/rust#130063、llvm-project#184177）。Alpine 系统的 `lld22` **有** zlib，所以 `apk add lld` 反而能跑（但那是 22.1.3，比捆绑的 22.1.6 还旧）。

**容器内四链接器实测**（同容器、同缓存、同 CPU 配额，每轮 `touch` 强制重链，各 2 次）：

| 链接器 | `run_linker` | 备注 |
|---|---|---|
| **GNU ld 2.45.1（当前默认）** | 0.704s / 0.748s | 基线 |
| **mold 2.39.1** | **0.162s / 0.187s** | 最快，Alpine 官方仓库可直接 `apk add mold` |
| Alpine lld 22.1.3 | 0.282s / 0.271s | `apk add lld` |
| wild 0.11.0 | 0.214s / 0.653s | 波动 3 倍，且 `-z text` 未支持 |

⚠️ **能升级 LLD 吗？不能**：镜像基础 Alpine 3.24.1，仓库只有 lld18/lld20/lld22（22.1.3 比捆绑的 22.1.6 还旧），`lld23-23.1.5` 只在 edge 分支。

**实测：换 LLD 无收益** [V]
- 小样例（397KB 产物，fat LTO，各 3 次）：GNU ld 5.43~6.01s vs LLD 5.68~6.05s —— 噪声内；体积小 1.7%
- **真实项目 `-Z time-passes`：`run_linker` = 0.406s（宿主）/ 0.74s（容器），占 total 的 0.5~0.6%**（见 §1.7）
- 换 **mold** 是唯一有实测收益的（省 ~0.54s = **0.4%**），但 `RUSTFLAGS` 会让**全部 301 依赖 fingerprint 失效 ⇒ 首次全量重编 3~4 分钟**。为省 0.5 秒付一次全量重建，只有顺手改 Dockerfile 时才划算。
- mold 官方论文 §6.3 亦明确：启用 LTO 时 plugin 的编译步骤主导端到端链接时间，链接器速度优势被削弱
- 第三方实测佐证：fasterthanli.me「there's just not that much linking to do」；Rust 论坛经典帖（用户以为 mold 没生效）自答「what's taking the time is the fat LTO」

⇒ **否决成立，但理由是"瓶颈是 LTO 合并（86%）而非链接（0.5%）"，不是"已经在用 lld"。**
**翻案条件**：若 `run_linker` 实测 > 10 秒（当前 0.4~0.7 秒，差 14 倍，无望）。

### 4.5 `lto="thin"`（本地路径）[X]（对 Docker）
宿主机上有效（62s→25s，−58%），但**Docker 路径反而更慢**且体积 +40%。
若将来需要本地快速 release 验证，可用独立 `[profile.fast]`（`inherits="release"` + `lto="thin"` + `cgu=16`），输出到 `target/fast/` 物理隔离。

### 4.6 `.dockerignore` [X]（不是当前问题）
BuildKit **惰性传输**只发匹配 `COPY` 模式的路径：实测命中 **45 个文件 / ~800KB**，遍历 10498 文件仅 0.028s。
4.6GB 上下文**没有造成可测量的传输开销**。它的价值是防御性的（防止将来有人写 `COPY . .`），**不省编译时间**。

### 4.7 Cranelift 后端 [X]

**决定性障碍：`lto = true` 直接 hard error，一行都编不过。**

`rustc_codegen_cranelift/src/lib.rs` 的 `init()` 原文：
```rust
match (sess.target.requires_lto, sess.early_lto()) {
    (true, _) | (false, LtoCli::Yes | LtoCli::Fat | LtoCli::NoParam | LtoCli::Thin) =>
        sess.dcx().fatal("LTO is not supported by rustc_codegen_cranelift"),
    (false, LtoCli::Unspecified | LtoCli::No) => {}
}
```
`driver/aot.rs` 的 `run_thin_lto()` 直接 `unreachable!()`。

其余障碍（即使 LTO 修好也各自致命）：
- **仍 nightly-only**：`-Z codegen-backend` tracking #77933 未稳定；本机 `rustc -Z ...` 实测报 `the option Z is only accepted on the nightly compiler`。**1.100（2026-11-12）发布说明无 cranelift 稳定化条目** —— 不是"落后几个版本"，是不在路线上。
- **`opt-level="z"` 语义被丢弃**：后端把 `Less|More|Size|SizeMin|Aggressive` 五个档**全折叠到 `speed_and_size`** ⇒ `"z"` 与 `3` 产物完全相同，**本项目"体积必须控住"的诉求得不到支持**。
- **`panic=abort` 默认开启**，README 至今把 "Unwinding on panics" 列在 Not yet supported ⇒ axum handler 一 panic 整个进程死，与当前 unwind 语义冲突。
- **2025h2「production-ready cranelift」目标（rust-lang/goals#397）2025-12-01 关闭，原因是没拿到经费**，负责人原话：*"the amount of time spent in codegen for realistic changes to real code bases was smaller than expected"* ⇒ 连收益本身都低于预期。
- ⚠️ 易误读：rust-lang/rust #163412「Apply LTO to Cranelift and GCC codegen backends」是**给 cg_clif 自己**编 LTO（让编译器跑快 ~0.6%），**不是**给用户代码开 LTO。只看标题会得出相反结论。

**翻案条件**：cg_clif 支持 `-Clto=fat` 且 stable 化，且 `--timings` 显示 codegen 占 paddock-api 编译 >40%。

### 4.8 sccache / kache / Hurry / 编译缓存类 [X]

**结构上不可能有收益：`bin` crate 不被缓存。**

sccache 官方 Rust 文档 2026 年一字未改：*"Crates that invoke the system linker cannot be cached. Examples are `bin`, `dylib`, `cdylib`, and `proc-macro` crates."*
机制：`lto=true` 下 cargo 对依赖 rlib 传 `-Clto=yes`（只产 bitcode，几乎零 codegen），**全部 LTO + 链接发生在最终 `crate-type=bin` 那次 rustc 调用**——恰是拒绝缓存的类型。

| 工具 | 2026 状态 | 否决理由 |
|---|---|---|
| sccache | **0.19.0（2026-10-24），维护活跃**；后端扩到 11 种 | bin crate 不缓存；且 `/build/target` 已复用全部依赖 rlib ⇒ 增量为零，反而每次 rustc 调用多 50~200ms 查表开销 |
| kache | v2.0.0，2026 新秀 | 同样默认跳过 bin crate（需 `KACHE_CACHE_EXECUTABLES=1`），上限相同 |
| Hurry | Apache-2.0，YC X25 | **硬阻塞**：README 明写只支持 **Rust ≤1.92**（依赖的 `--build-plan` 在 1.93 被移除），本项目用 1.97；且**不支持 first-party packages** ⇒ paddock-api 本身不在加速范围 |
| `Swatinem/rust-cache` | v2.9.3 | 只服务 GitHub Actions，本仓无 `.github/` |
| Cargo 内建共享缓存 | goals#5931 原型 | 2026-09 实测「30GB 里只省 0.5GB」，未落地 |

⚠️ 唯一受益场景「cache mount 丢了靠 sccache 救」**根本不成立**：`docker builder prune` 会**同时**清掉两者（见 §3.2）。
**实测反例**：release（lto=true+cgu=1）下引入 sccache **反慢 8%**（2m02s → 2m12s）。

### 4.9 替代构建工具链（Nix / Bazel / Buck2）[X]

全部否决。核心判据：**它们的卖点（跨机/跨仓/跨分支产物共享 + 远程执行）三个前提——多仓、CI、多分支——本项目一个都没有**，与 §4.8 否决 sccache 的判据逐字相同。

| 方案 | 2026 状态 | 否决理由 |
|---|---|---|
| Nix（crane 0.23.1 / naersk / fenix） | 三者都活跃 | 加速全来自**依赖产物复用**（cachix）⇒ 而本项目依赖三个月没重编，收益为零。且要换掉整条 `docker build → save → scp → load` 部署链 |
| Bazel + rules_rust 0.75.0（2026-10-26） | 成熟活跃 | 对单成员 workspace 输入配置量是现有 Dockerfile 十倍以上；远程缓存同 sccache 同构无效。⚠️ 另有质量风险：rules_rust#3407 报告 Bazel 与 Cargo 构建的同一二进制**性能差 ~60%**（`-Ldependency` 传参差异致 LLVM 优化不生效） |
| Buck2 | 仓库活跃（prelude/rust 2026-11-17 仍有提交） | 官方 "Why Buck2" 明确列出未完成项：**「There are not yet mechanisms to build in release mode」**，需自行改 toolchain；toolchains 未开源、开放版无远程执行 |

**唯一翻案条件**：将来要做 aarch64 多架构时，`crane#cross-musl` 或 `cargo-zigbuild` 才有价值。

## 5. 待验证

**已结案的两条（2026-11-17）**：
- [V] **`run_linker` 实际秒数 = 0.406s**（0.6%）。换链接器方向**彻底否决**，见 §4.4 / §1.7。
- [V] **全量 fat LTO 内存峰值 = used 4.82GB / available 5.13GB / swap 未用**。10GB 上限下余量 5GB，安全。见 §1.5。

**仍未验证**：
- [?] **`-Cembed-metadata` 的实际收益（应改标为磁盘优化，不是编译提速）**。⚠️ **定性修正 + 两道闸门**：
  - **rustc 侧已合并**：PR [rust-lang/rust#163436](https://github.com/rust-lang/rust/pull/163436) 2026-10-19 merged，milestone **1.101.0**（2026-12-24 发布）。stable flag 名 = **`-Cembed-metadata=no`**（`-Zembed-metadata` 是迁移期别名，未来会删）。本机 1.97.1 实测报 `unknown codegen option`。
  - ⚠️ **cargo 侧仍未合并**：PR [rust-lang/cargo#17533](https://github.com/rust-lang/cargo/pull/17533) **仍 open**（2026-09-29 创建）。1.101-beta 的 Cargo CHANGELOG 无该条目，nightly cargo 的 `unstable.html` 仍把它列在 unstable。
  - ⇒ **即使 1.101 发布也不能零配置生效**：cargo 不知道这个 flag 就不会给 bin/cdylib 补 `--extern foo.rmeta`，链接期报 `only metadata stub found`（Bazel/evcxr 等外部构建工具踩过的坑）。**要等 #17533 合并**。
  - **收益是磁盘，不是时间**：作者原话 *"I haven't noticed any measurable effect (either positive or negative) on compile times though, at least on Linux. The main motivation is disk size, not compilation speed."*
  - **本项目实测占比**（2026-11-17）：`target/release` 755MB，324 个 `.rlib` 共 418MB，其中内嵌 `lib.rmeta` **271MB = 64.8%**；独立 `.rmeta` 260MB 不受影响。⇒ 预期 release target 缩 **≈36%（~271MB）**，**最终二进制 sha256 完全相同**。
  - **对 paddock 的实际价值 ≈ 0**：Docker 用 cache mount，**不写回镜像层** ⇒ **镜像体积零变化**，只省 VPS 缓存目录磁盘。日常改一行只编 1 个 unit，结构上无作用面。
  - 临时路径（本项目实测通过）：`RUSTC_BOOTSTRAP=1 cargo -Zno-embed-metadata build --release`，构建成功、二进制可运行、产物字节相同、target 38MB→26MB。⚠️ 但 `RUSTC_BOOTSTRAP` 是绕过稳定性闸门的 hack，**不建议进生产 Dockerfile**。
- [?] **工具链升级：两者都不为编译速度而升**。逐版本核实（2026-11-17，全部一手源）：
  | 版本 | 日期 | 对编译速度的影响 |
  |---|---|---|
  | 1.98.0 | 2026-08-20 | 零（Internal 段只有 query 路径微调，无量化数字） |
  | 1.99.0 | 2026-10-01 | LLVM 23，历史同类收益 1~2%（被 LTO 噪声淹没）。⚠️ 网传"1.99 提速 40%"是**标题党**——那是 `cargo doc` 的 trait impl 过滤，不是 `cargo build` |
  | 1.100.0 | 2026-11-12 | **零编译收益**。build-dir layout v2 官方定位是 "unblocking future improvements"，只对**多 features / 跨 worktree 共享 build-dir** 有效；本项目单 workspace 单 features 全不适用。代价确定：一次性全量重编 + crater 回归 rust-lang/rust#164098（once_cell/ripgrep 等 `No such file or directory`） |
  | 1.101.0 | 2026-12-24（未发） | 只有 `-Cembed-metadata` 稳定，且如上无编译收益；另有 `build.profile` 配置项 |
  ⇒ **当前 1.97.1 无需升级。** 若将来要升，等 1.101 过后一次性跳，并接受一次全量重编（本地 `target/` + Docker cache mount 里的 `/build/target` 都要重建）。
- [?] **`codegen-units = 1 → 16` 的真实收益**。依据 cargo#14719（cargo 自身 −19% 时间 / +3.6% 体积），但**本项目 codegen_crate 只占 71s 里的 5.155s（7.3%）** ⇒ 预期只有 **4~5%**，不是 19%。⚠️ 不要把 §2.1 的"thin+cgu16 比 fat+cgu1 慢 16s"套到 `fat+cgu16` 上——那是完全不同配置。**待实测**。
- [?] **`-Zfat-lto-partitions=N`**（compiler-team #1023，nightly-only，2026-10 实测 16 线程省 33% 墙钟）——这是**唯一能真正打破 fat LTO 单核串行**的上游提案。但本机 `rustc -Z help` **实测不存在该 flag**，且 issue 已 Closed，落地前景不明。stable 用户拿不到。

## 6. 参考资料勘误（第三方 AI 提供的"Rust 编译加速方案"）

19 条断言逐条核实结果：11 条属实、4 条部分属实、**2 条编造**。

| 编造/误用 | 真相 |
|---|---|
| `CARGO_PROFILE_DEV_PACKAGE___DEBUG=false` | **编造**。官方对 `[profile.<name>.package.<name>]` 标注 **`Environment: not supported`**；package 级覆盖**无环境变量通道** |
| `[profile.dev.package."*"]` | 官方写法是**空串** `""`，`"*"` 不在优先级表 |
| `-Zthreads=N` | ⚠️ **对 1.97.1 而言它仍是唯一可用形式**（`--jobs-frontend=N` 是 1.99 才加入的）。且实测 `RUSTC_BOOTSTRAP=1` **单独够用**，不需要 `-Zunstable-options`；到 1.100 才变成两者都要。旧表述「2023 年旧名」过于绝对 |
| `split-debuginfo="unpacked"` | macOS 语境误用。**Linux 默认已是 `off`**，设了体积只增不减 |

**事实对但推荐被抬高**：`-Zembed-metadata`（官方明说"不打算稳定"）、`cargo-reclaim`（2026-07 才 0.5.0，且 `kill_active_builds=true` 会杀 cargo 进程）、`cargo-sweep`（已缺维护者）。
