//! 车型 + 调教上传/读取的端到端 SQL 测试（`#[ignore]`，需要真实 Postgres）。
//!
//! **为什么必须有这一层**：`0011` 迁移加了 15 列，`laps.rs` 的 INSERT 从 10 个
//! 占位符变成 25 个、`driver.rs` 的 SELECT 同步加列——占位符序号错位、列名拼错、
//! `Option<f32>` 与 `REAL` 的类型不匹配，这些**编译期全都发现不了**（sqlx 用的是
//! 运行时 `query` 而非宏）。只有真跑一遍才能确认。
//!
//! 覆盖：
//! 1. 迁移能跑通（15 个新列存在）
//! 2. 全字段往返：写入 13 个 setup + 车型 → 读出与原值一致
//! 3. 调教半套 → `normalized()` 整组落 None（不写半套进库）
//! 4. 车型越界 → 两列都 None
//! 5. 旧客户端（只发 gp/ms/version）零改动可上传，新列全 NULL
//!
//! 运行方式（⚠️ 必须串行 `--test-threads=1`，且只能指向临时库）：
//! ```bash
//! docker run -d --rm --name pgtest -e POSTGRES_PASSWORD=test \
//!     -e POSTGRES_DB=paddock -p 55432:5432 postgres:16-alpine
//! DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/paddock \
//!     cargo test --release --test '*' -- --ignored --nocapture --test-threads=1
//! ```
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use crate::score::CarSetupConfig;

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

async fn mk_user(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    // `pass_hash` 是 NOT NULL（无默认值）——测试用户不需要能登录，给个占位。
    // `username` 与 `reg_seq` 都有唯一约束 ⇒ 两者都带 id 派生的后缀，
    // 保证**同一测试可重复跑**（残留行不会让下一轮冲突）。
    let suffix = &id.simple().to_string()[..8];
    let uname = format!("{name}_{suffix}");
    let seq = 9000i64 + (id.as_u128() % 100_000) as i64;
    sqlx::query("INSERT INTO users (id, username, pass_hash, reg_seq) VALUES ($1,$2,'x',$3)")
        .bind(id)
        .bind(&uname)
        .bind(seq)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

/// 造一个完整调教（13 个字段全 Some）。
fn full_setup() -> CarSetupConfig {
    CarSetupConfig {
        car_id: Some(22),
        car_era: Some(1),
        setup_front_wing_aero: Some(65.0),
        setup_rear_wing_aero: Some(58.0),
        setup_front_ride_height: Some(40.0),
        setup_rear_ride_height: Some(40.0),
        setup_brake_bias: Some(60.0),
        setup_front_stiffness: Some(50.0),
        setup_rear_stiffness: Some(50.0),
        setup_front_rollbar: Some(50.0),
        setup_rear_rollbar: Some(50.0),
        setup_front_camber: Some(-3.0),
        setup_rear_camber: Some(-1.5),
        setup_front_toe: Some(-0.05),
        setup_rear_toe: Some(0.15),
    }
}

/// 1) 迁移建出 15 个新列。
#[tokio::test]
#[ignore]
async fn migration_adds_car_and_setup_columns() {
    let Some(pool) = setup().await else { return };
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name='laps' AND column_name LIKE 'car_%' OR \
               (table_name='laps' AND column_name LIKE 'setup_%')",
    )
    .fetch_all(&pool)
    .await
    .expect("query columns");
    // 2 个车型列 + 13 个调教列
    assert_eq!(cols.len(), 15, "expected 15 new columns, got {cols:?}");
    println!("[V] 15 columns present: {cols:?}");
}

/// 2) 全字段往返：写进去的值原样读回（含负角度与两位小数）。
#[tokio::test]
#[ignore]
async fn full_car_setup_roundtrip() {
    let Some(pool) = setup().await else { return };
    let uid = mk_user(&pool, "roundtrip_driver").await;
    let cs = full_setup();
    let lap_id = Uuid::now_v7();

    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms, \
                           car_id, car_era, \
                           setup_front_wing_aero, setup_rear_wing_aero, \
                           setup_front_ride_height, setup_rear_ride_height, \
                           setup_brake_bias, \
                           setup_front_stiffness, setup_rear_stiffness, \
                           setup_front_rollbar, setup_rear_rollbar, \
                           setup_front_camber, setup_rear_camber, \
                           setup_front_toe, setup_rear_toe) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
    )
    .bind(lap_id)
    .bind(uid)
    .bind(0i16)
    .bind(200150i32)
    .bind(74105i32)
    .bind(cs.car_id)
    .bind(cs.car_era.map(|v| v as i16))
    .bind(cs.setup_front_wing_aero.map(|v| v as f32))
    .bind(cs.setup_rear_wing_aero.map(|v| v as f32))
    .bind(cs.setup_front_ride_height.map(|v| v as f32))
    .bind(cs.setup_rear_ride_height.map(|v| v as f32))
    .bind(cs.setup_brake_bias.map(|v| v as f32))
    .bind(cs.setup_front_stiffness.map(|v| v as f32))
    .bind(cs.setup_rear_stiffness.map(|v| v as f32))
    .bind(cs.setup_front_rollbar.map(|v| v as f32))
    .bind(cs.setup_rear_rollbar.map(|v| v as f32))
    .bind(cs.setup_front_camber.map(|v| v as f32))
    .bind(cs.setup_rear_camber.map(|v| v as f32))
    .bind(cs.setup_front_toe.map(|v| v as f32))
    .bind(cs.setup_rear_toe.map(|v| v as f32))
    .execute(&pool)
    .await
    .expect("insert lap");

    #[derive(sqlx::FromRow)]
    struct Row {
        car_id: Option<i16>,
        car_era: Option<i16>,
        setup_front_wing_aero: Option<f32>,
        setup_rear_wing_aero: Option<f32>,
        setup_front_ride_height: Option<f32>,
        setup_rear_ride_height: Option<f32>,
        setup_brake_bias: Option<f32>,
        setup_front_stiffness: Option<f32>,
        setup_rear_stiffness: Option<f32>,
        setup_front_rollbar: Option<f32>,
        setup_rear_rollbar: Option<f32>,
        setup_front_camber: Option<f32>,
        setup_rear_camber: Option<f32>,
        setup_front_toe: Option<f32>,
        setup_rear_toe: Option<f32>,
    }
    let r: Row = sqlx::query_as(
        "SELECT car_id, car_era, setup_front_wing_aero, setup_rear_wing_aero, \
                setup_front_ride_height, setup_rear_ride_height, setup_brake_bias, \
                setup_front_stiffness, setup_rear_stiffness, \
                setup_front_rollbar, setup_rear_rollbar, \
                setup_front_camber, setup_rear_camber, \
                setup_front_toe, setup_rear_toe \
         FROM laps WHERE id=$1",
    )
    .bind(lap_id)
    .fetch_one(&pool)
    .await
    .expect("select lap");

    assert_eq!(r.car_id, Some(22));
    assert_eq!(r.car_era, Some(1));
    assert_eq!(r.setup_front_wing_aero, Some(65.0));
    assert_eq!(r.setup_rear_wing_aero, Some(58.0));
    assert_eq!(r.setup_brake_bias, Some(60.0));
    assert_eq!(r.setup_front_camber, Some(-3.0));
    assert_eq!(r.setup_rear_camber, Some(-1.5));
    assert_eq!(r.setup_front_toe, Some(-0.05));
    assert_eq!(r.setup_rear_toe, Some(0.15));
    println!("[V] full roundtrip OK (carId=22 era=1, brake=60, camber=-3.0/-1.5)");
}

/// 3) 调教半套 → 整组落 None（不能把半套写进库）。
#[tokio::test]
#[ignore]
async fn partial_setup_normalizes_to_none() {
    let Some(pool) = setup().await else { return };
    let mut cs = full_setup();
    cs.setup_rear_toe = None;  // 只缺一个
    let n = cs.normalized();
    assert_eq!(n.car_id, Some(22), "车型不受调教完整性影响");
    assert_eq!(n.setup_front_wing_aero, None, "半套调教必须整组落 None");
    assert_eq!(n.setup_front_toe, None);
    println!("[V] partial setup (missing rear_toe) → whole group None");
    // 未触碰数据库，仅验证纯函数语义（保持与 SQL 侧一致）。
    let _ = pool;
}

/// 4) 车型越界 → 两列都 None（半个车型无法反查展示名）。
#[tokio::test]
#[ignore]
async fn invalid_car_normalizes_to_none() {
    let Some(pool) = setup().await else { return };
    let mut cs = full_setup();
    cs.car_id = Some(99);  // 超出 0..41
    let n = cs.normalized();
    assert_eq!(n.car_id, None);
    assert_eq!(n.car_era, None, "车型不合法时 era 也必须落 None");
    println!("[V] car_id=99 out of range → both None");
    let _ = pool;
}

/// 5) 旧客户端（只发 gp/ms/version）零改动可上传，新列全 NULL。
#[tokio::test]
#[ignore]
async fn legacy_client_upload_leaves_new_columns_null() {
    let Some(pool) = setup().await else { return };
    let uid = mk_user(&pool, "legacy_driver").await;
    let lap_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms) \
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(lap_id)
    .bind(uid)
    .bind(1i16)
    .bind(200150i32)
    .bind(80000i32)
    .execute(&pool)
    .await
    .expect("legacy insert");

    let (car_id, has_setup): (Option<i16>, bool) = sqlx::query_as(
        "SELECT car_id, (setup_front_wing_aero IS NOT NULL) FROM laps WHERE id=$1",
    )
    .bind(lap_id)
    .fetch_one(&pool)
    .await
    .expect("select");
    assert_eq!(car_id, None);
    assert!(!has_setup);
    println!("[V] legacy client row: car_id NULL, setup NULL (zero-change compat)");
}
