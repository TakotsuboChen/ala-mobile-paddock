//! 车手主页：个人资料 + 全量圈速记录（分页）。
//!
//! ## 为什么是独立端点而不是 `/v1/me` 扩展
//!
//! `/v1/me` 是**本人**的登录态查询（Bearer），而车手主页是**公开**页面——
//! 从排行榜点任意一行都能进。若复用它，得额外区分"查自己/查别人"两套语义，
//! 且 `/v1/me` 的响应形状被模块登录恢复路径依赖，动它有回归风险。
//!
//! ## 数据源 = `laps`（全量留档），不是 `best_laps`
//!
//! 用户定案（2026-10-18）：主页要看**每一次上传的成绩**（进步曲线），
//! 而非每人每赛道一条最佳。`best_laps` 仍只服务榜单与"仅个人最佳"筛选。
//!
//! ## 筛选维度
//!
//! 赛道 / 版本 / 零辅助 / 仅个人最佳。前三者是 `laps` 上的原始列；
//! "仅个人最佳"用 `EXISTS (best_laps ...)` 子查询，与 `admin.rs::fetch_laps`
//! 的 `best=personal` 分支同语义（同一份数据面，两处口径必须一致）。

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::{
        auth_handlers::{ApiError, total_points_of},
        laps::{assist_display, track_display_name, version_display},
        leaderboard::{DEFAULT_PAGE_LIMIT, format_lap_ms},
    },
    state::AppState,
};

#[derive(Deserialize)]
pub struct DriverQuery {
    /// 0..15；缺省 = 全部赛道
    pub gp_index: Option<i16>,
    /// 游戏 versionCode；缺省 = 全部版本
    pub version: Option<i32>,
    /// `zero` = 零辅助（TC 与 ABS 均关）；缺省 = 不筛
    pub assist: Option<String>,
    /// `1` = 仅该用户各赛道各版本的最佳圈（与榜单同源）
    pub best: Option<i32>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl DriverQuery {
    fn page(&self) -> (i64, i64) {
        let limit = self.limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, 100);
        let offset = self.offset.unwrap_or(0).max(0);
        (limit, offset)
    }
}

#[derive(Serialize)]
pub struct DriverLapEntry {
    pub lap_id: Uuid,
    pub gp_index: i16,
    pub track_name: String,
    pub version_code: i32,
    pub version_name: String,
    pub lap_ms: i32,
    pub lap_display: String,
    /// 该圈是否是"该车手在该赛道该版本的个人最佳"（= best_laps 里的那条）
    pub personal_best: bool,
    /// 辅助配置三维展示（踏板 / TC / ABS）。缺失维显示"缺失"。
    pub pedal: String,
    pub tc: String,
    pub abs: String,
    /// 是否零辅助（TC 与 ABS 均关）——与榜单金标同判据
    pub zero_assist: bool,
    /// 上传时间（北京时间 `YYYY-MM-DD HH:MM`）
    pub created: String,
}

#[derive(Serialize)]
pub struct DriverProfile {
    pub user_id: Uuid,
    pub username: String,
    pub reg_seq: i64,
    pub avatar_url: Option<String>,
    /// 计时赛总积分（与 `/v1/me`、积分总榜同口径）
    pub total_points: i64,
    /// 有成绩的赛道数（best_laps 维度，与积分榜 best_count 同义）
    pub best_count: i64,
    /// 其中零辅助最快圈的条数（积分榜 zero_count 同义）
    pub zero_count: i64,
    /// 积分榜同款金标（服务端判定的唯一事实源）
    pub gold: bool,
}

#[derive(Serialize)]
pub struct DriverPage {
    pub profile: DriverProfile,
    pub entries: Vec<DriverLapEntry>,
    /// 满足筛选条件的总条数（客户端据此判断是否还有下一页）
    pub total: i64,
}

/// GET /v1/driver/{user_id}?gp_index=&version=&assist=zero&best=1&limit=30&offset=0
///
/// 公开端点（无需登录）：排行榜点击任意车手即可查看。
pub async fn driver_page(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Query(q): Query<DriverQuery>,
) -> Result<Json<DriverPage>, ApiError> {
    if let Some(g) = q.gp_index {
        if !(0..16).contains(&g) {
            return Err(ApiError::bad_request("gp_index 越界（0..15）"));
        }
    }
    // ── 资料卡 ──
    // best_count / zero_count / gold 与积分榜同口径：统计 best_laps 维度
    // （每人每赛道每版本一条）。`gold` 用总榜判据（2×零辅助 ≥ 总数 且 零辅助 > 0）。
    let profile_row: Option<(String, i64, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT u.username, u.reg_seq, \
           CASE WHEN u.avatar_key IS NOT NULL \
                THEN '/v1/avatar/'||u.id||'?v='||COALESCE(u.avatar_version,0) END, \
           count(b.*)::bigint, \
           count(b.*) FILTER (WHERE b.tc_off AND b.abs_off)::bigint \
         FROM users u LEFT JOIN best_laps b ON b.user_id = u.id \
         WHERE u.id = $1 \
         GROUP BY u.id, u.username, u.reg_seq, u.avatar_key, u.avatar_version",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((username, reg_seq, avatar_url, best_count, zero_count)) = profile_row else {
        return Err(ApiError::not_found("车手不存在"));
    };
    let total_points = total_points_of(&state, user_id).await?;
    let gold = zero_count > 0 && 2 * zero_count >= best_count;
    let profile = DriverProfile {
        user_id,
        username,
        reg_seq,
        avatar_url,
        total_points,
        best_count,
        zero_count,
        gold,
    };

    // ── 圈速记录（分页）──
    // 占位符按需累加：$1 恒为 user_id，其余按筛选条件顺序追加。
    let mut where_sql = String::from("l.user_id = $1");
    let mut next_ph = 2usize;
    if q.gp_index.is_some() {
        where_sql.push_str(&format!(" AND l.gp_index = ${next_ph}"));
        next_ph += 1;
    }
    if q.version.is_some() {
        where_sql.push_str(&format!(" AND l.version_code = ${next_ph}"));
        next_ph += 1;
    }
    if q.assist.as_deref() == Some("zero") {
        // 零辅助 = mode=custom 且 strength=off（与 score::AssistFlags 派生规则一致）。
        // ⚠️ 必须带 mode 判断：mode=default 时 strength 只是 UI 记忆值、不生效。
        where_sql.push_str(
            " AND l.tc_mode = 'custom' AND l.tc_strength = 'off' \
              AND l.abs_mode = 'custom' AND l.abs_strength = 'off'",
        );
    }
    if q.best == Some(1) {
        // 与 admin.rs::fetch_laps 的 best=personal 分支同语义。
        where_sql.push_str(
            " AND EXISTS (SELECT 1 FROM best_laps b2 WHERE b2.user_id=l.user_id \
              AND b2.gp_index=l.gp_index AND b2.version_code=l.version_code AND b2.lap_ms=l.lap_ms)",
        );
    }
    let limit_ph = next_ph;
    let offset_ph = next_ph + 1;
    let (limit, offset) = q.page();

    // total 用独立 count：`count(*) OVER ()` 在空页时拿不到值（offset 越界），
    // 而客户端正是靠 total 判断"还有没有下一页"，空页返回 0 会被误判成没数据。
    let count_sql = format!("SELECT count(*) FROM laps l WHERE {where_sql}");
    // 排序 = 时间倒序（最新上传在前）；同一时刻用 lap_id 兜底保证分页稳定
    //（无稳定排序键时 LIMIT/OFFSET 可能跨页重复/漏项）。
    let data_sql = format!(
        "SELECT l.id, l.gp_index, l.version_code, l.lap_ms, l.created_at, \
                l.pedal_mode, l.tc_mode, l.tc_strength, l.abs_mode, l.abs_strength, \
                EXISTS (SELECT 1 FROM best_laps b3 WHERE b3.user_id=l.user_id \
                  AND b3.gp_index=l.gp_index AND b3.version_code=l.version_code \
                  AND b3.lap_ms=l.lap_ms) AS personal_best \
         FROM laps l WHERE {where_sql} \
         ORDER BY l.created_at DESC, l.id DESC LIMIT ${limit_ph} OFFSET ${offset_ph}"
    );

    // 绑定顺序必须与占位符序号一致。
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql).bind(user_id);
    if let Some(g) = q.gp_index {
        cq = cq.bind(g);
    }
    if let Some(v) = q.version {
        cq = cq.bind(v);
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    #[derive(sqlx::FromRow)]
    struct LapRow {
        id: Uuid,
        gp_index: i16,
        version_code: i32,
        lap_ms: i32,
        created_at: chrono::DateTime<chrono::Utc>,
        pedal_mode: Option<String>,
        tc_mode: Option<String>,
        tc_strength: Option<String>,
        abs_mode: Option<String>,
        abs_strength: Option<String>,
        personal_best: bool,
    }
    let mut dq = sqlx::query_as::<_, LapRow>(&data_sql).bind(user_id);
    if let Some(g) = q.gp_index {
        dq = dq.bind(g);
    }
    if let Some(v) = q.version {
        dq = dq.bind(v);
    }
    let rows = dq.bind(limit).bind(offset).fetch_all(&state.pool).await?;

    let entries = rows
        .into_iter()
        .map(|r| {
            let (pedal, tc, abs) = assist_display(
                r.pedal_mode.as_deref(),
                r.tc_mode.as_deref(),
                r.tc_strength.as_deref(),
                r.abs_mode.as_deref(),
                r.abs_strength.as_deref(),
            );
            let zero_assist = r.tc_mode.as_deref() == Some("custom")
                && r.tc_strength.as_deref() == Some("off")
                && r.abs_mode.as_deref() == Some("custom")
                && r.abs_strength.as_deref() == Some("off");
            DriverLapEntry {
                lap_id: r.id,
                gp_index: r.gp_index,
                track_name: track_display_name(r.gp_index).to_string(),
                version_code: r.version_code,
                version_name: version_display(r.version_code),
                lap_ms: r.lap_ms,
                lap_display: format_lap_ms(r.lap_ms),
                personal_best: r.personal_best,
                pedal,
                tc,
                abs,
                zero_assist,
                // 北京时间（与管理端同口径）：服务端统一按东八区展示，避免
                // 用户在 UTC+8 看到"昨天"的成绩。
                created: r
                    .created_at
                    .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
            }
        })
        .collect();

    Ok(Json(DriverPage {
        profile,
        entries,
        total,
    }))
}

pub fn router() -> Router<AppState> {
    Router::new().route("/driver/{user_id}", get(driver_page))
}
