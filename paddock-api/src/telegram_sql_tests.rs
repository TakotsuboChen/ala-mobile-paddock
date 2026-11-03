//! Telegram 建号 + 身份反查的端到端 SQL 测试（`#[ignore]`，需要真实 Postgres）。
//!
//! **为什么必须有这一层**：`0012` 迁移给 `users` 加了 `telegram_id BIGINT UNIQUE`，
//! `create_user_from_pending` 改成按 `Platform` 分支写不同列、按不同列做重复检查。
//! 这些 SQL 是**运行时 `query`**（非宏），占位符序号、列名、BIGINT ↔ i64 的类型匹配
//! 在编译期全都发现不了——只有真跑一遍才知道。
//!
//! 覆盖：
//! 1. 迁移能跑通（`telegram_id` 列存在，且是 BIGINT）
//! 2. Telegram 建号 → 只写 `telegram_id`，`member_openid` 保持 NULL
//! 3. QQ 建号 → 只写 `member_openid`，`telegram_id` 保持 NULL
//! 4. 同一 Telegram id 重复建号 → 409（唯一约束路径）
//! 5. 超出 32 位的 Telegram id（52 位有效位）能正常存取
//! 6. `lookup_identity` 两平台各自命中，不串列
//!
//! 运行方式（⚠️ 必须串行，且只能指向临时库）：
//! ```bash
//! docker run -d --rm --name pgtest -e POSTGRES_PASSWORD=test \
//!     -e POSTGRES_DB=paddock -p 55432:5432 postgres:16-alpine
//! DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/paddock \
//!     cargo test --offline -- --ignored --test-threads=1
//! ```
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::bot_core::{lookup_identity, Platform};

async fn setup() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    sqlx::migrate!("./migrations").run(&pool).await.ok()?;
    Some(pool)
}

/// 造一条 pending_regs，返回 reg_code。
async fn mk_pending(pool: &PgPool, name: &str) -> String {
    let code = format!("TG{}", &uuid::Uuid::new_v4().simple().to_string()[..8].to_uppercase());
    sqlx::query(
        "INSERT INTO pending_regs (reg_code, username, pass_hash, expires_at) \
         VALUES ($1, $2, 'x', now() + interval '30 minutes')",
    )
    .bind(&code)
    .bind(name)
    .execute(pool)
    .await
    .expect("插入 pending_regs 失败");
    code
}

#[tokio::test]
#[ignore]
async fn migration_adds_telegram_id_as_bigint() {
    let Some(pool) = setup().await else { return };
    let ty: Option<String> = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name='users' AND column_name='telegram_id'",
    )
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten();
    assert_eq!(ty.as_deref(), Some("bigint"), "telegram_id 必须是 BIGINT（user id 超 32 位）");
}

#[tokio::test]
#[ignore]
async fn telegram_registration_writes_only_telegram_id() {
    let Some(pool) = setup().await else { return };
    let name = format!("tg{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let code = mk_pending(&pool, &name).await;
    let tg_id = 1_234_567_890_i64;

    let uid = crate::api::auth_handlers::create_user_from_pending(
        &pool,
        &code,
        Platform::Telegram,
        &tg_id.to_string(),
    )
    .await
    .expect("Telegram 建号应成功");

    let (openid, tg): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT member_openid, telegram_id FROM users WHERE id=$1")
            .bind(uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(openid.is_none(), "Telegram 建号不得写 member_openid");
    assert_eq!(tg, Some(tg_id));

    sqlx::query("DELETE FROM users WHERE id=$1").bind(uid).execute(&pool).await.ok();
}

#[tokio::test]
#[ignore]
async fn qq_registration_writes_only_member_openid() {
    let Some(pool) = setup().await else { return };
    let name = format!("qq{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let code = mk_pending(&pool, &name).await;
    let openid = format!("OPENID{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);

    let uid = crate::api::auth_handlers::create_user_from_pending(
        &pool,
        &code,
        Platform::Qq,
        &openid,
    )
    .await
    .expect("QQ 建号应成功");

    let (oid, tg): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT member_openid, telegram_id FROM users WHERE id=$1")
            .bind(uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(oid.as_deref(), Some(openid.as_str()));
    assert!(tg.is_none(), "QQ 建号不得写 telegram_id");

    sqlx::query("DELETE FROM users WHERE id=$1").bind(uid).execute(&pool).await.ok();
}

#[tokio::test]
#[ignore]
async fn duplicate_telegram_id_rejected() {
    let Some(pool) = setup().await else { return };
    let tg_id = 9_876_543_210_i64;

    let n1 = format!("tga{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let c1 = mk_pending(&pool, &n1).await;
    let uid = crate::api::auth_handlers::create_user_from_pending(
        &pool, &c1, Platform::Telegram, &tg_id.to_string(),
    )
    .await
    .expect("首次建号应成功");

    // 同一 Telegram id 再建一次 → 必须 409
    let n2 = format!("tgb{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let c2 = mk_pending(&pool, &n2).await;
    let err = crate::api::auth_handlers::create_user_from_pending(
        &pool, &c2, Platform::Telegram, &tg_id.to_string(),
    )
    .await
    .expect_err("重复 Telegram 身份必须被拒");
    assert_eq!(err.0, axum::http::StatusCode::CONFLICT);

    sqlx::query("DELETE FROM users WHERE id=$1").bind(uid).execute(&pool).await.ok();
}

#[tokio::test]
#[ignore]
async fn large_telegram_id_roundtrip() {
    let Some(pool) = setup().await else { return };
    // 52 位有效位：超过 i32 上限，验证没有意外截断
    let tg_id = 4_503_599_627_370_495_i64;
    let name = format!("tgl{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let code = mk_pending(&pool, &name).await;

    let uid = crate::api::auth_handlers::create_user_from_pending(
        &pool, &code, Platform::Telegram, &tg_id.to_string(),
    )
    .await
    .expect("52 位 id 建号应成功");

    let got: Option<i64> = sqlx::query_scalar("SELECT telegram_id FROM users WHERE id=$1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(got, Some(tg_id), "52 位 id 必须原样存取，不得截断");

    sqlx::query("DELETE FROM users WHERE id=$1").bind(uid).execute(&pool).await.ok();
}

#[tokio::test]
#[ignore]
async fn lookup_identity_does_not_cross_platforms() {
    let Some(pool) = setup().await else { return };
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let qq_name = format!("lqq{suffix}");
    let tg_name = format!("ltg{suffix}");
    let openid = format!("OP{suffix}");
    let tg_id = 5_555_666_777_i64;

    let c1 = mk_pending(&pool, &qq_name).await;
    let uid_qq = crate::api::auth_handlers::create_user_from_pending(
        &pool, &c1, Platform::Qq, &openid,
    ).await.unwrap();
    let c2 = mk_pending(&pool, &tg_name).await;
    let uid_tg = crate::api::auth_handlers::create_user_from_pending(
        &pool, &c2, Platform::Telegram, &tg_id.to_string(),
    ).await.unwrap();

    // 各自命中自己的行
    let qq_hit = lookup_identity(&pool, Platform::Qq, &openid).await;
    assert_eq!(qq_hit.as_ref().map(|x| x.0.as_str()), Some(qq_name.as_str()));
    let tg_hit = lookup_identity(&pool, Platform::Telegram, &tg_id.to_string()).await;
    assert_eq!(tg_hit.as_ref().map(|x| x.0.as_str()), Some(tg_name.as_str()));

    // 拿 Telegram id 当 QQ openid 查 → 不得命中（列不同，不可能串）
    assert!(lookup_identity(&pool, Platform::Qq, &tg_id.to_string()).await.is_none());
    // 拿 QQ openid 当 Telegram id 查 → 解析失败 → None
    assert!(lookup_identity(&pool, Platform::Telegram, &openid).await.is_none());

    sqlx::query("DELETE FROM users WHERE id IN ($1,$2)")
        .bind(uid_qq)
        .bind(uid_tg)
        .execute(&pool)
        .await
        .ok();
}
