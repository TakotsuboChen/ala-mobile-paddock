//! 积分公式 + 辅助配置派生的**单一事实源**（Rust 侧）。
//!
//! ⚠️ 本文件的公式与 `migrations/0009_lap_assist_config.sql` 的派生规则、
//!    以及 `api/leaderboard.rs` 的两条 SQL（版本榜 / 总榜）、
//!    `api/auth_handlers.rs` 的 `total_points`、`admin.rs` 的 `fetch_laps`
//!    中的加成 SQL **必须逐字对应**。任何一处改动都要把所有副本一起改——
//!    本模块存在的意义就是让"参与运算的那些副本"不再各写一遍字符串拼接，
//!    而未参与运算的副本（`total_points` 的独立查询）仍需手工同步。
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
        self.pedal_mode = keep(self.pedal_mode, &["off", "single", "dual"]);
        self.tc_mode = keep(self.tc_mode, &["default", "custom"]);
        self.tc_strength = keep(self.tc_strength, &["off", "weak", "medium", "strong", "stock"]);
        self.abs_mode = keep(self.abs_mode, &["default", "custom"]);
        self.abs_strength = keep(self.abs_strength, &["off", "weak", "medium", "strong", "stock"]);
        self
    }
}

impl From<&AssistConfig> for AssistFlags {
    /// ⚠️ 关 TC / 关 ABS **必须**同时满足 `mode == "custom"`——
    /// `mode == "default"` 表示原生透传，此时 `strength` 字段只是 UI 记忆值、
    /// 不生效，读它会把"从未调过 TC"的用户误判成"关 TC"（加 10% 分）。
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