//! POST /v1/laps —— 圈速上报（每有效圈都传）。
//! 事务内：写入 laps 全量留档 → upsert best_laps → 与 records 比对判定 Toast → 更新 records。
//! Toast 四条件取最高（定案）：全服历史 > 全服版本 > 个人历史 > 个人版本。
//! 防伪定案=全放行：无物理阈值拒收，仅登录态门槛 + 管理端事后删。
//!
//! ## 辅助配置（2026-09-18）
//!
//! 上报体附带**原始配置枚举**（`AssistConfig`），由服务端派生四个布尔位存进
//! best_laps——客户端只报事实，加减分规则留在服务端（改规则不必发模块版本）。
//! 缺失 = 全部 Option 为 None（旧模块不传字段 → serde 落到 `Default`），
//! 派生位全 false = 不加分，与"缺失"语义一致。
//!
//! ️ 配置随**圈**走：best_laps 存的是"拿分的**那条圈**的配置"。玩家换配置后
//! 跑得更快 → upsert 时配置一并刷新；跑更慢 → 配置不动（榜上仍是旧那条圈的配置）。

use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{api::auth_handlers::ApiError, auth, score, state::AppState};

#[derive(Deserialize)]
pub struct LapUpload {
    /// 0..15（buildIndex − 2，见 TRACK_IDENTIFICATION.md）
    pub gp_index: i16,
    /// 游戏 6 位 versionCode（200150 = 8.0.6）
    pub version_code: i32,
    /// 完整圈时（毫秒）。调用方须只对有效圈上报（模块侧 lap_hook validLap 位过滤）。
    pub lap_ms: i32,
    /// 辅助配置五维原始枚举（缺失 = None）。`#[serde(flatten)]` 让旧客户端
    /// （只发三个字段）与不确定字段的新客户端都能通过反序列化。
    #[serde(flatten)]
    pub assist: score::AssistConfig,
    /// 车型 + 车辆调教（2026-10-26，全部可选）。同样 flatten，旧客户端零改动。
    #[serde(flatten)]
    pub car_setup: score::CarSetupConfig,
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

    // 配置归一化：未知枚举值落"缺失"而非报错（前向兼容 + 数据面白名单）。
    let assist = lap.assist.normalized();
    let flags = score::AssistFlags::from(&assist);
    // 车型 + 调教归一化：车型越界/调教半套 → 整项落 None（同上动机）。
    let cs = lap.car_setup.normalized();

    // 1) 全量留档
    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms, \
                           pedal_mode, tc_mode, tc_strength, abs_mode, abs_strength, \
                           car_id, car_era, \
                           setup_front_wing_aero, setup_rear_wing_aero, \
                           setup_front_ride_height, setup_rear_ride_height, \
                           setup_brake_bias, \
                           setup_front_stiffness, setup_rear_stiffness, \
                           setup_front_rollbar, setup_rear_rollbar, \
                           setup_front_camber, setup_rear_camber, \
                           setup_front_toe, setup_rear_toe) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(lap.gp_index)
    .bind(lap.version_code)
    .bind(lap.lap_ms)
    .bind(&assist.pedal_mode)
    .bind(&assist.tc_mode)
    .bind(&assist.tc_strength)
    .bind(&assist.abs_mode)
    .bind(&assist.abs_strength)
    .bind(cs.car_id)
    .bind(cs.car_era)
    .bind(cs.setup_front_wing_aero)
    .bind(cs.setup_rear_wing_aero)
    .bind(cs.setup_front_ride_height)
    .bind(cs.setup_rear_ride_height)
    .bind(cs.setup_brake_bias)
    .bind(cs.setup_front_stiffness)
    .bind(cs.setup_rear_stiffness)
    .bind(cs.setup_front_rollbar)
    .bind(cs.setup_rear_rollbar)
    .bind(cs.setup_front_camber)
    .bind(cs.setup_rear_camber)
    .bind(cs.setup_front_toe)
    .bind(cs.setup_rear_toe)
    .execute(&mut *tx)
    .await?;

    // 1.5) **自动屏蔽闸门**（2026-10-20 二次定案）。
    //      用户开启过屏蔽（`hidden_missing_at IS NOT NULL`）且本圈配置缺失
    //      ⇒ 本圈按 [`AUTO_HIDDEN_SQL`] 属于「应被屏蔽」，**不得进入 best_laps /
    //      records**。laps 全量留档照常（1) 已写），这样管理端补全配置后，
    //      下次 recalc_dims 重算时谓词变假，本圈自动回榜——两条路径共用同一判据。
    //
    //      ⚠️ 必须在这里拦，不能只在重算路径拦：屏蔽时 best_laps 行已被删除，
    //         下面的 `UPDATE ... WHERE lap_ms > $4` 会命中 0 行并走 insert 分支，
    //         那条路径不看任何屏蔽判据，会把圈直接插回榜单。
    //
    //      判据同源：配置侧走 Rust 版 [`is_assist_missing`]（与 SQL 版
    //      `ASSIST_MISSING_SQL` 由单元测试锁死逐字对应），开关侧查 users。
    let hidden_switch: bool = sqlx::query_scalar(
        "SELECT hidden_missing_at IS NOT NULL FROM users WHERE id=$1",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    let auto_hidden = hidden_switch
        && is_assist_missing(
            assist.pedal_mode.as_deref(),
            assist.tc_mode.as_deref(),
            assist.tc_strength.as_deref(),
            assist.abs_mode.as_deref(),
            assist.abs_strength.as_deref(),
        );
    if auto_hidden {
        tx.commit().await?;
        tracing::debug!(
            "user {user_id} auto-hid missing-config lap gp={} ver={} ms={}",
            lap.gp_index,
            lap.version_code,
            lap.lap_ms
        );
        return Ok(Json(LapUploadResp {
            personal_best: false,
            server_best: false,
            toast: None,
        }));
    }

    // 2) 个人版本最佳（best_laps：每人每赛道每版本一条）
    //    ⚠️ 配置列必须与 lap_ms 同期更新——否则"刷新个人纪录"后榜上挂着
    //    上一条圈的旧配置，金标与加成都算错。两者同属"这条圈的事实"。
    let prev_personal: Option<i32> = sqlx::query_scalar(
        "UPDATE best_laps SET lap_ms = $4, updated_at = now(), \
                pedal_linear = $5, tc_off = $6, abs_off = $7, abs_low = $8 \
         WHERE user_id=$1 AND gp_index=$2 AND version_code=$3 AND lap_ms > $4 \
         RETURNING (SELECT lap_ms FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3 AND lap_ms > $4)",
    )
    .bind(user_id)
    .bind(lap.gp_index)
    .bind(lap.version_code)
    .bind(lap.lap_ms)
    .bind(flags.pedal_linear)
    .bind(flags.tc_off)
    .bind(flags.abs_off)
    .bind(flags.abs_low)
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
                        "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms, \
                                                pedal_linear, tc_off, abs_off, abs_low) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
                         ON CONFLICT (user_id, gp_index, version_code) \
                         DO UPDATE SET lap_ms = EXCLUDED.lap_ms, updated_at = now(), \
                                       pedal_linear = EXCLUDED.pedal_linear, \
                                       tc_off = EXCLUDED.tc_off, \
                                       abs_off = EXCLUDED.abs_off, \
                                       abs_low = EXCLUDED.abs_low",
                    )
                    .bind(user_id)
                    .bind(lap.gp_index)
                    .bind(lap.version_code)
                    .bind(lap.lap_ms)
                    .bind(flags.pedal_linear)
                    .bind(flags.tc_off)
                    .bind(flags.abs_off)
                    .bind(flags.abs_low)
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

    // 高频事件降采样：普通圈不入日志，只有破纪录（个人/全服任一维度）才记，
    // 防止 app_logs 被 205 人的每一圈刷爆（保留策略 90 天也扛不住全量）。
    if toast.is_some() {
        let (username,) = sqlx::query_as::<_, (String,)>("SELECT username FROM users WHERE id=$1")
            .bind(user_id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or((String::new(),));
        crate::applog::log_event(
            &state.pool, "info", "lap", "lap_record", &username,
            format!(
                "{} 刷新{}纪录：{} · {}",
                username,
                match toast.as_ref().unwrap().level {
                    "alltime_server" => "全服历史",
                    "version_server" => "全服版本",
                    "alltime_personal" => "个人历史",
                    _ => "个人版本",
                },
                track_display_name(lap.gp_index),
                crate::api::leaderboard::format_lap_ms(lap.lap_ms),
            ),
            serde_json::json!({"gp": lap.gp_index, "lap_ms": lap.lap_ms, "toast_level": toast.as_ref().unwrap().level}),
        );
    }

    // 全服纪录播报（事务外）：两事件分立，历史优先于版本；失败不影响上传
    if server_best {
        crate::qq_bot::broadcast_lap_change(&state, lap.gp_index, lap.version_code, lap.lap_ms).await;
    }

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

/// 游戏 versionCode → 版本名（管理端展示用；新版本上市时在此追加映射）。
pub fn version_display(version_code: i32) -> String {
    match version_code {
        200150 => "8.0.6".to_string(),
        200146 => "8.0.4".to_string(),
        other => format!("code {other}"),
    }
}

/// `laps` 配置五维（管理端"配置"列与模块回读共用）→ 与 [`crate::score::AssistFlags`] 同规则。
pub fn flags_of(
    pedal_mode: Option<&str>,
    tc_mode: Option<&str>,
    tc_strength: Option<&str>,
    abs_mode: Option<&str>,
    abs_strength: Option<&str>,
) -> score::AssistFlags {
    let cfg = score::AssistConfig {
        pedal_mode: pedal_mode.map(str::to_string),
        tc_mode: tc_mode.map(str::to_string),
        tc_strength: tc_strength.map(str::to_string),
        abs_mode: abs_mode.map(str::to_string),
        abs_strength: abs_strength.map(str::to_string),
    };
    score::AssistFlags::from(&cfg)
}

/// 「配置缺失」的**单一判据**（2026-10-20 用户定案：任一维缺失即算）。
///
/// 五维全齐才算完整：`pedal_mode` 一维 + TC 两维 + ABS 两维。
///
/// ⚠️ **为什么不能只看派生位**：`best_laps` 的四个布尔位是派生结论，
///    `pedal_mode='off'`（原生按键）、`tc_mode='default'`（游戏默认）都会让
///    对应位为 `false`——与「缺失」在派生位上不可区分。判「缺失」必须回到
///    `laps` 的原始五列。
///
/// ⚠️ **TC/ABS 的 mode 与 strength 必须成对齐全**：`mode` 有值而 `strength`
///    为 NULL 是管理端补录写了一半的状态，同样是「配置不全」。
///
/// **调用方**：`upload_lap` 的自动屏蔽闸门（2026-10-20）——那里手上就是五维
/// `Option<&str>`，走 SQL 反而要再查一次库。其余统计/筛选用 [`ASSIST_MISSING_SQL`]。
/// 两版由 `tests::sql_predicate_covers_all_five_columns` 锁死逐字对应。
pub fn is_assist_missing(
    pedal_mode: Option<&str>,
    tc_mode: Option<&str>,
    tc_strength: Option<&str>,
    abs_mode: Option<&str>,
    abs_strength: Option<&str>,
) -> bool {
    pedal_mode.is_none()
        || tc_mode.is_none()
        || tc_strength.is_none()
        || abs_mode.is_none()
        || abs_strength.is_none()
}

/// 同上，但以 SQL 布尔表达式形式返回（筛选/统计用，避免 Rust 侧拉全量回筛）。
///
/// 与 [`is_assist_missing`] **逐字对应**：任一列 IS NULL 即为缺失。
/// 两处若分叉，"能不能屏蔽"的预判与实际屏蔽的行集就会不一致。
pub const ASSIST_MISSING_SQL: &str = "(l.pedal_mode IS NULL OR l.tc_mode IS NULL \
     OR l.tc_strength IS NULL OR l.abs_mode IS NULL OR l.abs_strength IS NULL)";

/// 「该圈此刻应被自动屏蔽」的 SQL 谓词 —— **屏蔽判据的唯一事实源**。
///
/// ## 语义（2026-10-20 二次定案：从「快照」升级为「持续生效的开关」）
///
/// 用户开启屏蔽后，`users.hidden_missing_at` 就是那个开关；**开关打开期间，
/// 配置缺失的圈一律被屏蔽**，与它是什么时候上传的无关：
///
/// - 补全配置 → 谓词变假 → **自动解除**（管理端编辑保存即生效）；
/// - 之后旧版模块再传缺失圈 → 谓词为真 → **自动屏蔽**（用户定案原话）。
///
/// 首版是「快照」语义（按 `lap_id` 记进 `hidden_missing_laps`，屏蔽后新传的
/// 缺失圈照常进榜）。用户在实机看到「管理端补全配置后红标不消失」后改为本条，
/// 因为快照语义下屏蔽状态与配置**永久解耦**，任何一侧变化都不会互相影响。
///
/// ## 为什么是谓词而不是表
///
/// 谓词天然对"配置变化"和"新圈上传"两个方向都即时生效，不需要在每条写入
/// 路径上手工维护一份状态表。`hidden_missing_laps` 因此降级为**纯审计记录**
/// （谁在哪一刻屏蔽了哪些圈），不再参与任何判据。
///
/// ## 用法
///
/// 需要 `laps l` 与 `users u` 同时在 FROM 里（JOIN 或子查询），且 `u.id = l.user_id`。
/// 列表查询里 `l.user_id` 就是本人，故写成相关子查询也成立：
///
/// ```sql
/// EXISTS (SELECT 1 FROM users u WHERE u.id = l.user_id AND {AUTO_HIDDEN_SQL})
/// ```
pub const AUTO_HIDDEN_SQL: &str = "(u.hidden_missing_at IS NOT NULL AND \
     (l.pedal_mode IS NULL OR l.tc_mode IS NULL OR l.tc_strength IS NULL \
      OR l.abs_mode IS NULL OR l.abs_strength IS NULL))";

/// 踏板档位 token → 展示名（管理端下拉与"配置"列共用同一张表）。
pub const PEDAL_TOKENS: [(&str, &str); 3] =    [("off", "原生按键"), ("single", "单踏板"), ("dual", "双踏板")];

/// TC/ABS 档位 token → 展示名。`stock` = 最高档，也代表"未开自定义（游戏默认）"。
pub const STRENGTH_TOKENS: [(&str, &str); 5] = [
    ("off", "关"),
    ("weak", "低"),
    ("medium", "中"),
    ("strong", "高"),
    ("stock", "最高"),
];

/// token → 展示名。**唯一查表入口**：`&str` 是 `'static`，返回的 `&'static str`
/// 来自常量表，不借用入参生命周期（写成 `&[(&str,&str)]` 会被 elided lifetime
/// 绑到入参上而报 E0311 一类错误）。
pub fn token_label(tokens: &'static [(&'static str, &'static str)], token: &str) -> &'static str {
    if token.is_empty() {
        return "缺失";
    }
    tokens
        .iter()
        .find(|(k, _)| *k == token)
        .map(|(_, d)| *d)
        .unwrap_or("缺失")
}

/// TC/ABS 的**模式 + 档位合并成单一 token**——这是管理端展示与编辑的粒度。
///
/// 为什么合并：`mode=default`（游戏默认透传）与 `mode=custom + strength=stock`
/// 在**计分上完全等价**（都不加分），用户诉求也是"默认就是最高"。分两维展示
/// 只会让"配置"列多出一列没人看的"默认/自定义"，纯占地方。
///
/// 数据面仍存原始两维（`laps.tc_mode` / `tc_strength`）——合并只发生在展示层。
fn collapse_mode(mode: Option<&str>, strength: Option<&str>) -> String {
    match mode {
        None => String::new(),                        // 缺失
        Some("default") => "stock".to_string(),       // 默认 ≡ 最高
        Some(_) => strength.unwrap_or("").to_string(), // custom → 用真实档位
    }
}

/// 辅助配置 → 三个 token（踏板 / TC / ABS），`""` = 缺失。
///
/// 这是管理端"配置"列与编辑下拉的**同一份数据源**：下拉的 option value 就是
/// token，展示名由 [`token_label`] 查表，读写两侧不会漂移。
pub fn assist_tokens(
    pedal_mode: Option<&str>,
    tc_mode: Option<&str>,
    tc_strength: Option<&str>,
    abs_mode: Option<&str>,
    abs_strength: Option<&str>,
) -> (String, String, String) {
    (
        pedal_mode.unwrap_or("").to_string(),
        collapse_mode(tc_mode, tc_strength),
        collapse_mode(abs_mode, abs_strength),
    )
}

/// 辅助配置 → 展示三元组（`"双踏板" / "关" / "最高"` 的逐维读数）。
pub fn assist_display(
    pedal_mode: Option<&str>,
    tc_mode: Option<&str>,
    tc_strength: Option<&str>,
    abs_mode: Option<&str>,
    abs_strength: Option<&str>,
) -> (String, String, String) {
    let (p, tc, abs) = assist_tokens(pedal_mode, tc_mode, tc_strength, abs_mode, abs_strength);
    (
        token_label(&PEDAL_TOKENS, &p).to_string(),
        token_label(&STRENGTH_TOKENS, &tc).to_string(),
        token_label(&STRENGTH_TOKENS, &abs).to_string(),
    )
}

/// 三个 token → `(pedal_mode, tc_mode, tc_strength, abs_mode, abs_strength)` 原始五维。
///
/// 编辑弹窗提交的 token 经此展开回数据库列。`""`（缺失）= 该维两列都 NULL；
/// `"stock"` = `mode=default`（**游戏默认**，而非"开自定义选了最高"——后者在
/// 计分上等价但与用户语义不符：管理员选"最高"的意思是"这圈没调低辅助"）。
pub fn tokens_to_raw(
    pedal: &str,
    tc: &str,
    abs: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let pedal_raw = if pedal.is_empty() { None } else { Some(pedal.to_string()) };
    let expand = |t: &str| -> (Option<String>, Option<String>) {
        if t.is_empty() {
            (None, None)
        } else if t == "stock" {
            (Some("default".to_string()), Some("stock".to_string()))
        } else {
            (Some("custom".to_string()), Some(t.to_string()))
        }
    };
    let (tcm, tcs) = expand(tc);
    let (absm, abss) = expand(abs);
    (pedal_raw, tcm, tcs, absm, abss)
}

/// 赛道中文名，带国旗前缀（契约源头=PADDOCK_PLAN.md §5；模块侧同名表保持一致，改必须两边同改）。
pub fn track_display_name(gp_index: i16) -> &'static str {
    match gp_index {
        0 => "🇦🇺 阿尔伯特公园赛道",
        1 => "🇨🇳 上海国际赛车场",
        2 => "🇧🇭 巴林国际赛车场",
        3 => "🇮🇹 伊莫拉赛道",
        4 => "🇪🇸 加泰罗尼亚赛道",
        5 => "🇲🇨 摩纳哥赛道",
        6 => "🇨🇦 吉尔·维伦纽夫赛道",
        7 => "🇦🇹 红牛环赛道",
        8 => "🇬🇧 银石赛道",
        9 => "🇩🇪 霍根海姆赛道",
        10 => "🇭🇺 亨格罗宁赛道",
        11 => "🇧🇪 斯帕-弗朗科尔尚赛道",
        12 => "🇮🇹 蒙扎国家赛车场",
        13 => "🇯🇵 铃鹿赛道",
        14 => "🇧🇷 英特拉格斯赛道",
        15 => "🇦🇪 迪拜赛车场",
        _ => "未知赛道",
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/laps", post(upload_lap))
}

#[cfg(test)]
mod tests {
    use super::is_assist_missing;

    /// 五维齐全 = 完整。注意 `pedal_mode=off` / `tc_mode=default` 是**有效值**，
    /// 不是缺失——这正是"不能只看 best_laps 派生位"的原因。
    #[test]
    fn complete_config_is_not_missing() {
        assert!(!is_assist_missing(
            Some("off"),
            Some("default"),
            Some("stock"),
            Some("custom"),
            Some("off"),
        ));
    }

    /// 任一维 NULL 即算缺失（用户定案 2026-10-20）——逐维各测一遍，
    /// 防止将来有人把某个字段从判据里漏掉。
    #[test]
    fn any_null_dimension_is_missing() {
        let full = [Some("dual"), Some("custom"), Some("weak"), Some("custom"), Some("medium")];
        for i in 0..5 {
            let mut v = full;
            v[i] = None;
            assert!(
                is_assist_missing(v[0], v[1], v[2], v[3], v[4]),
                "第 {i} 维为 None 时应判缺失"
            );
        }
    }

    /// 全缺 = 缺失（旧模块 `LapAssist.MISSING` 上报的形态）。
    #[test]
    fn all_null_is_missing() {
        assert!(is_assist_missing(None, None, None, None, None));
    }

    /// 部分缺失（管理端补录写了一半）也是缺失。
    #[test]
    fn partial_config_is_missing() {
        assert!(is_assist_missing(
            Some("dual"),
            Some("custom"),
            None, // strength 没填
            Some("custom"),
            Some("off"),
        ));
    }

    /// SQL 判据与 Rust 判据必须逐字对应：五个列名都出现在表达式里。
    /// 两处分叉会让"预判能不能屏蔽"与实际屏蔽的行集不一致。
    #[test]
    fn sql_predicate_covers_all_five_columns() {
        for col in ["pedal_mode", "tc_mode", "tc_strength", "abs_mode", "abs_strength"] {
            assert!(
                super::ASSIST_MISSING_SQL.contains(col),
                "SQL 判据漏了 {col}"
            );
        }
        assert_eq!(super::ASSIST_MISSING_SQL.matches("IS NULL").count(), 5);
    }
}
