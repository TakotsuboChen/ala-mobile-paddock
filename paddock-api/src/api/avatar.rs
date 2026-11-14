//! 头像端点：对象存储（默认 Garage，可切阿里云 OSS）+ /v1/me/avatar 上传 /
//! /v1/avatar/{user_id} 下载。
//!
//! 设计（2026-09-01 定案；2026-11-14 加入 OSS 直连）：
//! - 上传：Bearer 登录态，PUT /v1/me/avatar（body=裁剪后的 JPEG/PNG，≤2MB）→
//!   写对象存储 → users.avatar_key 落库。
//! - 下载：公开（无鉴权，榜单/个人卡展示用）GET /v1/avatar/{user_id}。
//!   **OSS 已配置时这条端点仍保留**，作为旧版模块（写死相对 URL）的兼容路径；
//!   新版模块拿到的是 OSS 绝对 URL，直连对象存储、不经过本服务。
//! - SigV4 手写：Garage 走 S3 SigV4（本文件），OSS 走 OSS V4（见 [`crate::oss`]）。
//!
//! ## 存储后端选择（2026-11-14）
//! `configs` 表里有完整 OSS 配置 → 用 OSS；否则回落 Garage 环境变量。
//! 回落的意义：迁移是**可逆的两步走**，配置填好之前系统行为完全不变。
//!
//! ## 头像 key 的形态
//! `avatars/{user_id}/{version}.jpg`——版本号在**路径**里，不在 query。
//! 原因见 [`crate::oss::public_url`]：URL 变即内容变，客户端磁盘缓存语义不变，
//! 且不依赖任何服务端对查询串的解析承诺。
//!
//! Garage 时代的 key 写作 `avatars/{user_id}`（无版本段），迁移任务负责把旧
//! 对象搬到新形态；`users.avatar_key` 是唯一清单来源，无需 ListObjects。

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    api::laps,
    oss,
    state::AppState,
};

const MAX_AVATAR_BYTES: usize = 2 * 1024 * 1024; // 2MB（裁剪后足够）
const GARAGE_BUCKET: &str = "avatars";

// ---------- S3 SigV4（Garage：PutObject / GetObject 专用最小实现） ----------
//
// ⚠️ 这套是 **AWS SigV4**，与 OSS V4 不兼容（host 参与签名、真实 payload 哈希、
//    `AWS4` 前缀）。OSS 走 `crate::oss`，不要试图合并两份实现。

struct S3Config {
    endpoint: String, // http://garage:3900
    region: String,   // garage.toml s3_region = "paddock"
    key_id: String,
    secret: String,
}

fn s3_config() -> Option<S3Config> {
    let key_id = std::env::var("GARAGE_KEY_ID").ok()?;
    let secret = std::env::var("GARAGE_SECRET").ok()?;
    if key_id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(S3Config {
        endpoint: std::env::var("GARAGE_ENDPOINT")
            .unwrap_or_else(|_| "http://garage:3900".into()),
        region: std::env::var("GARAGE_REGION").unwrap_or_else(|_| "paddock".into()),
        key_id,
        secret,
    })
}

/// ISO8601 基本格式时间戳（SigV4 x-amz-date），如 20260901T120000Z。
fn amz_date(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key 任意长度合法");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// path-style 请求 URL（Garage 不做 virtual-host style）。
fn s3_url(cfg: &S3Config, key: &str) -> String {
    format!("{}/{}/{}", cfg.endpoint.trim_end_matches('/'), GARAGE_BUCKET, key)
}

/// 签名并执行 PutObject。返回 Ok(()) 或错误文案。
async fn s3_put(cfg: &S3Config, key: &str, content_type: &str, body: &[u8]) -> Result<(), String> {
    let now = chrono::Utc::now();
    let date = amz_date(now);
    let date_stamp = date[..8].to_string();
    let url = s3_url(cfg, key);
    let parsed = reqwest::Url::parse(&url).map_err(|e| format!("URL 解析失败: {e}"))?;
    let host = format!(
        "{}:{}",
        parsed.host_str().unwrap_or("garage"),
        parsed.port().unwrap_or(80)
    );
    let payload_hash = sha256_hex(body);

    // canonical request（PutObject：host + 4 个 x-amz 头 + content-type）
    let canonical_headers = format!(
        "content-type:{content_type}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{date}\n"
    );
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "PUT\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        path = parsed.path(),
    );
    // string to sign
    let scope = format!("{date_stamp}/{region}/s3/aws4_request", region = cfg.region);
    let sts = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    // signing key：date → region → service → aws4_request
    let k_date = hmac_sha256(format!("AWS4{secret}", secret = cfg.secret).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, cfg.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, sts.as_bytes()));

    let resp = reqwest::Client::new()
        .put(&url)
        .header("Content-Type", content_type)
        .header("x-amz-content-sha256", &payload_hash)
        .header("x-amz-date", &date)
        .header(
            "Authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                key_id = cfg.key_id,
            ),
        )
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("存储服务连接失败: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::error!("S3 PutObject {key} 失败 {status}: {text}");
        Err("存储服务写入失败，请稍后再试".into())
    }
}

/// 签名并执行 GetObject。返回 (content_type, body)。
async fn s3_get(cfg: &S3Config, key: &str) -> Result<(String, Bytes), String> {
    let now = chrono::Utc::now();
    let date = amz_date(now);
    let date_stamp = date[..8].to_string();
    let url = s3_url(cfg, key);
    let parsed = reqwest::Url::parse(&url).map_err(|e| format!("URL 解析失败: {e}"))?;
    let host = format!(
        "{}:{}",
        parsed.host_str().unwrap_or("garage"),
        parsed.port().unwrap_or(80)
    );
    let payload_hash = sha256_hex(b""); // GET 无 body

    let canonical_headers = format!(
        "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{date}\n"
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "GET\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        path = parsed.path(),
    );
    let scope = format!("{date_stamp}/{region}/s3/aws4_request", region = cfg.region);
    let sts = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(format!("AWS4{secret}", secret = cfg.secret).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, cfg.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, sts.as_bytes()));

    let resp = reqwest::Client::new()
        .get(&url)
        .header("x-amz-content-sha256", &payload_hash)
        .header("x-amz-date", &date)
        .header(
            "Authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                key_id = cfg.key_id,
            ),
        )
        .send()
        .await
        .map_err(|e| format!("存储服务连接失败: {e}"))?;
    if resp.status().is_success() {
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| format!("读取失败: {e}"))?;
        Ok((content_type, Bytes::from(bytes.to_vec())))
    } else {
        Err(format!("S3 状态码 {}", resp.status()))
    }
}

// ---------- 头像 key 与 URL（单一事实源） ----------

/// 头像对象 key。**唯一构造点**——所有写入、迁移、URL 拼装都必须走这里，
/// 否则格式一变就有地方漏改。
///
/// `version` 参与 key：换头像产生新对象，旧对象成为孤儿（迁移任务会清）。
/// 这让「URL 不变却指向新内容」在结构上不可能发生。
pub fn avatar_key(user_id: Uuid, version: i64) -> String {
    format!("avatars/{user_id}/{version}.jpg")
}

/// 下发给客户端的头像 URL。`None` = 该用户没有头像。
///
/// 分三种情况，**由对象实际所在位置决定**（不是由配置决定）：
/// 1. 存量未迁移（key = `avatars/{uuid}`）→ 相对路径，走本服务代理（读 Garage）
/// 2. 已迁移 + OSS 已配置（key = `avatars/{uuid}/{v}.jpg`）→ **OSS 绝对 URL，
///    客户端直连**（老模块也支持：`fetchAvatar` 从 2026-09-01 起就有
///    `startsWith("http")` 分支，所以不需要发新版模块）
/// 3. 新上传（key 天然是新形态）+ OSS 已配置 → 同 2
///
/// `version` 由调用方从 `users.avatar_version` 取。
/// ⚠️ **不要从 `key` 里反解版本号**：存量 key 末段就是 UUID 本身，那样拼出来是
///    `?v=01a06d14-...`（同日第二个 bug：代理端点忽略 v 所以能跑，但版本号永不
///    变化 ⇒ 换头像后客户端一直显示旧图）。
pub fn avatar_url(
    oss_cfg: Option<&oss::OssConfig>,
    migrated: bool,
    user_id: Uuid,
    key: &str,
    version: i64,
) -> String {
    match oss_cfg {
        // ⚠️ 判据是「**迁移已完成**」，不是「key 是新形态」。
        //    key 形态由代码写死、与对象实际在哪无关：新 `upload_avatar` 一上线
        //    就往 Garage 写新形态 key，靠形态判断会误把 Garage 对象当成 OSS 对象
        //    ⇒ URL 指向 OSS ⇒ 404（2026-11-14 全站头像消失事故）。
        Some(cfg) if migrated => oss::public_url(cfg, key),
        _ => format!("/v1/avatar/{user_id}?v={version}"),
    }
}

/// 迁移是否已完成（URL 是否该切到 OSS 直连）。
///
/// 放在这里而不是每次查库：调用方（榜单/车手主页/`/v1/me`）都是一次请求多处
/// 用到，统一在 handler 开头查一次再传下来。
pub async fn is_migrated(pool: &sqlx::PgPool) -> bool {
    crate::qq_bot::get_cfg(pool, oss::keys::MIGRATED)
        .await
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ---------- HTTP handlers ----------

#[derive(Serialize)]
pub struct AvatarResp {
    pub uploaded: bool,
    pub url: String,
}

/// POST /v1/me/avatar：上传头像（Bearer，body=图片字节）。
pub async fn upload_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AvatarResp>, (StatusCode, String)> {
    let user_id = laps::authenticate(&state, &headers)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;
    if body.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "图片内容为空".into()));
    }
    if body.len() > MAX_AVATAR_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "图片超过 2MB 限制".into()));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    if content_type != "image/jpeg" && content_type != "image/png" {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "仅支持 JPEG/PNG".into()));
    }
    let oss_cfg = oss::config_of(&state).await;
    let garage_cfg = s3_config();
    if oss_cfg.is_none() && garage_cfg.is_none() {
        tracing::error!("对象存储未配置（OSS configs 与 GARAGE_* 环境变量均缺失），头像功能不可用");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "头像服务未配置".into()));
    }
    // 冷却检查（2026-10-20 用户定案：30 天只能换一次）。
    // ⚠️ 检查必须在对象写入**之前**：先传后拒会把存储里的对象换掉，
    //    而 users.avatar_key 没更新——URL 版本号不变却指向新内容，客户端
    //    磁盘缓存（按 URL 版本失效）会一直显示旧图，实为"上传了但不生效"。
    let last: Option<Option<chrono::DateTime<chrono::Utc>>> =
        sqlx::query_scalar("SELECT avatar_updated_at FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("avatar cooldown 查询失败: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "服务器开小差了".into())
            })?;
    let remaining: i64 = crate::api::account::cooldown_remaining(last.flatten());
    if remaining > 0 {
        // 向上取整到天（本工具链的 Rust 版本未稳定 `div_ceil`）。
        let days = (remaining + 86399) / 86400;
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("头像 30 天内只能更换一次，还需 {days} 天"),
        ));
    }
    // avatar_version = epoch millis：既是 key 的一部分，也是客户端的缓存失效开关。
    let version = chrono::Utc::now().timestamp_millis();
    let key = avatar_key(user_id, version);

    if let Some(cfg) = &oss_cfg {
        oss::put_object(cfg, &key, &content_type, &body)
            .await
            .map_err(|msg| (StatusCode::INTERNAL_SERVER_ERROR, msg))?;
    } else if let Some(cfg) = &garage_cfg {
        s3_put(cfg, &key, &content_type, &body)
            .await
            .map_err(|msg| (StatusCode::INTERNAL_SERVER_ERROR, msg))?;
    }

    sqlx::query(
        "UPDATE users SET avatar_key = $2, avatar_version = $3, avatar_updated_at = now() \
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(&key)
    .bind(version)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("avatar_key 落库失败: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "服务器开小差了".into())
        })?;
    Ok(Json(AvatarResp {
        uploaded: true,
        url: avatar_url(oss_cfg.as_ref(), is_migrated(&state.pool).await, user_id, &key, version),
    }))
}

/// GET /v1/avatar/{user_id}：下载头像（公开）。
///
/// **OSS 已配置时的兼容路径**：新版模块拿绝对 URL 直连，不再请求这里；但旧版
/// 模块（写死相对路径）仍会打到这条端点——所以它必须继续工作，只是改为
/// 302 跳转到 OSS（或直接代理字节）。这里选择**302 跳转**：
/// 让流量从 OSS 出而不是从 VPS 出，且客户端缓存行为与直连一致。
pub async fn get_avatar(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> Response {
    let key: Option<String> = sqlx::query_scalar(
        "SELECT avatar_key FROM users WHERE id = $1 AND avatar_key IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(key) = key else {
        return (StatusCode::NOT_FOUND, "无头像").into_response();
    };

    // 判据是「迁移已完成」，不是 key 形态（key 形态与对象位置无关，见
    // `avatar_url` 的注释）。迁移没跑完就让客户端 302 到 OSS = 404。
    if let Some(cfg) = oss::config_of(&state).await {
        if is_migrated(&state.pool).await {
            let url = oss::public_url(&cfg, &key);
            let mut resp_headers = HeaderMap::new();
            resp_headers.insert(
                header::LOCATION,
                HeaderValue::from_str(&url).unwrap_or(HeaderValue::from_static("/")),
            );
            resp_headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=604800, immutable"),
            );
            return (StatusCode::FOUND, resp_headers).into_response();
        }
    }

    // 存量未迁移（或 OSS 未配置）→ 代理 Garage 读出字节。
    let Some(cfg) = s3_config() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "头像服务未配置").into_response();
    };
    match s3_get(&cfg, &key).await {
        Ok((content_type, bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("image/jpeg")));
            // 缓存控制：URL 已带版本号，同 URL 内容不变 → 允许长缓存。
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=604800, immutable"),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => {
            tracing::warn!("S3 GetObject {key} 失败: {e}");
            (StatusCode::NOT_FOUND, "头像读取失败").into_response()
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me/avatar", post(upload_avatar).get(get_my_avatar))
        .route("/avatar/{user_id}", get(get_avatar))
}

/// GET /v1/me/avatar：查自己是否有头像（客户端启动时同步 needsAvatar 用）。
async fn get_my_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AvatarResp>, (StatusCode, String)> {
    let user_id = laps::authenticate(&state, &headers)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;
    let has: Option<(String, i64)> = sqlx::query_as(
        "SELECT avatar_key, avatar_version FROM users \
         WHERE id = $1 AND avatar_key IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let oss_cfg = oss::config_of(&state).await;
    let url = match &has {
        Some((key, version)) => {
            avatar_url(oss_cfg.as_ref(), is_migrated(&state.pool).await, user_id, key, *version)
        }
        None => format!("/v1/avatar/{user_id}"),
    };
    Ok(Json(AvatarResp {
        uploaded: has.is_some(),
        url,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oss() -> oss::OssConfig {
        oss::OssConfig {
            endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
            bucket: "b".into(),
            region: "cn-hangzhou".into(),
            key_id: "k".into(),
            secret: "s".into(),
            public_base: "https://b.oss-cn-hangzhou.aliyuncs.com".into(),
        }
    }

    /// 状态机回归（2026-11-14 两次事故）：
    /// ① 配好 OSS 但没迁移 → URL 必须走代理（否则对象不存在 ⇒ 全站 404）
    /// ② **key 形态不能作为判据**——新代码会往 Garage 写「新形态 key」，
    ///    靠形态推断位置会把 Garage 对象误判成已迁移。
    #[test]
    fn url_follows_migration_state_not_key_shape() {
        let uid = Uuid::nil();
        let cfg = oss();

        // 已迁移 → OSS 绝对 URL（不论 key 长什么样）
        for key in [format!("avatars/{uid}/7.jpg"), format!("avatars/{uid}")] {
            assert_eq!(
                avatar_url(Some(&cfg), true, uid, &key, 7),
                format!("https://b.oss-cn-hangzhou.aliyuncs.com/{key}")
            );
        }

        // 未迁移 → 一律代理，**即使 key 是新形态**（这正是 231 张被误跳过的成因）
        for key in [format!("avatars/{uid}/7.jpg"), format!("avatars/{uid}")] {
            assert_eq!(
                avatar_url(Some(&cfg), false, uid, &key, 7),
                format!("/v1/avatar/{uid}?v=7"),
                "未迁移时不能指向 OSS（对象还在 Garage）"
            );
        }

        // 迁移完成但 OSS 配置被清空 → 回落代理，不至于全挂
        assert_eq!(
            avatar_url(None, true, uid, &format!("avatars/{uid}/7.jpg"), 7),
            format!("/v1/avatar/{uid}?v=7")
        );
    }
}
