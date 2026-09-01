//! 头像端点：S3（Garage）对象存储 + /v1/me/avatar 上传 / /v1/avatar/{user_id} 下载。
//!
//! 设计（2026-09-01 定案）：
//! - 上传：Bearer 登录态，PUT /v1/me/avatar（body=裁剪后的 JPEG/PNG，≤2MB）→
//!   SigV4 PutObject 到 Garage `avatars/{user_id}` → users.avatar_key 落库。
//! - 下载：公开（无鉴权，榜单/个人卡展示用）GET /v1/avatar/{user_id}。
//!   404 时返回 302 → 模块内置占位图（客户端本地 fallback）。
//! - SigV4 手写（约 80 行）：只服务 PutObject/GetObject 两个形态；
//!   AWS SDK 全家桶对 opt-level="z" 二进制体积不友好。
//!
//! Garage 凭据：环境变量 GARAGE_KEY_ID/GARAGE_SECRET（compose app 容器注入），
//! endpoint = 环境变量 GARAGE_ENDPOINT（默认 http://garage:3900）。
//! 首次部署需 `garage status` 造 key + bucket（见 README 部署章节）。

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
    state::AppState,
};

const MAX_AVATAR_BYTES: usize = 2 * 1024 * 1024; // 2MB（裁剪后足够）
const GARAGE_BUCKET: &str = "avatars";

// ---------- S3 SigV4（PutObject / GetObject 专用最小实现） ----------

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
    let Some(cfg) = s3_config() else {
        tracing::error!("GARAGE_KEY_ID/GARAGE_SECRET 未配置，头像功能不可用");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "头像服务未配置".into()));
    };
    let key = format!("avatars/{user_id}");
    s3_put(&cfg, &key, &content_type, &body)
        .await
        .map_err(|msg| (StatusCode::INTERNAL_SERVER_ERROR, msg))?;
    sqlx::query("UPDATE users SET avatar_key = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&key)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("avatar_key 落库失败: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "服务器开小差了".into())
        })?;
    Ok(Json(AvatarResp {
        uploaded: true,
        url: format!("/v1/avatar/{user_id}"),
    }))
}

/// GET /v1/avatar/{user_id}：下载头像（公开）。有 avatar_key 才代理 Garage，
/// 否则 404（客户端本地占位图 fallback）。
pub async fn get_avatar(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> Response {
    let avatar_key: Option<String> = sqlx::query_scalar(
        "SELECT avatar_key FROM users WHERE id = $1 AND avatar_key IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(_) = avatar_key else {
        return (StatusCode::NOT_FOUND, "无头像").into_response();
    };
    let Some(cfg) = s3_config() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "头像服务未配置").into_response();
    };
    // GetObject SigV4：与 Put 同构（payload 空串哈希 UNSIGNED-PAYLOAD 简化为空串哈希）
    let key = format!("avatars/{user_id}");
    match s3_get(&cfg, &key).await {
        Ok((content_type, bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("image/jpeg")));
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => {
            tracing::warn!("S3 GetObject {key} 失败: {e}");
            (StatusCode::NOT_FOUND, "头像读取失败").into_response()
        }
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
    let has: Option<String> = sqlx::query_scalar(
        "SELECT avatar_key FROM users WHERE id = $1 AND avatar_key IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .flatten();
    Ok(Json(AvatarResp {
        uploaded: has.is_some(),
        url: format!("/v1/avatar/{user_id}"),
    }))
}