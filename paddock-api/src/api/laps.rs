//! POST /v1/laps —— 圈速上报（每有效圈都传）。
//! 事务内：写入 laps 全量留档 → upsert best_laps → 与 records 比对判定 Toast → 更新 records。
//! Toast 四条件取最高（定案）：全服历史 > 全服版本 > 个人历史 > 个人版本。
//! 防伪定案=全放行：无物理阈值拒收，仅登录态门槛 + 管理端事后删。

use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{api::auth_handlers::ApiError, auth, state::AppState};

#[derive(Deserialize)]
pub struct LapUpload {
    /// 0..15（buildIndex − 2，见 TRACK_IDENTIFICATION.md）
    pub gp_index: i16,
    /// 游戏 6 位 versionCode（200146 = 8.0.4）
    pub version_code: i32,
    /// 完整圈时（毫秒）。调用方须只对有效圈上报（模块侧 lap_hook validLap 位过滤）。
    pub lap_ms: i32,
}

#[derive(Serialize)]
pub struct LapUploadResp {
    pub personal_best: bool,
    pub server_best: bool,
    /// null = 无提示；否则模块弹 Toast。四条件取最高后仅一条。
    pub toast: Option<Toast>,
}

#[derive(Serialize)]
pub struct Toast {
    /// alltime_server | version_server | alltime_personal | version_personal
    pub level: &'static str,
    /// "您已刷新{track}{scope}的{subject}最佳成绩" 模板所需的赛道中文名
    pub track: String,
    pub lap_ms: i32,
}

pub async fn upload_lap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(lap): Json<LapUpload>,
) -> Result<Json<LapUploadResp>, ApiError> {
    let user_id = authenticate(&state, &headers).await?;
    if !(0..16).contains(&lap.gp_index) {
        return Err(ApiError::bad_request("gp_index 越界（0..15）"));
    }
    if lap.lap_ms <= 0 {
        return Err(ApiError::bad_request("lap_ms 非法"));
    }

    let mut tx = state.pool.begin().await?;

    // 1) 全量留档
    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(lap.gp_index)
    .bind(lap.version_code)
    .bind(lap.lap_ms)
    .execute(&mut *tx)
    .await?;

    // 2) 个人版本最佳（best_laps：每人每赛道每版本一条）
    let prev_personal: Option<i32> = sqlx::query_scalar(
        "UPDATE best_laps SET lap_ms = $4, updated_at = now() \
         WHERE user_id=$1 AND gp_index=$2 AND version_code=$3 AND lap_ms > $4 \
         RETURNING (SELECT lap_ms FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3 AND lap_ms > $4)",
    )
    .bind(user_id)
    .bind(lap.gp_index)
    .bind(lap.version_code)
    .bind(lap.lap_ms)
    .fetch_optional(&mut *tx)
    .await
    ?
    .flatten();

    // 上面 UPDATE ... RETURNING 的技巧在"无旧行"时返回 NULL，需补 insert 路径
    let prev_personal = match prev_personal {
        Some(p) => Some(p),
        None => {
            let exists: Option<i32> = sqlx::query_scalar(
                "SELECT lap_ms FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3",
            )
            .bind(user_id)
            .bind(lap.gp_index)
            .bind(lap.version_code)
            .fetch_optional(&mut *tx)
            .await?;
            match exists {
                Some(better) if better <= lap.lap_ms => Some(better), // 未破个人版本纪录
                _ => {
                    sqlx::query(
                        "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms) \
                         VALUES ($1,$2,$3,$4) \
                         ON CONFLICT (user_id, gp_index, version_code) \
                         DO UPDATE SET lap_ms = EXCLUDED.lap_ms, updated_at = now()",
                    )
                    .bind(user_id)
                    .bind(lap.gp_index)
                    .bind(lap.version_code)
                    .bind(lap.lap_ms)
                    .execute(&mut *tx)
                    .await?;
                    None // 新纪录或首次，无"前值"
                }
            }
        }
    };
    let personal_best = prev_personal.map_or(true, |p| lap.lap_ms < p);

    // 3) 全服记录比对（历史 + 版本两个维度）
    let mut toast: Option<Toast> = None;
    let mut server_best = false;

    // 3a. 全服历史（alltime，跨版本）
    let alltime: Option<(i32, Uuid)> =
        sqlx::query_as("SELECT lap_ms, user_id FROM records WHERE gp_index=$1 AND kind='alltime'")
            .bind(lap.gp_index)
            .fetch_optional(&mut *tx)
            .await?;
    let is_alltime_new = match alltime {
        None => true,
        Some((ms, _)) => lap.lap_ms < ms,
    };

    // 3b. 版本最佳
    let version_best: Option<i32> = sqlx::query_scalar(
        "SELECT lap_ms FROM records WHERE gp_index=$1 AND kind='version' AND version_code=$2",
    )
    .bind(lap.gp_index)
    .bind(lap.version_code)
    .fetch_optional(&mut *tx)
    .await?;
    let is_version_new = match version_best {
        None => true,
        Some(ms) => lap.lap_ms < ms,
    };

    // 两个纪录维度独立判定、独立 upsert；Toast 按 alltime_server > version_server 取最高。
    server_best = is_alltime_new || is_version_new;
    if server_best {
        let track = track_display_name(lap.gp_index).to_string();
        if is_alltime_new {
            // alltime 行 version_code 占位 0（复合主键列不可 NULL，见 migration 注释）
            upsert_record(&mut tx, lap.gp_index, "alltime", 0, lap.lap_ms, user_id).await?;
        }
        if is_version_new {
            upsert_record(
                &mut tx,
                lap.gp_index,
                "version",
                lap.version_code,
                lap.lap_ms,
                user_id,
            )
            .await?;
        }
        if is_alltime_new {
            toast = Some(Toast {
                level: "alltime_server",
                track,
                lap_ms: lap.lap_ms,
            });
        } else {
            toast = Some(Toast {
                level: "version_server",
                track,
                lap_ms: lap.lap_ms,
            });
        }
    } else if personal_best {
        // 未登顶但破个人纪录：历史 > 版本（同帧取最高定案）
        let track = track_display_name(lap.gp_index).to_string();
        let had_history_personal = sqlx::query_scalar::<_, i32>(
            "SELECT min(lap_ms) FROM best_laps WHERE user_id=$1 AND gp_index=$2",
        )
        .bind(user_id)
        .bind(lap.gp_index)
        .fetch_one(&mut *tx)
        .await?;
        // min 含刚更新的本版本行；若 min==lap_ms 则本圈也是最速 → 个人历史纪录
        toast = if had_history_personal == lap.lap_ms {
            Some(Toast {
                level: "alltime_personal",
                track,
                lap_ms: lap.lap_ms,
            })
        } else {
            Some(Toast {
                level: "version_personal",
                track,
                lap_ms: lap.lap_ms,
            })
        };
    }

    tx.commit().await?;
    Ok(Json(LapUploadResp {
        personal_best,
        server_best,
        toast,
    }))
}

async fn upsert_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    gp_index: i16,
    kind: &str,
    version_code: i32, // alltime 行恒 0 占位
    lap_ms: i32,
    user_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id, updated_at) \
         VALUES ($1,$2,$3,$4,$5,now()) \
         ON CONFLICT (gp_index, kind, version_code) \
         DO UPDATE SET lap_ms = EXCLUDED.lap_ms, user_id = EXCLUDED.user_id, updated_at = now()",
    )
    .bind(gp_index)
    .bind(kind)
    .bind(version_code)
    .bind(lap_ms)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Bearer token → user_id（90 天滑动：命中即续期）。
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("缺少登录态，请先登录围场"))?;
    let token_hash = auth::sha256_hex(token);
    let row: Option<(Uuid, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT user_id, expires_at FROM sessions WHERE token_hash = $1")
            .bind(&token_hash)
            .fetch_optional(&state.pool)
            .await?;
    let Some((user_id, expires_at)) = row else {
        return Err(ApiError::unauthorized("登录态无效，请重新登录"));
    };
    if expires_at < chrono::Utc::now() {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(&token_hash)
            .execute(&state.pool)
            .await?;
        return Err(ApiError::unauthorized("登录态已过期，请重新登录"));
    }
    // 滑动续期
    sqlx::query("UPDATE sessions SET expires_at = $2 WHERE token_hash = $1")
        .bind(&token_hash)
        .bind(chrono::Utc::now() + chrono::Duration::days(auth::TOKEN_TTL_DAYS))
        .execute(&state.pool)
        .await?;
    Ok(user_id)
}

/// 赛道中文名（契约源头=PADDOCK_PLAN.md §5；模块侧同名表保持一致，改必须两边同改）。
pub fn track_display_name(gp_index: i16) -> &'static str {
    match gp_index {
        0 => "阿尔伯特公园赛道",
        1 => "上海国际赛车场",
        2 => "巴林国际赛车场",
        3 => "伊莫拉赛道",
        4 => "加泰罗尼亚赛道",
        5 => "摩纳哥赛道",
        6 => "吉尔·维伦纽夫赛道",
        7 => "红牛环赛道",
        8 => "银石赛道",
        9 => "霍根海姆赛道",
        10 => "亨格罗宁赛道",
        11 => "斯帕-弗朗科尔尚赛道",
        12 => "蒙扎国家赛车场",
        13 => "铃鹿赛道",
        14 => "英特拉格斯赛道",
        _ => "亚斯码头赛道",
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/laps", post(upload_lap))
}
