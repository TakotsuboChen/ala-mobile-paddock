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

/// 榜单分页参数（2026-10-18 新增）。
///
/// 为什么必须分页：榜单随用户数线性增长，一次性下发全部条目会让
/// ① 服务端 `fetch_all` 把全表搬进内存；② 移动网络下响应体随人数膨胀。
/// 客户端按需"滚到底再拉下一页"，单次上限 [`MAX_PAGE_LIMIT`]。
///
/// `limit` 缺省 = [`DEFAULT_PAGE_LIMIT`]（30），越界一律夹到上限——
/// 不报错（旧客户端不传参天然拿到第一页，前向兼容）。
#[derive(Deserialize)]
pub struct VersionFilter {
    pub version: Option<i32>,
    /// 本页条数（1..=MAX_PAGE_LIMIT）
    pub limit: Option<i64>,
    /// 跳过条数（≥0）
    pub offset: Option<i64>,
}

/// 默认每页条数（与模块端 PaddockClient.PAGE_SIZE 同值，改必须两边同改）。
pub const DEFAULT_PAGE_LIMIT: i64 = 30;
/// 单页硬上限：防止手改 URL 拉全表。
pub const MAX_PAGE_LIMIT: i64 = 100;

impl VersionFilter {
    /// 规范化后的 (limit, offset)。负数/超大值一律夹到合法区间。
    fn page(&self) -> (i64, i64) {
        let limit = self.limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
        let offset = self.offset.unwrap_or(0).max(0);
        (limit, offset)
    }
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
    /// 该榜总条数（不受本页 limit/offset 影响）。客户端据此判断是否还有下一页。
    pub total: i64,
}

/// GET /v1/leaderboard/points?version=200146&limit=30&offset=0
/// 无 version 参数 = 总榜（跨版本累加）。
pub async fn points_board(
    State(state): State<AppState>,
    Query(f): Query<VersionFilter>,
) -> Result<Json<PointsBoard>, ApiError> {
    let (limit, offset) = f.page();
    // 两个榜只差金标的存在性条件，其余（窗口 + 基础分）由 score 模块的
    // [`score::scored_cte`] 生成——与 `/v1/me`、车手主页共用同一份窗口定义。
    let gold_expr = match f.version {
        Some(_) => "2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)",
        None => {
            "count(*) FILTER (WHERE s.tc_off AND s.abs_off) > 0 \
             AND 2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)"
        }
    };
    let bonus = score::bonus_sql("s.base_pts", "s");
    let scored = score::scored_cte(if f.version.is_some() {
        "WHERE bl.version_code = $1"
    } else {
        ""
    });

    // 层级说明：Postgres 不允许在同一 SELECT 列表里引用窗口函数别名，
    // 也不允许引用同层的普通别名 → 窗口(ranked) → 基础分(scored) → 加成(最外层) 三层。
    // ⚠️ `scored` 必须把 bonus 用到的**全部四个派生位列**一起带下来（pedal_linear /
    //    tc_off / abs_off / abs_low）——漏一个就是运行期 `column s.x does not exist`
    //    500 错误。Rust 侧 bonus_points() 读的是结构体字段（天然齐全），
    //    单元测试发现不了这类"SQL 管线漏列"，只有真库能测（见 score_sql_tests.rs）。
    //
    // ⚠️ 分页（2026-10-18）：排序**必须下推到 SQL**——原先在 Rust 侧
    //    `entries.sort_by(...)` 之后再切片，切片拿到的只是"未排序结果的前 N 条"，
    //    跨页会重复/漏项。故把原聚合查询包成 `agg` 子查询，外层 ORDER BY
    //    points DESC, username ASC（与原 Rust 排序逐字等价）+ LIMIT/OFFSET。
    //    `total` 由同一条 CTE 的 count 窗口在分页前算好，省一次全表 count。
    let sql = format!(
        "{scored}, \
         agg AS ( \
           SELECT u.id AS user_id, u.reg_seq AS reg_seq, u.username AS username, \
                  (CASE WHEN u.avatar_key IS NOT NULL \
                        THEN '/v1/avatar/'||u.id||'?v='||COALESCE(u.avatar_version,0) END) AS avatar_url, \
                  sum(s.base_pts + {bonus})::bigint AS points, \
                  count(*)::bigint AS best_count, \
                  count(*) FILTER (WHERE s.tc_off AND s.abs_off)::bigint AS zero_count, \
                  ({gold_expr}) AS gold \
           FROM scored s JOIN users u ON u.id = s.user_id \
           GROUP BY u.id, u.reg_seq, u.username, u.avatar_key, u.avatar_version \
         ) \
         SELECT *, count(*) OVER ()::bigint AS total FROM agg \
         ORDER BY points DESC, username ASC LIMIT ${limit_ph} OFFSET ${offset_ph}",
        limit_ph = if f.version.is_some() { 2 } else { 1 },
        offset_ph = if f.version.is_some() { 3 } else { 2 },
    );

    // total 作为额外列取出来：分页后 count(*) OVER () 只在结果行上出现，
    // 空页（offset 越界）时拿不到 → 单独兜底为 0（客户端见 entries 空即停止）。
    #[derive(sqlx::FromRow)]
    struct RowWithTotal {
        user_id: Uuid,
        reg_seq: i64,
        username: String,
        avatar_url: Option<String>,
        points: i64,
        best_count: i64,
        zero_count: i64,
        gold: bool,
        total: i64,
    }
    let mut q = sqlx::query_as::<_, RowWithTotal>(&sql);
    if let Some(v) = f.version {
        q = q.bind(v);
    }
    q = q.bind(limit).bind(offset);
    let rows = q.fetch_all(&state.pool).await?;
    let total = rows.first().map(|r| r.total).unwrap_or(0);
    let entries = rows
        .into_iter()
        .map(|r| PointsEntry {
            user_id: r.user_id,
            reg_seq: r.reg_seq,
            username: r.username,
            avatar_url: r.avatar_url,
            points: r.points,
            best_count: r.best_count,
            zero_count: r.zero_count,
            gold: r.gold,
        })
        .collect();
    Ok(Json(PointsBoard {
        version: f.version,
        entries,
        total,
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
    /// 该赛道榜总人数（不受本页 limit/offset 影响）。
    pub total: i64,
}

/// GET /v1/leaderboard/track/{gp_index}?version=200146&limit=30&offset=0
pub async fn track_board(
    State(state): State<AppState>,
    Path(gp_index): Path<i16>,
    Query(f): Query<VersionFilter>,
) -> Result<Json<TrackBoard>, ApiError> {
    if !(0..16).contains(&gp_index) {
        return Err(ApiError::bad_request("gp_index 越界（0..15）"));
    }
    let (limit, offset) = f.page();
    // 两分支都取"该用户在该赛道的最佳圈"及其配置：显式版本按版本过滤；
    // 无版本 = 跨版本 best of best（DISTINCT ON 取最快那条，与 min() 语义等价
    // 但能同时带回该圈的配置列）。外层再按圈速排序定名次。
    let avatar = "(CASE WHEN u.avatar_key IS NOT NULL \
                  THEN '/v1/avatar/'||u.id||'?v='||COALESCE(u.avatar_version,0) END)";
    let inner_where = match f.version {
        Some(_) => "b.gp_index=$1 AND b.version_code=$2",
        None => "b.gp_index=$1",
    };
    // ⚠️ 名次必须按**全量**排完再分页（rank 是全局名次，不是页内序号）。
    //    做法：先 `ranked` 算好 row_number + total，再在外层 LIMIT/OFFSET。
    //    旧实现在 Rust 里 `enumerate()` 生成 rank——分页后第二页会从 1 重新编号。
    // ⚠️ 别名 `rank` 必须加双引号：`rank` 是 Postgres 内置窗口函数名（非保留字，
    //    但裸用做输出别名 + `ORDER BY rank` 有被解析成函数调用的风险）。加引号
    //    后是纯标识符，`ORDER BY "rank"` 无歧义。
    let limit_ph = if f.version.is_some() { 3 } else { 2 };
    let offset_ph = if f.version.is_some() { 4 } else { 3 };
    let sql = format!(
        "WITH ranked AS ( \
           SELECT user_id, reg_seq, username, avatar_url, lap_ms, \
                  (tc_off AND abs_off) AS gold, \
                  row_number() OVER (ORDER BY lap_ms ASC)::bigint AS \"rank\", \
                  count(*) OVER ()::bigint AS total \
           FROM ( \
             SELECT DISTINCT ON (b.user_id) b.user_id, u.reg_seq, u.username, {avatar} AS avatar_url, \
                    b.lap_ms, b.tc_off, b.abs_off \
             FROM best_laps b JOIN users u ON u.id = b.user_id \
             WHERE {inner_where} \
             ORDER BY b.user_id, b.lap_ms ASC \
           ) t \
         ) \
         SELECT * FROM ranked ORDER BY \"rank\" LIMIT ${limit_ph} OFFSET ${offset_ph}"
    );
    #[derive(sqlx::FromRow)]
    struct RowWithRank {
        user_id: Uuid,
        reg_seq: i64,
        username: String,
        avatar_url: Option<String>,
        lap_ms: i32,
        gold: bool,
        #[sqlx(rename = "rank")]
        rank: i64,
        total: i64,
    }
    let mut q = sqlx::query_as::<_, RowWithRank>(&sql);
    q = q.bind(gp_index);
    if let Some(v) = f.version {
        q = q.bind(v);
    }
    q = q.bind(limit).bind(offset);
    let rows = q.fetch_all(&state.pool).await?;
    let total = rows.first().map(|r| r.total).unwrap_or(0);

    let entries = rows
        .into_iter()
        .map(|r| TrackEntry {
            rank: r.rank,
            user_id: r.user_id,
            reg_seq: r.reg_seq,
            username: r.username,
            avatar_url: r.avatar_url,
            lap_ms: r.lap_ms,
            lap_display: format_lap_ms(r.lap_ms),
            gold: r.gold,
        })
        .collect();
    Ok(Json(TrackBoard {
        gp_index,
        track_name: track_display_name(gp_index).to_string(),
        version: f.version,
        entries,
        total,
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