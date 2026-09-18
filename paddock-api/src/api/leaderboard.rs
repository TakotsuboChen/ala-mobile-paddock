//! 排行榜查询：积分总榜/版本榜 + 赛道榜。
//!
//! ## 积分公式（v40 基础分 + 2026-09-18 辅助加成）
//!
//! `total = base + bonus`，其中 base/bonus 的定义与 SQL 片段**全部**在
//! [`crate::score`]（单一事实源）。本文件只负责窗口分区与聚合：
//! - 版本榜：`PARTITION BY gp_index`（该版本内每赛道独立排名）
//! - 总榜：`PARTITION BY version_code, gp_index`——**版本 = 独立赛季**，
//!   同一用户在 8.0.4 与 8.0.6 各拿全赛道第一 = 每赛道两笔满分，累加（不跨版本取最快）。
//!
//! ## 零辅助金标（2026-09-18 定案）
//!
//! 判据用**比值**而非绝对数，故新旧版本记录并存时口径自洽：
//!   版本榜：`2 × 零辅助最快圈数 ≥ 最快圈总数`
//!   总　榜：上式 **且** 零辅助最快圈数 > 0
//! 分母是该版本（或全部版本）内有成绩的赛道数，不是"全服所有记录"——
//! 即"个人最快圈里有多少比例是零辅助"。总榜与版本榜的金标互不蕴含
//! （单版本可 100% 零辅助而总榜被老版本记录拉到 50% 以下）。

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{api::auth_handlers::ApiError, api::laps::track_display_name, score, state::AppState};

#[derive(Deserialize)]
pub struct VersionFilter {
    pub version: Option<i32>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct PointsEntry {
    pub user_id: Uuid,
    pub reg_seq: i64,
    pub username: String,
    pub avatar_url: Option<String>,
    pub points: i64,
    /// 该用户在该榜维度内有成绩的赛道数（金标分母，客户端可用于展示"x/y"）
    pub best_count: i64,
    /// 其中零辅助最快圈的条数（金标分子）
    pub zero_count: i64,
    /// 是否显示金色流光（服务端判定的唯一事实源，客户端不重算）
    pub gold: bool,
}

#[derive(Serialize)]
pub struct PointsBoard {
    pub version: Option<i32>,
    pub entries: Vec<PointsEntry>,
}

/// GET /v1/leaderboard/points?version=200146
/// 无 version 参数 = 总榜（跨版本累加）。
pub async fn points_board(
    State(state): State<AppState>,
    Query(f): Query<VersionFilter>,
) -> Result<Json<PointsBoard>, ApiError> {
    // 两个榜只差窗口分区与金标的存在性条件，其余 SQL 由 score 模块的片段拼出。
    let (partition, gold_expr) = match f.version {
        Some(_) => ("bl.gp_index", "2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)"),
        None => (
            "bl.version_code, bl.gp_index",
            "count(*) FILTER (WHERE s.tc_off AND s.abs_off) > 0 \
             AND 2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)",
        ),
    };
    let base = score::base_sql("rank_in_track", "n_in_track");
    let bonus = score::bonus_sql("s.base_pts", "s");

    // 层级说明：Postgres 不允许在同一 SELECT 列表里引用窗口函数别名，
    // 也不允许引用同层的普通别名 → 窗口(ranked) → 基础分(scored) → 加成(最外层) 三层。
    // ⚠️ `scored` 必须把 bonus 用到的**全部四个派生位列**一起带下来（pedal_linear /
    //    tc_off / abs_off / abs_low）——漏一个就是运行期 `column s.x does not exist`
    //    500 错误。Rust 侧 bonus_points() 读的是结构体字段（天然齐全），
    //    单元测试发现不了这类"SQL 管线漏列"，只有真库能测（见 score_sql_tests.rs）。
    let sql = format!(
        "WITH ranked AS ( \
           SELECT bl.user_id, bl.tc_off, bl.abs_off, bl.pedal_linear, bl.abs_low, \
                  rank() OVER (PARTITION BY {partition} ORDER BY bl.lap_ms ASC) AS rank_in_track, \
                  count(*) OVER (PARTITION BY {partition}) AS n_in_track \
           FROM best_laps bl {version_filter} \
         ), \
         scored AS ( \
           SELECT user_id, tc_off, abs_off, pedal_linear, abs_low, {base} AS base_pts FROM ranked \
         ) \
         SELECT u.id AS user_id, u.reg_seq AS reg_seq, u.username AS username, \
                (CASE WHEN u.avatar_key IS NOT NULL \
                      THEN '/v1/avatar/'||u.id||'?v='||COALESCE(u.avatar_version,0) END) AS avatar_url, \
                sum(s.base_pts + {bonus})::bigint AS points, \
                count(*)::bigint AS best_count, \
                count(*) FILTER (WHERE s.tc_off AND s.abs_off)::bigint AS zero_count, \
                ({gold_expr}) AS gold \
         FROM scored s JOIN users u ON u.id = s.user_id \
         GROUP BY u.id, u.reg_seq, u.username, u.avatar_key, u.avatar_version",
        version_filter = f
            .version
            .map(|_| "WHERE bl.version_code = $1".to_string())
            .unwrap_or_default(),
    );

    let mut q = sqlx::query_as::<_, PointsEntry>(&sql);
    if let Some(v) = f.version {
        q = q.bind(v);
    }
    let mut entries = q.fetch_all(&state.pool).await?;

    entries.sort_by(|a, b| {
        b.points
            .cmp(&a.points)
            .then_with(|| a.username.cmp(&b.username))
    });
    Ok(Json(PointsBoard {
        version: f.version,
        entries,
    }))
}

#[derive(Serialize)]
pub struct TrackEntry {
    pub rank: i64,
    pub user_id: Uuid,
    /// 车手 ID（注册顺序，从 1 起）
    pub reg_seq: i64,
    pub username: String,
    /// 头像（有 avatar_key 才有；客户端本地占位图兜底）
    pub avatar_url: Option<String>,
    pub lap_ms: i32,
    /// 服务端格式化的圈时（mm:ss.mmm）
    pub lap_display: String,
    /// 该车手的个人最快圈是否为零辅助（TC 与 ABS 均关）→ 整行金色流光。
    /// 判据只含 TC/ABS，不含踏板——用户定案：原生按键物理上跑不出有效圈。
    pub gold: bool,
}

#[derive(Serialize)]
pub struct TrackBoard {
    pub gp_index: i16,
    pub track_name: String,
    pub version: Option<i32>,
    pub entries: Vec<TrackEntry>,
}

/// GET /v1/leaderboard/track/{gp_index}?version=200146
pub async fn track_board(
    State(state): State<AppState>,
    Path(gp_index): Path<i16>,
    Query(f): Query<VersionFilter>,
) -> Result<Json<TrackBoard>, ApiError> {
    if !(0..16).contains(&gp_index) {
        return Err(ApiError::bad_request("gp_index 越界（0..15）"));
    }
    // 两分支都取"该用户在该赛道的最佳圈"及其配置：显式版本按版本过滤；
    // 无版本 = 跨版本 best of best（DISTINCT ON 取最快那条，与 min() 语义等价
    // 但能同时带回该圈的配置列）。外层再按圈速排序定名次。
    let avatar = "(CASE WHEN u.avatar_key IS NOT NULL \
                  THEN '/v1/avatar/'||u.id||'?v='||COALESCE(u.avatar_version,0) END)";
    let inner_where = match f.version {
        Some(_) => "b.gp_index=$1 AND b.version_code=$2",
        None => "b.gp_index=$1",
    };
    let sql = format!(
        "SELECT user_id, reg_seq, username, avatar_url, lap_ms, (tc_off AND abs_off) AS gold \
         FROM ( \
           SELECT DISTINCT ON (b.user_id) b.user_id, u.reg_seq, u.username, {avatar} AS avatar_url, \
                  b.lap_ms, b.tc_off, b.abs_off \
           FROM best_laps b JOIN users u ON u.id = b.user_id \
           WHERE {inner_where} \
           ORDER BY b.user_id, b.lap_ms ASC \
         ) t ORDER BY lap_ms ASC"
    );
    let mut q = sqlx::query_as::<_, (Uuid, i64, String, Option<String>, i32, bool)>(&sql);
    q = q.bind(gp_index);
    if let Some(v) = f.version {
        q = q.bind(v);
    }
    let rows = q.fetch_all(&state.pool).await?;

    let entries = rows
        .into_iter()
        .enumerate()
        .map(
            |(i, (user_id, reg_seq, username, avatar_url, lap_ms, gold))| TrackEntry {
                rank: (i + 1) as i64,
                user_id,
                reg_seq,
                username,
                avatar_url,
                lap_ms,
                lap_display: format_lap_ms(lap_ms),
                gold,
            },
        )
        .collect();
    Ok(Json(TrackBoard {
        gp_index,
        track_name: track_display_name(gp_index).to_string(),
        version: f.version,
        entries,
    }))
}

pub fn format_lap_ms(ms: i32) -> String {
    let m = ms / 60_000;
    let s = (ms % 60_000) / 1000;
    let frac = ms % 1000;
    format!("{m}:{s:02}.{frac:03}")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/leaderboard/points", get(points_board))
        .route("/leaderboard/track/{gp_index}", get(track_board))
}