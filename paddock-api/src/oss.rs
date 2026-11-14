//! 阿里云 OSS 客户端：V4 签名（OSS4-HMAC-SHA256）手写实现 + GetObject/PutObject。
//!
//! ## 为什么手写而不上官方 SDK
//! 二进制用 `opt-level="z" + lto + strip`（workspace `Cargo.toml`），官方 SDK 会带进
//! 整套 HTTP/重试/分片逻辑；本项目只需要「取一个对象」「放一个对象」两个形态。
//! 签名算法已对**阿里云官方测试向量**逐字节验证（见文件末尾 `tests::official_vector`）。
//!
//! ## 与 AWS SigV4 的三处实质差异（照抄 S3 代码必挂）
//! 1. **`host` 头不参与签名**。OSS V4 只签 `x-oss-*` 前缀头 + 默认签名头
//!    （`content-type` / `content-md5`）+ `AdditionalHeaders` 显式声明的头。
//! 2. **`x-oss-content-sha256` 只接受 `UNSIGNED-PAYLOAD`**，不做真实 payload 哈希。
//! 3. **密钥派生前缀是 `aliyun_v4`**（AWS 是 `AWS4`），终止串是 `aliyun_v4_request`。
//!
//! ## 只支持 virtual-hosted style
//! OSS **不接受 path-style URL**（`endpoint/bucket/key`），官方原文：「For security
//! reasons, OSS accepts only virtual hosted-style URLs, not path-style URLs」。
//! 因此请求域名恒为 `{bucket}.{endpoint}`，canonical URI 里**不含** bucket。
//!
//! ## 两套域名：服务端访问域名与客户端访问域名分开填
//! - `endpoint`：**服务端**访问 OSS 用的域名（只填 host，不填 scheme/bucket）。
//!   填公网域名（`oss-cn-XXXX.aliyuncs.com`）任何情况都能用。
//!   填内网域名（`oss-cn-XXXX-internal.aliyuncs.com`）**只有 bucket 与该 ECS
//!   同地域时**才可达，此时流量免费；跨地域内网域名**无法解析**，会直接连不上。
//! - `public_base`：下发给模块客户端的 URL 前缀（含 scheme）。必须用公网域名
//!   或已绑定的自定义域名——内网域名不对外解析，客户端填了必挂。
//!
//! ⚠️ 两个域名由设置页填写，本模块**不做任何地域假设**。
//! ⚠️ 混用的两种浪费：服务端填公网域名 = 白付一遍外网流量（上传方向本来就
//!    免费，所以只有在服务端需要**下载**对象时才产生费用）；客户端填内网域名
//!    = 完全连不上。

use std::sync::OnceLock;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::state::AppState;

/// 共享 HTTP 客户端：复用连接池。
///
/// ⚠️ 不能用 `reqwest::Client::new()` 每次新建——迁移 228 张头像会做 228 次
/// TCP + TLS 握手。这里同时显式设置超时：头像单张最大 2MB，20 秒足够；
/// 不设超时会让迁移任务在 OSS 抖动时永久挂起且无进度。
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("构建 HTTP 客户端失败")
    })
}

// ---------- 配置 ----------

/// OSS 连接配置。全部来自 `configs` 表（管理端设置页可改，无需重启）。
#[derive(Clone)]
pub struct OssConfig {
    /// 服务端访问用的 endpoint（**不含 scheme、不含 bucket**），如
    /// `oss-cn-hangzhou.aliyuncs.com`（公网）。填内网域名仅同地域可用。
    pub endpoint: String,
    /// Bucket 名（只允许 DNS 兼容字符，下划线会让 virtual-hosted 域名非法）。
    pub bucket: String,
    /// 地域 ID，如 `cn-hangzhou`。Scope 与密钥派生都要用，**必须与 bucket 实际
    /// 所在地域一致**（填错会 403 SignatureDoesNotMatch，不是区域不支持）。
    pub region: String,
    pub key_id: String,
    pub secret: String,
    /// 下发给客户端的 URL 前缀（含 scheme）。
    pub public_base: String,
}

/// `configs` 表的键名。集中定义，避免管理端与业务侧各写一份字符串。
pub mod keys {
    pub const ENDPOINT: &str = "oss_endpoint";
    pub const BUCKET: &str = "oss_bucket";
    pub const REGION: &str = "oss_region";
    pub const KEY_ID: &str = "oss_key_id";
    pub const SECRET: &str = "oss_key_secret";
    pub const PUBLIC_BASE: &str = "oss_public_base";
    /// 迁移完成标记（"1" = 全部头像已在 OSS）。**URL 直连的唯一开关**。
    ///
    /// 为什么需要它：`avatar_key` 只记 key，不记对象在哪。新代码一上线就会
    /// 往 Garage 写「新形态 key」，靠 key 形态推断位置会误判（2026-11-14 事故）。
    /// 迁移是整体动作 —— 跑完写这个标记，URL 才整体切到 OSS 直连。
    ///
    /// ⚠️ 标记必须由**迁移任务在该轮全部成功后**写入，不能中途写：中途写会让
    ///    还没搬的对象不可达（URL 指向 OSS，对象还在 Garage）。
    pub const MIGRATED: &str = "oss_migrated";
}

/// 读一条记录（空串视为未配置）。
async fn get(pool: &sqlx::PgPool, key: &str) -> Option<String> {
    crate::qq_bot::get_cfg(pool, key)
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 从 `configs` 表加载完整配置。任必填项缺失 → `None`（功能未启用，不是错误）。
///
/// `public_base` 可省略：缺省时按 `https://{bucket}.{endpoint}` 推导。但服务端
/// 用内网 endpoint 时这个推导是**错的**（内网域名不对外解析），所以设置页应当
/// 显式填写；此处推导只为兼容「服务端和客户端都走公网」的简单部署。
pub async fn load_config(pool: &sqlx::PgPool) -> Option<OssConfig> {
    let endpoint = get(pool, keys::ENDPOINT).await?;
    let bucket = get(pool, keys::BUCKET).await?;
    let region = get(pool, keys::REGION).await?;
    let key_id = get(pool, keys::KEY_ID).await?;
    let secret = get(pool, keys::SECRET).await?;
    let public_base = get(pool, keys::PUBLIC_BASE)
        .await
        .unwrap_or_else(|| format!("https://{bucket}.{endpoint}"));
    Some(OssConfig {
        endpoint: normalize_endpoint(&endpoint),
        bucket,
        region,
        key_id,
        secret,
        public_base: public_base.trim_end_matches('/').to_string(),
    })
}

/// endpoint 允许用户粘贴带 scheme 或带 bucket 的形态，统一规整成裸 host。
fn normalize_endpoint(raw: &str) -> String {
    let s = raw.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

// ---------- 签名 ----------

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC 密钥任意长度合法");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// OSS 要求的 URI 编码：仅 `A-Za-z0-9-._~` 不编码，空格编成 `%20`（不是 `+`）。
/// `encode_slash = false` 时保留 `/`（资源路径用）。
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `x-oss-date` 与 Scope 里的日期：ISO8601 基本格式（`20250411T064124Z`）。
fn iso8601(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// 派生签名密钥链：
/// `HMAC("aliyun_v4"+SK, date) → region → "oss" → "aliyun_v4_request"`。
fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k = hmac(format!("aliyun_v4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"oss");
    hmac(&k, b"aliyun_v4_request")
}

/// 构造规范化请求（CanonicalRequest）。
///
/// 格式（官方）：
/// `VERB \n URI \n Query \n Headers \n AdditionalHeaders \n HashedPayload`
/// 其中 Headers 自身以 `\n` 结尾 ⇒ 与 AdditionalHeaders 之间会出现一个空行。
/// 有 AdditionalHeaders 时是 `...\n\n额外头`，无时是 `...\n\n\nUNSIGNED-PAYLOAD`。
fn canonical_request(
    verb: &str,
    canonical_uri: &str,
    canonical_query: &str,
    canonical_headers: &str,
    additional_headers: &str,
) -> String {
    format!(
        "{verb}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n\
         {additional_headers}\nUNSIGNED-PAYLOAD"
    )
}

fn signature(cfg: &OssConfig, date_stamp: &str, timestamp: &str, canonical_request: &str) -> String {
    let scope = format!("{date_stamp}/{}/oss/aliyun_v4_request", cfg.region);
    let sts = format!(
        "OSS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    hex::encode(hmac(
        &signing_key(&cfg.secret, date_stamp, &cfg.region),
        sts.as_bytes(),
    ))
}

/// virtual-hosted style 的对象 URL（服务端访问用）。
fn object_url(cfg: &OssConfig, key: &str) -> String {
    format!(
        "https://{}.{}/{}",
        cfg.bucket,
        cfg.endpoint,
        uri_encode(key, false)
    )
}

/// 对象的**公网** URL（下发给模块客户端）。
///
/// ⚠️ 版本号编进**路径**而非 query：`?v=` 在当前架构下安全是因为解析方是自家
/// axum handler；换成 OSS 后解析方变成阿里云，其查询参数是白名单式的
/// （`versionId` / `response-*` 等），未知参数的行为无官方承诺。路径里带版本
/// 则「没有东西可解析」，跨厂商稳，且缓存语义完全不变（URL 变 = 内容变）。
pub fn public_url(cfg: &OssConfig, key: &str) -> String {
    format!("{}/{}", cfg.public_base, uri_encode(key, false))
}

// ---------- 对象操作 ----------

/// PutObject。`key` 形如 `avatars/{uuid}/{version}.jpg`。
pub async fn put_object(
    cfg: &OssConfig,
    key: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let timestamp = iso8601(now);
    let date_stamp = &timestamp[..8];
    // ⚠️ Canonical URI **必须含 bucket**，即使请求走 virtual-hosted style
    //    （`https://{bucket}.{endpoint}/{key}`，HTTP 请求行里只有 `/{key}`）。
    //    官方测试向量同样写作 `/examplebucket/exampleobject`。
    //    2026-11-14 首次部署实测踩到：漏了 bucket ⇒ OSS 算出的 CanonicalRequest
    //    与我们的不一致 ⇒ 403 SignatureDoesNotMatch（错误 XML 会把服务端算的
    //    CanonicalRequest 原样回显，是排查这类问题最直接的证据）。
    let canonical_uri = format!(
        "/{}/{}",
        uri_encode(&cfg.bucket, false),
        uri_encode(key, false)
    );

    // 参与签名的头：content-type（存在即签）+ x-oss-prefix 头。
    // ⚠️ 不签 host（OSS 规则），也不签 content-length（由 reqwest 自行设置，
    //    强行纳入签名会因值不一致导致 SignatureDoesNotMatch）。
    let canonical_headers = format!(
        "content-type:{content_type}\n\
         x-oss-content-sha256:UNSIGNED-PAYLOAD\n\
         x-oss-date:{timestamp}\n"
    );
    let creq = canonical_request("PUT", &canonical_uri, "", &canonical_headers, "");
    let sig = signature(cfg, date_stamp, &timestamp, &creq);
    let auth = format!(
        "OSS4-HMAC-SHA256 Credential={}/{date_stamp}/{}/oss/aliyun_v4_request,Signature={sig}",
        cfg.key_id, cfg.region
    );

    let resp = client()
        .put(object_url(cfg, key))
        .header("Content-Type", content_type)
        .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-oss-date", &timestamp)
        .header("Authorization", auth)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("OSS 连接失败: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::error!("OSS PutObject {key} 失败 {status}: {text}");
        Err(format!("对象存储写入失败（{status}）"))
    }
}

/// GetObject。返回 `(content_type, bytes)`。
pub async fn get_object(cfg: &OssConfig, key: &str) -> Result<(String, Vec<u8>), String> {
    let now = chrono::Utc::now();
    let timestamp = iso8601(now);
    let date_stamp = &timestamp[..8];
    // ⚠️ Canonical URI **必须含 bucket**，即使请求走 virtual-hosted style
    //    （`https://{bucket}.{endpoint}/{key}`，HTTP 请求行里只有 `/{key}`）。
    //    官方测试向量同样写作 `/examplebucket/exampleobject`。
    //    2026-11-14 首次部署实测踩到：漏了 bucket ⇒ OSS 算出的 CanonicalRequest
    //    与我们的不一致 ⇒ 403 SignatureDoesNotMatch（错误 XML 会把服务端算的
    //    CanonicalRequest 原样回显，是排查这类问题最直接的证据）。
    let canonical_uri = format!(
        "/{}/{}",
        uri_encode(&cfg.bucket, false),
        uri_encode(key, false)
    );

    // GET 不带 content-type ⇒ 只签两个 x-oss 头。
    let canonical_headers = format!(
        "x-oss-content-sha256:UNSIGNED-PAYLOAD\nx-oss-date:{timestamp}\n"
    );
    let creq = canonical_request("GET", &canonical_uri, "", &canonical_headers, "");
    let sig = signature(cfg, date_stamp, &timestamp, &creq);
    let auth = format!(
        "OSS4-HMAC-SHA256 Credential={}/{date_stamp}/{}/oss/aliyun_v4_request,Signature={sig}",
        cfg.key_id, cfg.region
    );

    let resp = client()
        .get(object_url(cfg, key))
        .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-oss-date", &timestamp)
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| format!("OSS 连接失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("OSS 状态码 {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取失败: {e}"))?
        .to_vec();
    Ok((content_type, bytes))
}

/// 连通性自检：写一个探针对象再读回并删除。
///
/// 管理端「测试连接」按钮走这里。分步返回失败原因——签名错（403
/// SignatureDoesNotMatch）和网络不通（连接超时）是完全不同的修复方向，
/// 混成一句「连接失败」等于没测。
pub async fn self_check(cfg: &OssConfig) -> Result<String, String> {
    let key = format!(".selfcheck/{}", uuid::Uuid::new_v4());
    let payload = b"paddock oss self-check";
    put_object(cfg, &key, "text/plain", payload)
        .await
        .map_err(|e| format!("写入失败：{e}"))?;
    let (_, back) = get_object(cfg, &key)
        .await
        .map_err(|e| format!("写成功但读回失败：{e}"))?;
    if back != payload {
        return Err("读回内容与写入不一致".into());
    }
    // 探测语义自检：已存在的 key 必须 true，随机 key 必须 false。
    // ⚠️ 这两条一起验，单验「存在」会漏掉「什么都返回 true」的实现错误
    //    （2026-11-14 实测踩到：迁移把 231 张全判成已迁移）。
    match object_exists(cfg, &key).await {
        Ok(true) => {}
        Ok(false) => return Err(format!("写入了对象但 HeadObject 说它不存在：{key}")),
        Err(e) => return Err(format!("HeadObject 探测失败：{e}")),
    }
    let ghost = format!(".selfcheck/ghost-{}", uuid::Uuid::new_v4());
    match object_exists(cfg, &ghost).await {
        Ok(false) => {}
        Ok(true) => {
            return Err(format!(
                "HeadObject 误报：不存在的对象 {ghost} 被判定为存在。\
                 请检查 bucket 是否为「私有」以及 RAM 是否授予 oss:HeadObject"
            ))
        }
        Err(e) => return Err(format!("HeadObject 探测失败（不存在的对象）：{e}")),
    }
    // 清理探针对象。失败不影响结论（只是留一个 20 字节的垃圾）。
    let _ = delete_object(cfg, &key).await;
    Ok("连接正常（写入/读取/删除均成功）".into())
}

/// HeadObject：判断对象是否存在。返回 `Ok(true)` 存在 / `Ok(false)` 不存在。
///
/// ⚠️ 迁移**必须**用它判断「是否已迁移」，不能靠 `avatar_key` 的字符串形态猜：
///    key 形态是由代码写的，不是由对象位置决定的——2026-11-14 实测踩到，
///    新版 `upload_avatar` 一上线就开始往 Garage 写「新形态 key」，于是
///    「新形态 key + 对象在 Garage」这个组合出现，靠字符串判断会全部误跳过。
pub async fn object_exists(cfg: &OssConfig, key: &str) -> Result<bool, String> {
    let now = chrono::Utc::now();
    let timestamp = iso8601(now);
    let date_stamp = &timestamp[..8];
    let canonical_uri = format!(
        "/{}/{}",
        uri_encode(&cfg.bucket, false),
        uri_encode(key, false)
    );
    let canonical_headers = format!(
        "x-oss-content-sha256:UNSIGNED-PAYLOAD\nx-oss-date:{timestamp}\n"
    );
    let creq = canonical_request("HEAD", &canonical_uri, "", &canonical_headers, "");
    let sig = signature(cfg, date_stamp, &timestamp, &creq);
    let auth = format!(
        "OSS4-HMAC-SHA256 Credential={}/{date_stamp}/{}/oss/aliyun_v4_request,Signature={sig}",
        cfg.key_id, cfg.region
    );
    let resp = client()
        .head(object_url(cfg, key))
        .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-oss-date", &timestamp)
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| format!("OSS 连接失败: {e}"))?;
    match resp.status() {
        s if s.is_success() => Ok(true),
        reqwest::StatusCode::NOT_FOUND => Ok(false),
        // 403 通常意味着 bucket 权限没给 HeadObject（RAM policy 里漏了），
        // 这种情况必须报错而不是当成「不存在」——否则会重复迁移并覆盖。
        s => Err(format!("HeadObject 返回 {}", s)),
    }
}
/// DeleteObject。仅自检清理与迁移回滚用。
pub async fn delete_object(cfg: &OssConfig, key: &str) -> Result<(), String> {
    let now = chrono::Utc::now();
    let timestamp = iso8601(now);
    let date_stamp = &timestamp[..8];
    // ⚠️ Canonical URI **必须含 bucket**，即使请求走 virtual-hosted style
    //    （`https://{bucket}.{endpoint}/{key}`，HTTP 请求行里只有 `/{key}`）。
    //    官方测试向量同样写作 `/examplebucket/exampleobject`。
    //    2026-11-14 首次部署实测踩到：漏了 bucket ⇒ OSS 算出的 CanonicalRequest
    //    与我们的不一致 ⇒ 403 SignatureDoesNotMatch（错误 XML 会把服务端算的
    //    CanonicalRequest 原样回显，是排查这类问题最直接的证据）。
    let canonical_uri = format!(
        "/{}/{}",
        uri_encode(&cfg.bucket, false),
        uri_encode(key, false)
    );
    let canonical_headers = format!(
        "x-oss-content-sha256:UNSIGNED-PAYLOAD\nx-oss-date:{timestamp}\n"
    );
    let creq = canonical_request("DELETE", &canonical_uri, "", &canonical_headers, "");
    let sig = signature(cfg, date_stamp, &timestamp, &creq);
    let auth = format!(
        "OSS4-HMAC-SHA256 Credential={}/{date_stamp}/{}/oss/aliyun_v4_request,Signature={sig}",
        cfg.key_id, cfg.region
    );
    let resp = client()
        .delete(object_url(cfg, key))
        .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-oss-date", &timestamp)
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| format!("OSS 连接失败: {e}"))?;
    if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!("删除失败（{}）", resp.status()))
    }
}

/// 便捷入口：为 `AppState` 加载配置，未配置返回 `None`。
pub async fn config_of(state: &AppState) -> Option<OssConfig> {
    load_config(&state.pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阿里云官方 V4 签名测试向量（PutObject 示例）。
    ///
    /// 来源：「在Header中包含V4签名（推荐）」文档的「签名计算过程示例」。
    /// ⚠️ 文档里的 `AccessKeySecret` 是占位符 `yourAccessKeySecret`，因此
    /// **SigningKey 无法与文档值对上**；能验证的是：
    /// ① CanonicalRequest 逐字节一致（最容易翻车的地方：换行、排序、编码）
    /// ② 其 SHA256 与官方值一致
    /// ③ 用文档给出的 SigningKey 反推 Signature 与官方值一致（证明待签串构造正确）
    #[test]
    fn official_vector() {
        let canonical_headers = "content-disposition:attachment\n\
             content-length:3\n\
             content-md5:ICy5YqxZB1uWSwcVLSNLcA==\n\
             content-type:text/plain\n\
             x-oss-content-sha256:UNSIGNED-PAYLOAD\n\
             x-oss-date:20250411T064124Z\n";
        let creq = canonical_request(
            "PUT",
            "/examplebucket/exampleobject",
            "",
            canonical_headers,
            "content-disposition;content-length",
        );
        assert_eq!(
            sha256_hex(creq.as_bytes()),
            "c46d96390bdbc2d739ac9363293ae9d710b14e48081fcb22cd8ad54b63136eca",
            "CanonicalRequest 与官方向量不一致"
        );

        // 文档给出的 SigningKey（十六进制）与最终签名。
        let doc_key = hex::decode(
            "3543b7686e65eda71e5e5ca19d548d78423c37e8ddba4dc9d83f90228b457c76",
        )
        .unwrap();
        let sts = format!(
            "OSS4-HMAC-SHA256\n20250411T064124Z\n20250411/cn-hangzhou/oss/aliyun_v4_request\n{}",
            sha256_hex(creq.as_bytes())
        );
        assert_eq!(
            hex::encode(hmac(&doc_key, sts.as_bytes())),
            "053edbf550ebd239b32a9cdfd93b0b2b3f2d223083aa61f75e9ac16856d61f23",
            "Signature 与官方向量不一致"
        );
    }

    /// 密钥派生链的形状自检：每一步都必须真的改变结果（防止误写成
    /// 「四步都拿同一个 key」这类静默错误）。
    #[test]
    fn signing_key_derivation_is_chained() {
        let a = signing_key("secret", "20250411", "region-a");
        let b = signing_key("secret", "20250412", "region-a");
        let c = signing_key("secret", "20250411", "region-b");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "日期不参与派生");
        assert_ne!(a, c, "地域不参与派生");
    }

    #[test]
    fn uri_encoding_rules() {
        // 路径里 `/` 保留，空格编 %20（不是 +）
        assert_eq!(uri_encode("avatars/a b/c.jpg", false), "avatars/a%20b/c.jpg");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        // 未保留字符集外的字符必须编码
        assert_eq!(uri_encode("a+b", false), "a%2Bb");
        assert_eq!(uri_encode("~-._", false), "~-._");
    }

    #[test]
    fn public_url_has_no_query_string() {
        let cfg = OssConfig {
            endpoint: "oss-cn-example.aliyuncs.com".into(),
            bucket: "b".into(),
            region: "cn-example".into(),
            key_id: "k".into(),
            secret: "s".into(),
            public_base: "https://b.oss-cn-example.aliyuncs.com".into(),
        };
        let url = public_url(&cfg, "avatars/uuid/1712345678000.jpg");
        assert_eq!(
            url,
            "https://b.oss-cn-example.aliyuncs.com/avatars/uuid/1712345678000.jpg"
        );
        assert!(!url.contains('?'), "公网 URL 不得带查询串");
    }

    /// 请求结构自检：验证发出请求的关键字段。
    ///
    /// ⚠️ 刻意**不做**真实网络请求：virtual-hosted 域名是
    /// `{bucket}.{endpoint}`，本地监听器拿不到 `bucket.127.0.0.1` 这种域名，
    /// 硬发只会走到 DNS 失败分支、抓包永远为空——那种"测试通过"是假的。
    /// 真实验证交给部署后的管理端「测试连接」（写→读→删三步）。
    #[test]
    fn request_shape_matches_oss_spec() {
        let cfg = OssConfig {
            endpoint: "oss-cn-example.aliyuncs.com".into(),
            bucket: "testbucket".into(),
            region: "cn-example".into(),
            key_id: "AKID".into(),
            secret: "SECRET".into(),
            public_base: "https://testbucket.example.com".into(),
        };
        // virtual-hosted：bucket 在域名里，路径只有 key（不含 bucket）
        assert_eq!(
            object_url(&cfg, "avatars/x/1.jpg"),
            "https://testbucket.oss-cn-example.aliyuncs.com/avatars/x/1.jpg"
        );
        // x-oss-date 必须是 16 字符 ISO8601 基本格式
        let date = iso8601(chrono::Utc::now());
        assert_eq!(date.len(), 16, "x-oss-date 长度错误: {date}");
        assert!(date.ends_with('Z'));
        assert_eq!(&date[8..9], "T");
        // Scope 用前 8 位
        assert_eq!(&date[..8].len(), &8);
    }

    /// 生产事故回归（2026-11-14）：Canonical URI 必须含 bucket。
    ///
    /// 首次部署时漏了 bucket 前缀，OSS 返回 403 SignatureDoesNotMatch，错误 XML
    /// 里 `<CanonicalRequest>` 字段原样回显了**服务端**算的请求串：
    ///   `PUT\n/ala-mobile-paddock/.selfcheck/{uuid}\n\ncontent-type:...`
    /// 注意请求行走的是 virtual-hosted（`ala-mobile-paddock.oss-cn-hangzhou...`），
    /// HTTP 路径只有 `/.selfcheck/{uuid}`——但 OSS 计算签名时**仍把 bucket 拼回
    /// canonical URI**。这个测试用真实的事故数据锁死该行为。
    #[test]
    fn canonical_uri_includes_bucket() {
        let cfg = OssConfig {
            endpoint: "oss-cn-hangzhou.aliyuncs.com".into(),
            bucket: "ala-mobile-paddock".into(),
            region: "cn-hangzhou".into(),
            key_id: "k".into(),
            secret: "s".into(),
            public_base: "https://ala-mobile-paddock.oss-cn-hangzhou.aliyuncs.com".into(),
        };
        let creq = canonical_request(
            "PUT",
            &format!("/{}/{}", cfg.bucket, ".selfcheck/e9a33ed1-a90c-48d4-a6aa-2c845af71762"),
            "",
            "content-type:text/plain\n\
             x-oss-content-sha256:UNSIGNED-PAYLOAD\n\
             x-oss-date:20261114T104659Z\n",
            "",
        );
        // 这是 OSS 服务端实际计算并回显的 SHA256（事故原始值）。
        assert_eq!(
            sha256_hex(creq.as_bytes()),
            "df03d0007f6da0bbbf74d472f2aada4434bc1afad205f4e4d9d54838f20762b4",
            "CanonicalRequest 与 OSS 服务端不一致"
        );
    }

}
