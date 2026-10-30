//! 积分公式 + 辅助配置派生的**单一事实源**（Rust 侧）。
//!
//! ⚠️ 本文件的公式与 `migrations/0009_lap_assist_config.sql` 的派生规则、
//!    以及 `api/leaderboard.rs` 的两条 SQL（版本榜 / 总榜）、
//!    `api/auth_handlers.rs` 的 `total_points`、`admin.rs` 的 `fetch_laps`
//!    中的加成 SQL **必须逐字对应**。任何一处改动都要把所有副本一起改——
//!    本模块存在的意义就是让"参与运算的那些副本"不再各写一遍字符串拼接，
//!    而未参与运算的副本（`total_points` 的独立查询）仍需手工同步。
//!
//! 2026-10-19 进一步收口：窗口定义抽成 [`scored_cte`]，积分聚合抽成
//! [`user_points_and_rank`]。榜单 / `/v1/me` / 车手主页现在共用同一份 CTE，
//! 不再各拼一遍 `ranked` + `scored`。
//!
//! ## 积分 = 名次基础分 + 辅助加成
//!
//! 基础分（v40 定案，2026-09-04）：
//!   `base = round((N+1−rank)×100/N)`：第一 100、第 N 名 100/N、虚位第 N+1 名 0；
//!   N=1 自然得 100，无需特判。
//!
//! 加成（2026-09-18 定案）：
//!   线性踏板 +10% / 关 TC +10% / 低档 ABS +10% / 关 ABS +30%，最大 50%。
//!   `bonus = if base == 0 { 0 } else { max(1, round(base × pct)) }`
//!   —— base=0（虚位无分）时加成恒 0，否则不足 1 保底 1。
//!   注意 pct 的**相乘对偶**：`base × (1 + pct)` 与 `base + base × pct` 在
//!   实数域相同，但取整时机不同（前者先乘后舍一次，后者两次舍入）——本实现
//!   按"先算加成再舍入、再相加"（用户描述的顺序），SQL 副本必须同序。

use serde::Deserialize;

/// 该维「无效」的哨兵 token（2026-10-30）。
///
/// 语义 = 配置拿到了，但**这一圈中途变过**，无法确定整圈用的是哪档 ⇒
/// 不可作为成绩属性采信（不加分、不参与金标）。
///
/// ⚠️ 与 `None`（缺失 = 根本没拿到）**必须区分**：折成缺失会让用户看不出是
///    自己中途改了配置。模块侧同值见 `ModConfig.LapAssistCodes.INVALID_TOKEN`。
pub const INVALID_TOKEN: &str = "invalid";

/// 辅助配置四维（缺失 = None）。管理端补录与模块上报共用同一套枚举字符串。
///
/// `Default` 全 None = 缺失——旧模块不传字段时 serde 自然落到这里。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssistConfig {
    pub pedal_mode: Option<String>,
    pub tc_mode: Option<String>,
    pub tc_strength: Option<String>,
    pub abs_mode: Option<String>,
    pub abs_strength: Option<String>,
}

/// 由原始配置派生的四个布尔位（缺失一律 false = 不加分）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AssistFlags {
    pub pedal_linear: bool,
    pub tc_off: bool,
    pub abs_off: bool,
    pub abs_low: bool,
}

/// 一圈的车型 + 车辆调教（2026-10-26）。全部 `Option` = 缺失。
///
/// **车型只存整数**（`car_id` = `CarDefinition.carId`，`car_era` = `CarType`），
/// 展示名「2026 梅赛德斯-AMG W17」由模块侧 `CarCatalog` 反查渲染——改车队译名
/// 只发模块，服务端永远不需要认识车型字。
///
/// **调教存的是 UI 显示值本身**（下压力 65.0 不是 0.65，外倾角 -3.0 度不是
/// 弧度）。服务端不做任何换算，只在展示时格式化。
///
/// ⚠️ `setup_brake_bias` 只有**前**值，后值 = 100 − 前。游戏 UI 的 `60 / 40`
/// 是派生量——不要加 rear 列，那会出现两个真值源。
///
/// ⚠️ 整组调教是**全有或全无**语义：模块侧用 `has_setup` 判定，解析侧
/// [`Self::normalized`] 会拒绝半套（缺任一字段 → 整组落 None）。半套调教
/// 比缺失更误导（看起来有数据，实际不全）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CarSetupConfig {
    pub car_id: Option<i32>,
    pub car_era: Option<i32>,
    pub setup_front_wing_aero: Option<f64>,
    pub setup_rear_wing_aero: Option<f64>,
    pub setup_front_ride_height: Option<f64>,
    pub setup_rear_ride_height: Option<f64>,
    pub setup_brake_bias: Option<f64>,
    pub setup_front_stiffness: Option<f64>,
    pub setup_rear_stiffness: Option<f64>,
    pub setup_front_rollbar: Option<f64>,
    pub setup_rear_rollbar: Option<f64>,
    pub setup_front_camber: Option<f64>,
    pub setup_rear_camber: Option<f64>,
    pub setup_front_toe: Option<f64>,
    pub setup_rear_toe: Option<f64>,
}

impl CarSetupConfig {
    /// 归一化：车型越界落 None；调教半套 → 整组落 None。
    ///
    /// 与 [`AssistConfig::normalized`] 同一动机——宁可记缺失也不丢圈，
    /// 同时堵住客户端塞入无意义数据污染数据面。
    pub fn normalized(self) -> Self {
        // 车型区间与模块侧 CarCatalog 一致：2023 地效 0..19、2026 主动空力 20..41。
        // era 只接受 0/1（CarType 枚举）。任一不合法 → 两个都落 None
        // （半个车型没有意义，服务端无法反查展示名）。
        let (car_id, car_era) = match (self.car_id, self.car_era) {
            (Some(id), Some(era)) if (0..=41).contains(&id) && (0..=1).contains(&era) => {
                (Some(id), Some(era))
            }
            _ => (None, None),
        };

        // 调教整组：13 个全 Some 才保留，否则整组 None。
        let setup = [
            self.setup_front_wing_aero,
            self.setup_rear_wing_aero,
            self.setup_front_ride_height,
            self.setup_rear_ride_height,
            self.setup_brake_bias,
            self.setup_front_stiffness,
            self.setup_rear_stiffness,
            self.setup_front_rollbar,
            self.setup_rear_rollbar,
            self.setup_front_camber,
            self.setup_rear_camber,
            self.setup_front_toe,
            self.setup_rear_toe,
        ];
        let complete = setup.iter().all(|v| v.is_some());
        let keep = |v: Option<f64>| if complete { v } else { None };

        CarSetupConfig {
            car_id,
            car_era,
            setup_front_wing_aero: keep(self.setup_front_wing_aero),
            setup_rear_wing_aero: keep(self.setup_rear_wing_aero),
            setup_front_ride_height: keep(self.setup_front_ride_height),
            setup_rear_ride_height: keep(self.setup_rear_ride_height),
            setup_brake_bias: keep(self.setup_brake_bias),
            setup_front_stiffness: keep(self.setup_front_stiffness),
            setup_rear_stiffness: keep(self.setup_rear_stiffness),
            setup_front_rollbar: keep(self.setup_front_rollbar),
            setup_rear_rollbar: keep(self.setup_rear_rollbar),
            setup_front_camber: keep(self.setup_front_camber),
            setup_rear_camber: keep(self.setup_rear_camber),
            setup_front_toe: keep(self.setup_front_toe),
            setup_rear_toe: keep(self.setup_rear_toe),
        }
    }
}

/// 枚举白名单：写入 `laps` 前归一化，未知值一律落 None（= 缺失）。
///
/// 为什么不在反序列化层拒绝：旧模块可能带未来版本的枚举值（前向兼容），
/// 报错会让整条圈丢失——宁可存成"缺失"，也不丢圈（防伪=全放行的一贯口径）。
/// 同时堵住管理端/客户端往里塞任意字符串污染数据面。
impl AssistConfig {
    pub fn normalized(mut self) -> Self {
        fn keep(v: Option<String>, allowed: &[&str]) -> Option<String> {
            v.filter(|s| allowed.contains(&s.as_str()))
        }
        // `"invalid"` = 该维在本圈中途变过配置（模块判定，2026-10-30）。
        // ⚠️ 必须进白名单，否则会被归一化成 None（= 缺失）——那就退回到
        //    "用户看不出是自己改的配置"的老问题。
        self.pedal_mode = keep(self.pedal_mode, &["off", "single", "dual", INVALID_TOKEN]);
        self.tc_mode = keep(self.tc_mode, &["default", "custom", INVALID_TOKEN]);
        self.tc_strength = keep(
            self.tc_strength,
            &["off", "weak", "medium", "strong", "stock", INVALID_TOKEN],
        );
        self.abs_mode = keep(self.abs_mode, &["default", "custom", INVALID_TOKEN]);
        self.abs_strength = keep(
            self.abs_strength,
            &["off", "weak", "medium", "strong", "stock", INVALID_TOKEN],
        );
        self
    }
}

impl From<&AssistConfig> for AssistFlags {
    /// ⚠️ 关 TC / 关 ABS **必须**同时满足 `mode == "custom"`——
    /// `mode == "default"` 表示原生透传，此时 `strength` 字段只是 UI 记忆值、
    /// 不生效，读它会把"从未调过 TC"的用户误判成"关 TC"（加 10% 分）。
    ///
    /// ⚠️ `"invalid"` 维一律**不加分**（2026-10-30）：该维在本圈中途变过配置，
    /// 等于竞赛条件被改变过，不能据此享受加成。它也不是"关"——所以下面每个
    /// 判据都要求值**精确等于**目标 token，`"invalid"` 天然落空。
    fn from(c: &AssistConfig) -> Self {
        let custom = |m: &Option<String>| m.as_deref() == Some("custom");
        let is = |s: &Option<String>, v: &str| s.as_deref() == Some(v);
        AssistFlags {
            // 原生按键（'off'）不算线性踏板：一按就是 100% 油门/刹车。
            pedal_linear: matches!(c.pedal_mode.as_deref(), Some("single") | Some("dual")),
            tc_off: custom(&c.tc_mode) && is(&c.tc_strength, "off"),
            abs_off: custom(&c.abs_mode) && is(&c.abs_strength, "off"),
            abs_low: custom(&c.abs_mode) && is(&c.abs_strength, "weak"),
        }
    }
}

impl AssistFlags {
    /// 加成比例（0.0 ~ 0.5）。仅当 base > 0 时才有意义。
    pub fn bonus_pct(&self) -> f64 {
        let mut p = 0.0;
        if self.pedal_linear {
            p += 0.10;
        }
        if self.tc_off {
            p += 0.10;
        }
        if self.abs_off {
            p += 0.30;
        } else if self.abs_low {
            p += 0.10;
        }
        p
    }

    /// 基础分的加成值（整数）。
    ///
    /// ️ 两道零门：`base == 0`（虚位无分）→ 0；`pct == 0`（四项全不达标）→ 0。
    /// 第二道门不可省——缺了它，"完全没达标"会走"不足 1 保底 1"分支，**人人白送 1 分**。
    /// 保底规则的本意只针对"达标了但加成不足 1 分"的场景（如 base=2、pct=0.1 → 0.2 → 1）。
    pub fn bonus_points(&self, base: i64) -> i64 {
        let pct = self.bonus_pct();
        if base <= 0 || pct <= 0.0 {
            return 0;
        }
        let raw = base as f64 * pct;
        if raw < 1.0 {
            1
        } else {
            raw.round() as i64
        }
    }

    /// 四项全达标 = 满额加成 +50%（管理端/文档展示用）。
    #[allow(dead_code)]
    pub fn is_max(&self) -> bool {
        self.pedal_linear && self.tc_off && self.abs_off
    }

    /// 零辅助 = TC 与 ABS 均为"关"（用户定案：**踏板不参与**此判据——
    /// 原生按键物理上跑不出有效圈，实际满足者必然在用模块踏板）。
    /// ⚠️ 榜单金标走 SQL 里的 `tc_off AND abs_off`，本函数是同语义的 Rust 对照。
    #[allow(dead_code)] // 见 base_points 注释（测试锚定 + 语义文档）
    pub fn is_zero_assist(&self) -> bool {
        self.tc_off && self.abs_off
    }
}

/// 名次基础分（v40）。`rank` / `n` 均为 1-based 名次与参赛人数。
/// N=1 → 100；rank = N+1（虚位）→ 0。
///
/// ⚠️ 运行时的实际计算走 [`base_sql`] 生成的 SQL（避免逐行回传 PostgreSQL 再算）；
/// 本函数是**同语义的 Rust 对照实现**，供单元测试锚定公式——两边行为不一致
/// 时测试会红，是防止"SQL 片段被改坏却没人发现"的唯一防线。
#[allow(dead_code)] // 见上：测试与文档用，非运行时路径
pub fn base_points(rank: i64, n: i64) -> i64 {
    if n <= 1 {
        return 100;
    }
    (((n + 1 - rank) as f64) * 100.0 / n as f64).round() as i64
}

/// 名次基础分 + 辅助加成。`flags = None`（缺失）时退化为基础分。
#[allow(dead_code)] // 见 base_points 注释
pub fn total_points(rank: i64, n: i64, flags: Option<AssistFlags>) -> i64 {
    let base = base_points(rank, n);
    base + flags.map_or(0, |f| f.bonus_points(base))
}

/// 参与积分/榜单运算的 SQL 片段（`base` 与 `bonus` 各一段，供 CTE 内联）。
///
/// `rank_expr` / `n_expr` 由调用方提供，因为版本榜与总榜的窗口分区不同
///（前者 `PARTITION BY gp_index`，后者 `PARTITION BY version_code, gp_index`）。
/// `bl` 为 best_laps 的别名（两处均为显式别名，无歧义）。
///
/// ️ 返回的 SQL 里 `base` 是 numeric（round 的返回类型）、加成也是 numeric；
/// 调用方负责 `::bigint` 收口，避免整数除法陷阱。
pub fn base_sql(rank_expr: &str, n_expr: &str) -> String {
    format!(
        "(CASE WHEN {n_expr} = 1 THEN 100 \
           ELSE round(({n_expr} + 1 - {rank_expr})::numeric * 100 / {n_expr}) \
         END)"
    )
}

/// 加成 SQL 片段。与 [`AssistFlags::bonus_points`] 同语义：
/// base=0 → 0；pct=0 → 0（**不做保底**，否则人人白送 1 分）；否则不足 1 保底 1、四舍五入。
pub fn bonus_sql(base_expr: &str, bl: &str) -> String {
    let pct = format!(
        "(CASE WHEN {bl}.pedal_linear THEN 0.10 ELSE 0 END) + \
         (CASE WHEN {bl}.tc_off       THEN 0.10 ELSE 0 END) + \
         (CASE WHEN {bl}.abs_off      THEN 0.30 \
               WHEN {bl}.abs_low      THEN 0.10 ELSE 0 END)"
    );
    format!(
        "CASE WHEN ({base_expr}) <= 0 OR ({pct}) <= 0 THEN 0 \
           ELSE GREATEST(1, round(({base_expr}) * ({pct}))) \
         END"
    )
}

/// 全量车手积分聚合的 CTE 前半段（`ranked` + `scored` 两层）。
///
/// 榜单、`/v1/me` 的总积分、车手主页的"总积分/总排名"三处**共用这一段**——
/// 它们的差别只有窗口分区与是否过滤版本，而分区与过滤是同一件事：
/// 版本榜 `WHERE version_code = $1` 之后 `version_code` 恒定，
/// `PARTITION BY version_code, gp_index` 与 `PARTITION BY gp_index` 等价，
/// 故**统一按总榜分区写**，版本榜只靠 WHERE 收窄。这样三处只有一份窗口定义。
///
/// 调用方负责在外面接 `agg`（按 user 分组求和）并追加自己的过滤/排序。
/// 返回的 SQL 以 `scored` CTE 结尾，不含末尾逗号。
pub fn scored_cte(version_filter: &str) -> String {
    let base = base_sql("rank_in_track", "n_in_track");
    format!(
        "WITH ranked AS ( \
           SELECT bl.user_id, bl.tc_off, bl.abs_off, bl.pedal_linear, bl.abs_low, \
                  rank() OVER (PARTITION BY bl.version_code, bl.gp_index \
                               ORDER BY bl.lap_ms ASC) AS rank_in_track, \
                  count(*) OVER (PARTITION BY bl.version_code, bl.gp_index) AS n_in_track \
           FROM best_laps bl {version_filter} \
         ), \
         scored AS ( \
           SELECT user_id, tc_off, abs_off, pedal_linear, abs_low, {base} AS base_pts FROM ranked \
         )"
    )
}

/// 单用户积分 + 名次（`/v1/me`、车手主页共用）。
///
/// 名次用 `row_number()` 而非 `rank()`：**必须与积分榜的展示顺序逐行对齐**——
/// 榜单是 `ORDER BY points DESC, username ASC` 再按行号显示名次，`rank()` 遇到
/// 同分并列会跳号（1,1,3），主页就会显示"第 2 名"而榜单里他在第 3 行。
/// `row_number()` 保证主页名次 = 该用户在榜单里的行号。
///
/// `version = None` → 总榜口径（各版本独立计分累加）；`Some(v)` → 该版本口径。
/// 返回 `(points, rank)`；该用户完全没成绩时 rank 为 `None`（积分恒 0）。
pub async fn user_points_and_rank(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    version: Option<i32>,
) -> Result<(i64, Option<i64>), sqlx::Error> {
    let (vf, bind_version) = match version {
        Some(_) => ("WHERE bl.version_code = $1", true),
        None => ("", false),
    };
    let bonus = bonus_sql("s.base_pts", "s");
    // 名次在全量聚合之后算：`row_number() OVER (ORDER BY points DESC, username ASC)`
    // 与榜单 SQL 的 ORDER BY 逐字一致（含同分时按用户名定序的兜底）。
    let sql = format!(
        "{cte}, \
         agg AS ( \
           SELECT u.id AS user_id, u.username AS username, \
                  sum(s.base_pts + {bonus})::bigint AS points \
           FROM scored s JOIN users u ON u.id = s.user_id \
           GROUP BY u.id, u.username \
         ), \
         ranked_users AS ( \
           SELECT user_id, points, \
                  row_number() OVER (ORDER BY points DESC, username ASC)::bigint AS \"rank\" \
           FROM agg \
         ) \
         SELECT points, \"rank\" FROM ranked_users WHERE user_id = ${ph}",
        cte = scored_cte(vf),
        ph = if bind_version { 2 } else { 1 },
    );
    let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
    if let Some(v) = version {
        q = q.bind(v);
    }
    let row = q.bind(user_id).fetch_optional(pool).await?;
    // 无成绩 = 不在 agg 里（best_laps 无行）→ 积分 0、无名次。
    Ok(row.map(|(p, r)| (p, Some(r))).unwrap_or((0, None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pedal: &str, tcm: &str, tcs: &str, absm: &str, abss: &str) -> AssistConfig {
        AssistConfig {
            pedal_mode: if pedal.is_empty() { None } else { Some(pedal.into()) },
            tc_mode: if tcm.is_empty() { None } else { Some(tcm.into()) },
            tc_strength: if tcs.is_empty() { None } else { Some(tcs.into()) },
            abs_mode: if absm.is_empty() { None } else { Some(absm.into()) },
            abs_strength: if abss.is_empty() { None } else { Some(abss.into()) },
        }
    }

    fn flags_of(c: AssistConfig) -> AssistFlags {
        AssistFlags::from(&c.normalized())
    }

    /// 基础分：v40 定案（N=1 特判 100；虚位第 N+1 名 0）。
    #[test]
    fn base_points_matches_v40() {
        assert_eq!(base_points(1, 1), 100);
        // 2 人榜：100 / 50 / 虚位 0
        assert_eq!(base_points(1, 2), 100);
        assert_eq!(base_points(2, 2), 50);
        assert_eq!(base_points(3, 2), 0);
        // 4 人榜：100 / 75 / 50 / 25 / 虚位 0
        assert_eq!(
            (1..=5).map(|r| base_points(r, 4)).collect::<Vec<_>>(),
            vec![100, 75, 50, 25, 0]
        );
        // 205 人榜：第一名 100、最后一名 round(1*100/205)=0
        assert_eq!(base_points(1, 205), 100);
        assert_eq!(base_points(205, 205), 0);
    }

    /// 四项全达标 = +50%；缺一项按各自权重。
    #[test]
    fn bonus_pct_by_items() {
        let none = flags_of(cfg("off", "default", "stock", "default", "stock"));
        assert_eq!(none.bonus_pct(), 0.0);
        let pedal = flags_of(cfg("dual", "default", "stock", "default", "stock"));
        assert_eq!(pedal.bonus_pct(), 0.10);
        let pedal_tc = flags_of(cfg("dual", "custom", "off", "default", "stock"));
        assert_eq!(pedal_tc.bonus_pct(), 0.20);
        let abs_low = flags_of(cfg("off", "default", "stock", "custom", "weak"));
        assert_eq!(abs_low.bonus_pct(), 0.10);
        let full = flags_of(cfg("dual", "custom", "off", "custom", "off"));
        assert_eq!(full.bonus_pct(), 0.50);
        assert!(full.is_max());
    }

    /// 未开自定义的档位是 UI 记忆值 → **绝不算"关"**（否则每个从没调过 TC 的用户白拿 10%）。
    #[test]
    fn default_mode_never_counts_as_off() {
        let f = flags_of(cfg("", "default", "off", "default", "off"));
        assert!(!f.tc_off && !f.abs_off && !f.abs_low);
        assert_eq!(f.bonus_pct(), 0.0);
    }

    /// 全缺失（旧记录 / 旧模块）= 不加分、不金标。
    #[test]
    fn missing_config_grants_nothing() {
        let f = flags_of(cfg("", "", "", "", ""));
        assert_eq!(f.bonus_pct(), 0.0);
        assert!(!f.is_zero_assist());
        assert!(!f.pedal_linear);
    }

    /// 未知枚举值被白名单挡成"缺失"（前向兼容：宁可记缺失也不丢圈）。
    #[test]
    fn unknown_enum_normalizes_to_missing() {
        let f = flags_of(cfg("quad", "custom", "ultra", "custom", "off"));
        assert!(!f.pedal_linear);
        assert!(!f.tc_off);
        assert!(f.abs_off); // 同一份配置里合法维度仍生效
    }

    /// 取整三条边界：base=0 无加成、pct=0 无保底、不足 1 保底 1。
    #[test]
    fn bonus_rounding_edges() {
        let f = flags_of(cfg("dual", "default", "stock", "default", "stock")); // pct=0.10
        assert_eq!(f.bonus_points(0), 0, "虚位无分不给加成");
        assert_eq!(f.bonus_points(2), 1, "0.2 → 保底 1");
        assert_eq!(f.bonus_points(5), 1, "0.5 → 保底 1");
        assert_eq!(f.bonus_points(10), 1, "1.0 → 1");
        assert_eq!(f.bonus_points(15), 2, "1.5 → 四舍五入 2");
        assert_eq!(f.bonus_points(100), 10);

        let zero = flags_of(cfg("", "", "", "", ""));
        assert_eq!(zero.bonus_points(100), 0, "四项全不达标 → 0，不走保底");
    }

    /// 零辅助金标判据只含 TC/ABS（用户定案：原生按键物理上跑不出有效圈）。
    #[test]
    fn zero_assist_ignores_pedal() {
        assert!(flags_of(cfg("off", "custom", "off", "custom", "off")).is_zero_assist());
        assert!(flags_of(cfg("dual", "custom", "off", "custom", "off")).is_zero_assist());
        assert!(!flags_of(cfg("dual", "custom", "off", "custom", "stock")).is_zero_assist());
    }

    /// `"invalid"` 维：不加分、不给金标，但也**不是**"关"。
    ///
    /// 用户定案 2026-10-30：invalid = 本圈中途改过配置 ⇒ 竞赛条件被改变过，
    /// 不能据此享受加成；但它也不代表用户"关掉了辅助"，所以不能算零辅助。
    #[test]
    fn invalid_dimension_grants_nothing() {
        // 只有 ABS 维 invalid，其余正常：ABS 的两个加分项都不触发。
        let f = flags_of(cfg("dual", "custom", "off", "invalid", "invalid"));
        assert!(f.pedal_linear, "踏板维正常，照常给线性踏板加成");
        assert!(f.tc_off, "TC 维正常，照常算关 TC");
        assert!(!f.abs_off, "ABS 维 invalid 不算关");
        assert!(!f.abs_low, "ABS 维 invalid 也不算低档");
        assert_eq!(f.bonus_pct(), 0.20); // 踏板 0.10 + TC 0.10
        assert!(!f.is_zero_assist(), "ABS 维 invalid ⇒ 不是零辅助，不给金标");

        // 五维全 invalid：什么分都没有。
        let all = flags_of(cfg("invalid", "invalid", "invalid", "invalid", "invalid"));
        assert_eq!(all.bonus_pct(), 0.0);
        assert!(!all.is_zero_assist());
    }

    /// `"invalid"` 必须能通过 `normalized()` 白名单（否则会被归一成缺失）。
    #[test]
    fn invalid_token_survives_normalization() {
        let n = cfg("invalid", "invalid", "invalid", "invalid", "invalid").normalized();
        assert_eq!(n.pedal_mode.as_deref(), Some("invalid"));
        assert_eq!(n.tc_mode.as_deref(), Some("invalid"));
        assert_eq!(n.tc_strength.as_deref(), Some("invalid"));
        assert_eq!(n.abs_mode.as_deref(), Some("invalid"));
        assert_eq!(n.abs_strength.as_deref(), Some("invalid"));
    }

    /// 总分 = 基础分 + 加成，且加成基于**基础分**而非总分。
    #[test]
    fn total_is_base_plus_bonus() {
        let full = flags_of(cfg("dual", "custom", "off", "custom", "off"));
        assert_eq!(total_points(1, 2, Some(full)), 150); // 100 + 50
        assert_eq!(total_points(2, 2, Some(full)), 75); // 50 + 25
        assert_eq!(total_points(3, 2, Some(full)), 0); // 虚位仍 0
        assert_eq!(total_points(1, 1, Some(full)), 150);
    }
}