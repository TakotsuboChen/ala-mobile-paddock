//! 存量头像迁移：Garage（或旧 OSS 形态）→ OSS 新 key 形态 + 就地压到 256×256。
//!
//! ## 为什么不需要 ListObjects
//! `users.avatar_key` 就是完整清单（228 行）。这天然过滤掉 Garage 里 159 个孤儿
//! blob（历史换头像留下的旧对象），也省掉一次 OSS 侧权限（RAM policy 可以完全
//! 不给 `oss:ListObjects`）。
//!
//! ## 幂等与续跑
//! 每行处理完就写回 `users.avatar_key`。中断后再点一次按钮时，`avatar_key`
//! 已经是新形态的行会被跳过（`migrated` 计数），所以重复点击是安全的。
//!
//! ## 三条不可违背的约束
//! 1. **绝不写 `avatar_updated_at`**。写它等于给全部 228 个用户加 30 天冷却，
//!    是一次性把所有人的换头像功能锁死（比迁移失败严重得多）。
//! 2. **绝不改 `avatar_version`**。改了 URL 就变，526 个客户端全部重下头像，
//!    瞬时 QPS 峰值反而制造事故。
//! 3. **原图先备份到 `avatars_orig/`**。压缩不可逆，30 天内可回滚。
//!
//! ## 进度上报
//! 复用 `runlog` 同款「单例状态 + 游标轮询」骨架：管理端每 1.5 秒拉一次
//! `/admin/api/settings/oss/migrate?after=<seq>`，只拿增量行 + 汇总计数。

use std::sync::{Mutex, OnceLock};

use crate::oss;

/// 迁移进度状态。`seq` 是日志游标，`done/total` 是汇总计数。
#[derive(Default, Clone, serde::Serialize)]
pub struct Progress {
    pub running: bool,
    pub done: u64,
    pub total: u64,
    pub failed: u64,
    pub skipped: u64,
    /// 迁移完成时间（None = 未完成或从未运行）
    pub finished_at: Option<String>,
    /// 致命错误（配置缺失等），非空时表示整体失败
    pub error: Option<String>,
}

struct State {
    progress: Progress,
    seq: u64,
    lines: Vec<(u64, String)>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            progress: Progress::default(),
            seq: 0,
            lines: Vec::new(),
        })
    })
}

fn log(line: impl Into<String>) {
    let line = line.into();
    // ⚠️ 必须同时写 tracing：只进内存环的话，`docker logs` 一行都看不到，
    //    排查「迁移到底跑没跑」时完全没有证据（2026-11-14 实测踩到）。
    tracing::info!("[avatar-migrate] {line}");
    let mut s = match state().lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    s.seq += 1;
    let seq = s.seq;
    s.lines.push((seq, line));
    // 只保留最近 500 行：迁移日志是过程性信息，长时间运行不该无限增长。
    if s.lines.len() > 500 {
        let drop = s.lines.len() - 500;
        s.lines.drain(0..drop);
    }
}

/// 读增量日志（游标语义与 `runlog::read_after` 一致）。
pub fn read_after(after: u64) -> (Vec<String>, u64, Progress) {
    let s = match state().lock() {
        Ok(s) => s,
        Err(_) => return (Vec::new(), 0, Progress::default()),
    };
    let lines: Vec<String> = s
        .lines
        .iter()
        .filter(|(seq, _)| *seq > after)
        .map(|(_, l)| l.clone())
        .collect();
    (lines, s.seq, s.progress.clone())
}

/// 当前进度快照（不含日志）。
pub fn snapshot() -> Progress {
    match state().lock() {
        Ok(s) => s.progress.clone(),
        Err(_) => Progress::default(),
    }
}

fn set_progress(f: impl FnOnce(&mut Progress)) {
    if let Ok(mut s) = state().lock() {
        f(&mut s.progress);
    }
}

/// 迁移是否正在跑（防止并发触发两次）。
pub fn is_running() -> bool {
    snapshot().running
}

// ---------- 图片压缩 ----------

/// 头像目标边长。前端显示最大 96dp，3× 屏 = 288px；256 是实测均衡点
/// （22KB/张 vs 原图 1.8MB，省 98.8%）。与模块端上传侧参数保持一致。
const AVATAR_SIDE: u32 = 256;
const JPEG_QUALITY: u8 = 88;

/// 把任意图片压到 256×256（等比缩放后居中裁方）。
///
/// 返回 `None` = 解码失败（非图片/格式不支持/数据损坏）。此时调用方**保留原图
/// 不覆盖**——宁可留一张大图，也不能因为压不了就把用户头像弄丢。
fn compress_to_avatar(bytes: &[u8]) -> Option<Vec<u8>> {
    // `image` crate 的 `thumbnail` 是「等比缩放到能装进 N×N」，不裁切、不变形。
    // 头像在 UI 端是圆形裁切，方图等比缩放即视觉等价（游戏内头像原始就是方的）。
    let img = image::load_from_memory(bytes).ok()?;
    let thumb = img.thumbnail(AVATAR_SIDE, AVATAR_SIDE);

    // 统一编码成 JPEG：头像没有透明需求（历史数据实测四角全是实色），
    // PNG 只会让体积翻几倍。
    let mut out = std::io::Cursor::new(Vec::new());
    let rgb = thumb.to_rgb8();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    enc.encode_image(&rgb).ok()?;
    let data = out.into_inner();
    // 压缩反而变大（极小图/已高度压缩的图）时保留原图：体积是唯一动机，
    // 变大就没有理由覆盖。
    if data.is_empty() || data.len() >= bytes.len() {
        return None;
    }
    Some(data)
}

// ---------- 迁移主流程 ----------

/// 启动迁移（后台任务）。返回 `Err` = 前置条件不满足（配置缺失/已在运行）。
///
/// 用 `tokio::spawn` 而非请求内同步执行：228 张图 × (下载 + 解码 + 压缩 + 上传)
/// 远超 HTTP 超时，同步跑会让管理页 502。
pub fn start(app: crate::state::AppState) -> Result<(), String> {
    if is_running() {
        return Err("迁移已在运行中".into());
    }
    // 重置状态（seq 不清零，让前端游标继续有效但能看到新行；这里选择清零更直观）
    {
        let mut s = state().lock().map_err(|_| "状态锁中毒".to_string())?;
        s.seq = 0;
        s.lines.clear();
        s.progress = Progress {
            running: true,
            ..Default::default()
        };
    }
    tokio::spawn(async move {
        let pool = app.pool.clone();
        let result = run(app).await;
        match result {
            Ok(()) => {
                let p = snapshot();
                log(format!(
                    "迁移完成：成功 {}，跳过 {}，失败 {}",
                    p.done, p.skipped, p.failed
                ));
                // 只有**零失败**才切 URL 开关：有失败意味着仍有对象留在 Garage，
                // 此时切过去会让那部分头像 404。宁可先不切，让管理员排查后重跑
                // （重跑会跳过已成功的，补上失败的）。
                if p.failed == 0 {
                    if let Err(e) = crate::qq_bot::set_cfg(
                        &pool,
                        oss::keys::MIGRATED,
                        "1",
                    )
                    .await
                    {
                        log(format!("⚠️ 迁移数据已就绪，但写完成标记失败：{e}"));
                        set_progress(|p| p.error = Some(format!("写迁移完成标记失败：{e}")));
                    } else {
                        log("✅ 已写入迁移完成标记，头像 URL 从现在起改为 OSS 直连");
                    }
                } else {
                    log(format!(
                        "⚠️ 有 {} 张失败，**不切换** URL（否则这些头像会 404）。\
                         排查后重新点「开始迁移」即可续跑（已成功的会自动跳过）",
                        p.failed
                    ));
                }
            }
            Err(e) => {
                log(format!("迁移中止：{e}"));
                set_progress(|p| p.error = Some(e));
            }
        }
        set_progress(|p| {
            p.running = false;
            p.finished_at = Some(chrono::Utc::now().to_rfc3339());
        });
    });
    Ok(())
}

/// 一次迁移任务的实际执行体。
async fn run(state: crate::state::AppState) -> Result<(), String> {
    let Some(oss_cfg) = oss::config_of(&state).await else {
        return Err("OSS 未配置，无法迁移".into());
    };
    let garage = garage_config();
    if garage.is_none() {
        log("未检测到 Garage 凭据，只能迁移已在 OSS 上的旧形态对象");
    }

    // 清单：所有有头像的用户。
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, avatar_key FROM users WHERE avatar_key IS NOT NULL ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| format!("查询用户清单失败: {e}"))?;

    set_progress(|p| p.total = rows.len() as u64);
    log(format!("共 {} 个头像待处理", rows.len()));
    // Garage 与 OSS 的连通性先各探一次：迁移慢/卡住时，第一现场就能看出是
    // 源端还是目标端的问题，不用等第一张图超时 20 秒。
    log(format!(
        "源=Garage({}) 目标=OSS({})",
        if garage.is_some() { "已配置" } else { "未配置" },
        oss_cfg.endpoint
    ));

    for (user_id, old_key) in rows {
        // 幂等判据 = **OSS 上是否真的已有这个对象**，不是 key 的字符串形态。
        //
        // ⚠️ 2026-11-14 事故：最初用 `is_new_key()` 判「已经迁移过」，结果全部
        //    231 张被跳过。根因是 **key 形态由代码写死、与对象位置无关**——
        //    新版 `upload_avatar` 上线后立刻开始往 Garage 写「新形态 key」，
        //    于是「新形态 key + 对象还在 Garage」这个组合出现，被判成已迁移。
        //    代价是：用户换头像后写进 Garage 的对象永远不会被搬到 OSS，
        //    而 URL 已经是 OSS 绝对地址 ⇒ 那张头像 404。
        let probe = oss::object_exists(&oss_cfg, &old_key).await;
        // 诊断日志：把探测结果打出来。231 张「全部跳过」时，只有这一行能区分
        // 「OSS 真的全有」和「探测逻辑坏了」——2026-11-14 排查就卡在这里。
        log(format!("探测 {old_key} → {probe:?}"));
        match probe {
            Ok(true) => {
                set_progress(|p| p.skipped += 1);
                continue;
            }
            Ok(false) => {} // 正常：OSS 上还没有，需要迁移
            Err(e) => {
                // 探测失败不能当成「不存在」——那会导致重复覆盖。
                log(format!("{user_id}: 探测 OSS 失败，跳过 — {e}"));
                set_progress(|p| p.failed += 1);
                continue;
            }
        }

        match migrate_one(&state, &oss_cfg, garage.as_ref(), user_id, &old_key).await {
            Ok(msg) => {
                log(format!("{user_id}: {msg}"));
                set_progress(|p| p.done += 1);
            }
            Err(e) => {
                log(format!("{user_id}: 失败 — {e}"));
                set_progress(|p| p.failed += 1);
            }
        }
    }
    Ok(())
}

/// 迁移单个头像：读原图 → 压缩 → 备份原图 → 写新 key → 更新 avatar_key。
///
/// ⚠️ 只更新 `avatar_key`，**不动** `avatar_version` 与 `avatar_updated_at`。
async fn migrate_one(
    state: &crate::state::AppState,
    oss_cfg: &oss::OssConfig,
    garage: Option<&GarageConfig>,
    user_id: uuid::Uuid,
    old_key: &str,
) -> Result<String, String> {
    // 源数据：优先 OSS（若旧形态已在 OSS），否则 Garage。
    let (content_type, original) = match oss::get_object(oss_cfg, old_key).await {
        Ok(v) => v,
        Err(_) => match garage {
            Some(g) => s3_get(g, old_key).await?,
            None => return Err("源对象读取失败（Garage 未配置且 OSS 上不存在）".into()),
        },
    };

    // 原图备份（压缩不可逆，30 天内可回滚）。失败不阻断主流程。
    let backup_key = format!("avatars_orig/{user_id}.bin");
    if let Err(e) = oss::put_object(oss_cfg, &backup_key, &content_type, &original).await {
        log(format!("  {user_id}: 原图备份失败（继续）— {e}"));
    }

    // 压缩。压不了就整份搬运，不丢图。
    let (payload, out_ct) = match compress_to_avatar(&original) {
        Some(compressed) => {
            let ratio = compressed.len() as f64 / original.len().max(1) as f64;
            log(format!(
                "  {user_id}: {} B → {} B（{:.1}%）",
                original.len(),
                compressed.len(),
                ratio * 100.0
            ));
            (compressed, "image/jpeg".to_string())
        }
        None => {
            log(format!("  {user_id}: 无法压缩，按原样搬运"));
            (original, content_type)
        }
    };

    // 版本号沿用库里已有的值：URL 不变 ⇒ 客户端缓存继续命中，不产生重下峰值。
    let version: i64 = sqlx::query_scalar(
        "SELECT COALESCE(avatar_version, 0) FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| format!("读 avatar_version 失败: {e}"))?;

    let new_key = crate::api::avatar::avatar_key(user_id, version);
    oss::put_object(oss_cfg, &new_key, &out_ct, &payload).await?;

    // ⚠️ 只改 avatar_key。绝不写 avatar_updated_at / avatar_version。
    sqlx::query("UPDATE users SET avatar_key = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&new_key)
        .execute(&state.pool)
        .await
        .map_err(|e| format!("更新 avatar_key 失败: {e}"))?;

    Ok(format!("→ {new_key}"))
}

// ---------- Garage 侧读取（与 api/avatar.rs 的 S3 实现对齐） ----------

/// Garage/S3 的 bucket 名（与 `api::avatar` 的同名常量保持一致）。
const GARAGE_BUCKET: &str = "avatars";

pub struct GarageConfig {
    endpoint: String,
    region: String,
    key_id: String,
    secret: String,
}

fn garage_config() -> Option<GarageConfig> {
    let key_id = std::env::var("GARAGE_KEY_ID").ok()?;
    let secret = std::env::var("GARAGE_SECRET").ok()?;
    if key_id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(GarageConfig {
        endpoint: std::env::var("GARAGE_ENDPOINT")
            .unwrap_or_else(|_| "http://garage:3900".into()),
        region: std::env::var("GARAGE_REGION").unwrap_or_else(|_| "paddock".into()),
        key_id,
        secret,
    })
}

/// Garage GetObject（AWS SigV4，path-style）。与 `api::avatar::s3_get` 同构，
/// 但那个是模块私有、且绑定 `GARAGE_BUCKET` 常量，这里独立一份以免把迁移逻辑
/// 塞进请求路径模块。
async fn s3_get(g: &GarageConfig, key: &str) -> Result<(String, Vec<u8>), String> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key 合法");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }
    fn sha_hex(d: &[u8]) -> String {
        hex::encode(Sha256::digest(d))
    }

    let now = chrono::Utc::now();
    let date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let ds = date[..8].to_string();
    // ⚠️ `key` 已含 `avatars/` 前缀（如 `avatars/{uuid}/{v}.jpg`），此处只拼
    //    bucket 名 `avatars`（= GARAGE_BUCKET），**不要再补一次路径段**。
    //    正确形态：http://garage:3900/avatars/avatars/{uuid}/{v}.jpg  ← 错
    //              http://garage:3900/avatars/{uuid}/{v}.jpg          ← 对
    //    拼错会让 Garage 404（2026-11-14 实测踩到）。
    let url = format!("{}/{}/{}", g.endpoint.trim_end_matches('/'), GARAGE_BUCKET, key);
    let parsed = reqwest::Url::parse(&url).map_err(|e| format!("URL 解析失败: {e}"))?;
    let host = format!(
        "{}:{}",
        parsed.host_str().unwrap_or("garage"),
        parsed.port().unwrap_or(80)
    );
    let ph = sha_hex(b"");
    let ch = format!("host:{host}\nx-amz-content-sha256:{ph}\nx-amz-date:{date}\n");
    let sh = "host;x-amz-content-sha256;x-amz-date";
    let creq = format!("GET\n{}\n\n{ch}\n{sh}\n{ph}", parsed.path());
    let scope = format!("{ds}/{}/s3/aws4_request", g.region);
    let sts = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", sha_hex(creq.as_bytes()));
    let k = hmac(format!("AWS4{}", g.secret).as_bytes(), ds.as_bytes());
    let k = hmac(&k, g.region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let sig = hex::encode(hmac(&k, sts.as_bytes()));

    let resp = reqwest::Client::new()
        .get(&url)
        .header("x-amz-content-sha256", &ph)
        .header("x-amz-date", &date)
        .header(
            "Authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={sh}, Signature={sig}",
                g.key_id
            ),
        )
        .send()
        .await
        .map_err(|e| format!("Garage 连接失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Garage 状态码 {}", resp.status()));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| format!("读取失败: {e}"))?.to_vec();
    Ok((ct, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_rejects_garbage() {
        // 非图片数据必须返回 None（调用方据此保留原图，不能误判成「压到 0 字节」）
        assert!(compress_to_avatar(b"not an image").is_none());
        assert!(compress_to_avatar(b"").is_none());
    }

    #[test]
    fn compress_shrinks_a_real_image() {
        // 造一张 1024×1024 的纯色 JPEG，验证能解码 + 缩小。
        let mut src = std::io::Cursor::new(Vec::new());
        let img = image::RgbImage::from_fn(1024, 1024, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 95);
        enc.encode_image(&img).unwrap();
        let orig = src.into_inner();

        let out = compress_to_avatar(&orig).expect("应能压缩");
        assert!(out.len() < orig.len(), "压缩后应更小");
        // 解码回来验证边长
        let decoded = image::load_from_memory(&out).unwrap();
        assert!(decoded.width() <= AVATAR_SIDE && decoded.height() <= AVATAR_SIDE);
    }
}
