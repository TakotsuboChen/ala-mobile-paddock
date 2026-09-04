//! 排行榜查询：积分总榜/版本榜 + 赛道榜。
//! 积分公式（v39 定案）：score = round((N−rank)×100/N)，第一 100、每名次递减 100/N、虚位第 N+1 名 0 分；N=1 → 100。
//! 总榜跨版本累加：对用户在所有版本 best_laps 的每赛道最佳名次积分求和
//! （同赛道多版本取该用户跨版本的最好成绩参与版本排名——定案口径：版本榜看该版本，
//! 总榜看"每个用户在该赛道的绝对最佳"，以 best of best 参与总榜排名）。

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{api::auth_handlers::ApiError, api::laps::track_display_name, state::AppState};

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
    let entries: Vec<PointsEntry> = match f.version {
        Some(v) => {
            // 版本榜：每用户在该版本内每赛道积分求和
            sqlx::query_as::<_, PointsEntry>(
                r#"WITH per_track AS (
                     SELECT user_id,
                            gp_index,
                            lap_ms,
                            rank() OVER (PARTITION BY gp_index ORDER BY lap_ms ASC) AS rank_in_track,
                            count(*) OVER (PARTITION BY gp_index) AS n_in_track
                     FROM best_laps WHERE version_code = $1
                   )
                   SELECT u.id AS user_id, u.reg_seq AS reg_seq, u.username AS username,
                          (CASE WHEN u.avatar_key IS NOT NULL THEN '/v1/avatar/'||u.id END) AS avatar_url,
                          (CASE WHEN n_in_track = 1 THEN 100
                                ELSE round((n_in_track + 1 - rank_in_track)::numeric * 100 / n_in_track)
                           END)::bigint AS points
                   FROM per_track p JOIN users u ON u.id = p.user_id"#,
            )
            .bind(v)
            .fetch_all(&state.pool)
            .await
            ?
            .into_iter()
            // 聚合到用户级
            .fold(std::collections::HashMap::<Uuid, PointsEntry>::new(), |mut m, e| {
                let entry = m.entry(e.user_id).or_insert(PointsEntry {
                    user_id: e.user_id, reg_seq: e.reg_seq, username: e.username.clone(), avatar_url: e.avatar_url.clone(), points: 0,
                });
                entry.points += e.points;
                m
            })
            .into_values()
            .collect()
        }
        None => {
            // 总榜（语义定案 2026-09-04）：每个游戏版本是独立赛季——同一用户在
            // 8.0.4 和 8.0.6 各拿全赛道第一 = 1600+1600 = 3200 分。积分按
            // (user, version, track) 维度独立计算后按用户累加，不跨版本取最快。
            sqlx::query_as::<_, PointsEntry>(
                r#"WITH per_track AS (
                     SELECT user_id,
                            gp_index,
                            version_code,
                            lap_ms,
                            rank() OVER (PARTITION BY version_code, gp_index ORDER BY lap_ms ASC) AS rank_in_track,
                            count(*) OVER (PARTITION BY version_code, gp_index) AS n_in_track
                     FROM best_laps
                   )
                   SELECT u.id AS user_id, u.reg_seq AS reg_seq, u.username AS username,
                          (CASE WHEN u.avatar_key IS NOT NULL THEN '/v1/avatar/'||u.id END) AS avatar_url,
                          (CASE WHEN n_in_track = 1 THEN 100
                                ELSE round((n_in_track + 1 - rank_in_track)::numeric * 100 / n_in_track)
                           END)::bigint AS points
                   FROM per_track p JOIN users u ON u.id = p.user_id"#,
            )
            .fetch_all(&state.pool)
            .await
            ?
            .into_iter()
            .fold(std::collections::HashMap::<Uuid, PointsEntry>::new(), |mut m, e| {
                let entry = m.entry(e.user_id).or_insert(PointsEntry {
                    user_id: e.user_id, reg_seq: e.reg_seq, username: e.username.clone(), avatar_url: e.avatar_url.clone(), points: 0,
                });
                entry.points += e.points;
                m
            })
            .into_values()
            .collect()
        }
    };
    let mut entries = entries;
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
    let rows: Vec<(Uuid, i64, String, Option<String>, i32)> = match f.version {
        Some(v) => sqlx::query_as(
            "SELECT b.user_id, u.reg_seq, u.username, (CASE WHEN u.avatar_key IS NOT NULL THEN '/v1/avatar/'||u.id END), b.lap_ms FROM best_laps b JOIN users u ON u.id=b.user_id \
             WHERE b.gp_index=$1 AND b.version_code=$2 ORDER BY b.lap_ms ASC",
        )
        .bind(gp_index)
        .bind(v)
        .fetch_all(&state.pool)
        .await
        ?,
        None => {
            sqlx::query_as(
                "SELECT ub.user_id, u.reg_seq, u.username, (CASE WHEN u.avatar_key IS NOT NULL THEN '/v1/avatar/'||u.id END), ub.lap_ms FROM \
                 (SELECT user_id, min(lap_ms) AS lap_ms FROM best_laps WHERE gp_index=$1 GROUP BY user_id) ub \
                 JOIN users u ON u.id=ub.user_id ORDER BY ub.lap_ms ASC",
            )
            .bind(gp_index)
            .fetch_all(&state.pool)
            .await
            ?
        }
    };
    let entries = rows
        .into_iter()
        .enumerate()
        .map(|(i, (user_id, reg_seq, username, avatar_url, lap_ms))| TrackEntry {
            rank: (i + 1) as i64,
            user_id,
            reg_seq,
            username,
            avatar_url,
            lap_ms,
            lap_display: format_lap_ms(lap_ms),
        })
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
