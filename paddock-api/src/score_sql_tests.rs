//! 端到端 SQL 正确性测试（`#[ignore]`，需要真实 Postgres）。
//!
//! **为什么必须有这一层**：`score.rs` 的单元测试锚定的是 **Rust 对照实现**，
//! 而线上跑的是 `score::base_sql` / `bonus_sql` **生成的 SQL**——两者是两份
//! 独立代码。SQL 片段写错（别名错、窗口分区错、`GREATEST` 语义错、CTE 里
//! 同名别名冲突…）单元测试全绿也发现不了。本文件把生成的 SQL 真跑一遍，
//! 并与 Rust 实现对拍。
//!
//! ⚠️ 与榜单查询**同构但独立**：不做「调 points_board 函数」式的复用，因为
//! 那会把测试绑在 handler 的取参/错误分支上。这里只复制**拼 SQL 的那几行**——
//! 一旦 handler 里的拼接逻辑改了而本文件没改，测试就不再覆盖它（可接受：
//! 拼接逻辑改动本身就是需要人工复核的事件，已由 score.rs 的注释点名）。
//!
//! 运行方式：
//! ```bash
//! docker run -d --rm --name pgtest -e POSTGRES_PASSWORD=test \
//!     -e POSTGRES_DB=paddock -p 55432:5432 postgres:16-alpine
//! DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/paddock \
//!     cargo test --release -- --ignored --nocapture
//! ```
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

use crate::score;

async fn setup() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    // 迁移（与 main.rs 同路径）：0009 建出 pedal_*/tc_*/abs_* 与四个派生位
    sqlx::migrate!("./migrations").run(&pool).await.ok()?;
    Some(pool)
}

/// 造一个用户（返回 user_id）。
async fn mk_user(pool: &PgPool, name: &str, seq: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, username, pass_hash, reg_seq) VALUES ($1,$2,'x',$3)")
        .bind(id)
        .bind(name)
        .bind(seq)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// 造一条圈（laps 留档 + best_laps，配置已派生）。
async fn mk_lap(
    pool: &PgPool,
    uid: Uuid,
    gp: i16,
    ver: i32,
    ms: i32,
    pedal: Option<&str>,
    tcm: Option<&str>,
    tcs: Option<&str>,
    absm: Option<&str>,
    abss: Option<&str>,
) {
    let cfg = score::AssistConfig {
        pedal_mode: pedal.map(Into::into),
        tc_mode: tcm.map(Into::into),
        tc_strength: tcs.map(Into::into),
        abs_mode: absm.map(Into::into),
        abs_strength: abss.map(Into::into),
    };
    let f = score::AssistFlags::from(&cfg);
    sqlx::query(
        "INSERT INTO laps (id, user_id, gp_index, version_code, lap_ms, \
                           pedal_mode, tc_mode, tc_strength, abs_mode, abs_strength) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(Uuid::now_v7()).bind(uid).bind(gp).bind(ver).bind(ms)
    .bind(&cfg.pedal_mode).bind(&cfg.tc_mode).bind(&cfg.tc_strength)
    .bind(&cfg.abs_mode).bind(&cfg.abs_strength)
    .execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms, \
                                pedal_linear, tc_off, abs_off, abs_low) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (user_id, gp_index, version_code) DO UPDATE SET \
             lap_ms=EXCLUDED.lap_ms, pedal_linear=EXCLUDED.pedal_linear, \
             tc_off=EXCLUDED.tc_off, abs_off=EXCLUDED.abs_off, abs_low=EXCLUDED.abs_low",
    )
    .bind(uid).bind(gp).bind(ver).bind(ms)
    .bind(f.pedal_linear).bind(f.tc_off).bind(f.abs_off).bind(f.abs_low)
    .execute(pool).await.unwrap();
}

/// 清空所有业务表（每次跑测试前复位；容器是一次性的，但同一容器内多次跑要干净）。
async fn wipe(pool: &PgPool) {
    for t in ["best_laps", "laps", "records", "users"] {
        sqlx::query(&format!("DELETE FROM {t}")).execute(pool).await.ok();
    }
}

/// 复刻 `leaderboard::points_board` 的 SQL 拼接（版本榜与总榜两分支）。
/// 返回 (user_id, points, best_count, zero_count, gold)，按 points 降序。
///
/// ️ 与 handler 里的拼接**必须逐字同步**（含 `scored` 带下的四个派生位列）——
/// 本文件是"能在真库上跑"的镜像，不同步就等于测试不再覆盖生产 SQL。
async fn points_board_sql(
    pool: &PgPool,
    version: Option<i32>,
) -> Vec<(Uuid, i64, i64, i64, bool)> {
    let (partition, gold_expr) = match version {
        Some(_) => ("bl.gp_index", "2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)"),
        None => (
            "bl.version_code, bl.gp_index",
            "count(*) FILTER (WHERE s.tc_off AND s.abs_off) > 0 \
             AND 2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)",
        ),
    };
    let base = score::base_sql("rank_in_track", "n_in_track");
    let bonus = score::bonus_sql("s.base_pts", "s");
    let sql = format!(
        "WITH ranked AS ( \
           SELECT bl.user_id, bl.tc_off, bl.abs_off, bl.pedal_linear, bl.abs_low, \
                  rank() OVER (PARTITION BY {partition} ORDER BY bl.lap_ms ASC) AS rank_in_track, \
                  count(*) OVER (PARTITION BY {partition}) AS n_in_track \
           FROM best_laps bl {vf} \
         ), scored AS ( \
           SELECT user_id, tc_off, abs_off, pedal_linear, abs_low, {base} AS base_pts FROM ranked \
         ) \
         SELECT u.id, sum(s.base_pts + {bonus})::bigint, count(*)::bigint, \
                count(*) FILTER (WHERE s.tc_off AND s.abs_off)::bigint, ({gold_expr}) \
         FROM scored s JOIN users u ON u.id = s.user_id \
         GROUP BY u.id ORDER BY 2 DESC",
        vf = version.map(|_| "WHERE bl.version_code = $1".to_string()).unwrap_or_default(),
    );
    let mut q = sqlx::query_as::<_, (Uuid, i64, i64, i64, bool)>(&sql);
    if let Some(v) = version { q = q.bind(v); }
    q.fetch_all(pool).await.unwrap()
}

/// 基础分 SQL 与 Rust 对照实现必须在同一批输入上给出相同结果。
#[tokio::test]
#[ignore]
async fn sql_base_points_matches_rust() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 3 人同一赛道同一版本：100 / round(2*100/3)=67 / round(1*100/3)=33
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let uid = mk_user(&pool, name, i as i64 + 1).await;
        mk_lap(&pool, uid, 0, 200150, 90_000 + i as i32 * 1000, None, None, None, None, None).await;
    }
    let rows = points_board_sql(&pool, Some(200150)).await;
    assert_eq!(rows.len(), 3);
    let pts: Vec<i64> = rows.iter().map(|r| r.1).collect();
    assert_eq!(pts, vec![100, 67, 33], "SQL 基础分与 v40 公式不符");
    // 与 Rust 对照实现逐名次对拍
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.1, score::base_points(i as i64 + 1, 3), "第 {} 名 SQL≠Rust", i + 1);
    }
}

/// 完整加成链：4 人 4 档配置，逐行核对总分 = 基础分 + 加成（SQL 侧）。
#[tokio::test]
#[ignore]
async fn sql_bonus_matches_rust() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 名次由圈速决定：a 最快（100 基础分）
    // a = 四项全达标（+50% → 150）；b = 仅踏板（+10% → 75+8）；c = 仅关 ABS（+30%）；d = 全缺失
    let a = mk_user(&pool, "a", 1).await;
    let b = mk_user(&pool, "b", 2).await;
    let c = mk_user(&pool, "c", 3).await;
    let d = mk_user(&pool, "d", 4).await;
    mk_lap(&pool, a, 0, 200150, 90_000, Some("dual"), Some("custom"), Some("off"), Some("custom"), Some("off")).await;
    mk_lap(&pool, b, 0, 200150, 91_000, Some("single"), Some("default"), Some("stock"), Some("default"), Some("stock")).await;
    mk_lap(&pool, c, 0, 200150, 92_000, Some("off"), Some("default"), Some("stock"), Some("custom"), Some("off")).await;
    mk_lap(&pool, d, 0, 200150, 93_000, None, None, None, None, None).await;

    let rows = points_board_sql(&pool, Some(200150)).await;
    let by_id: std::collections::HashMap<Uuid, i64> = rows.iter().map(|r| (r.0, r.1)).collect();
    // 4 人：基础分 100 / 75 / 50 / 25
    assert_eq!(by_id[&a], 150, "全达标：100 + round(100*0.5)=50");
    assert_eq!(by_id[&b], 83, "仅踏板：75 + round(75*0.1)=8 → 83");
    assert_eq!(by_id[&c], 65, "仅关 ABS：50 + round(50*0.3)=15 → 65");
    assert_eq!(by_id[&d], 25, "全缺失：25 + 0");
}

/// 虚位（第 N+1 名）不给加成——base=0 时加成必须恒 0。
#[tokio::test]
#[ignore]
async fn sql_no_bonus_at_zero_base() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 2 人榜：a = 100 基础分 + 四项全达标；b 慢 → 基础分 50
    let a = mk_user(&pool, "a", 1).await;
    let b = mk_user(&pool, "b", 2).await;
    mk_lap(&pool, a, 0, 200150, 90_000, Some("dual"), Some("custom"), Some("off"), Some("custom"), Some("off")).await;
    mk_lap(&pool, b, 0, 200150, 91_000, Some("dual"), Some("custom"), Some("off"), Some("custom"), Some("off")).await;
    let rows = points_board_sql(&pool, Some(200150)).await;
    assert_eq!(rows[0].1, 150, "第 1 名 100+50");
    assert_eq!(rows[1].1, 75, "第 2 名 50+25");

    // base=0 分支：直接对 SQL 片段取值（虚位不落在 best_laps 里，无法从榜上构造）。
    // 用内联 VALUES 提供 base 与四个派生位，逐项验证加成恒 0。
    let bonus = score::bonus_sql("v.base", "v");
    let zero_base: i64 = sqlx::query_scalar(&format!(
        "SELECT {bonus}::bigint FROM (VALUES (0, true, true, true, true)) v(base, pedal_linear, tc_off, abs_off, abs_low)"
    ))
    .fetch_one(&pool).await.unwrap();
    assert_eq!(zero_base, 0, "base=0 → 加成恒 0（虚位无分不给加成）");

    // 对照：base>0 且四项全达标 → 50%
    let pos: i64 = sqlx::query_scalar(&format!(
        "SELECT {bonus}::bigint FROM (VALUES (4, true, true, true, true)) v(base, pedal_linear, tc_off, abs_off, abs_low)"
    ))
    .fetch_one(&pool).await.unwrap();
    assert_eq!(pos, 2, "base=4 × 50% = 2");
}

/// 零辅助金标的三种场景（版本榜）与总榜的跨版本比值。
#[tokio::test]
#[ignore]
async fn sql_gold_flag_rules() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "u", 1).await;
    // 8.0.6：4 条赛道，2 条零辅助 → 2*2 >= 4 → 版本榜金标
    for gp in 0..4i16 {
        let zero = gp < 2;
        mk_lap(
            &pool, u, gp, 200150, 90_000 + gp as i32 * 100, Some("dual"),
            Some("custom"), Some(if zero { "off" } else { "stock" }),
            Some("custom"), Some(if zero { "off" } else { "stock" }),
        ).await;
    }
    let v = points_board_sql(&pool, Some(200150)).await;
    assert_eq!((v[0].2, v[0].3), (4, 2), "4 条最快圈里 2 条零辅助");
    assert!(v[0].4, "恰好 50% → 版本榜金标");

    // 再加 1 条（非零辅助）→ 2/5 = 40% → 金标消失（未达 50%）
    mk_lap(&pool, u, 4, 200150, 99_000, Some("off"), Some("default"), Some("stock"), Some("default"), Some("stock")).await;
    let v2 = points_board_sql(&pool, Some(200150)).await;
    assert_eq!((v2[0].2, v2[0].3), (5, 2));
    assert!(!v2[0].4, "2/5 = 40% < 50% → 无金标");

    // 总榜：8.0.4 加 4 条全非零辅助 → 2/9 ≈ 22% → 总榜无金标（版本榜仍 40%）
    for gp in 0..4i16 {
        mk_lap(&pool, u, gp, 200146, 80_000 + gp as i32 * 100, Some("off"),
               Some("default"), Some("stock"), Some("default"), Some("stock")).await;
    }
    let all = points_board_sql(&pool, None).await;
    assert_eq!((all[0].2, all[0].3), (9, 2), "总榜 9 条最快圈（跨版本）");
    assert!(!all[0].4, "总榜 2/9 < 50% → 无金标");
    // 总榜必须比版本榜严格更严（这里是"版本有金标而总榜无"）
    assert!(!all[0].4);
}

/// 总榜的"存在性"条件：一个零辅助都没有时，哪怕比值公式算出 0>=n 也不得给金标。
#[tokio::test]
#[ignore]
async fn sql_gold_requires_at_least_one_zero_lap() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "u", 1).await;
    // 空集是不出现在 best_laps 里的；用 1 条非零辅助圈验证 0 >= 1 为假
    mk_lap(&pool, u, 0, 200150, 90_000, Some("off"), Some("default"), Some("stock"), Some("default"), Some("stock")).await;
    let all = points_board_sql(&pool, None).await;
    assert_eq!(all[0].3, 0, "零辅助计数为 0");
    assert!(!all[0].4, "0 个零辅助绝不给总榜金标");
}

/// 赛道榜 SQL：跨版本 best-of-best 取到的那条圈的配置必须与圈速同源。
#[tokio::test]
#[ignore]
async fn sql_track_board_picks_config_of_winning_lap() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "u", 1).await;
    // 8.0.4 有更快的零辅助圈；8.0.6 有更慢的非零辅助圈
    mk_lap(&pool, u, 2, 200146, 80_000, Some("dual"), Some("custom"), Some("off"), Some("custom"), Some("off")).await;
    mk_lap(&pool, u, 2, 200150, 90_000, Some("off"), Some("default"), Some("stock"), Some("default"), Some("stock")).await;
    // 总榜（无版本）→ 应取 8.0.4 那条（更快）且带其零辅助配置
    let row: (i32, bool) = sqlx::query_as(
        "SELECT lap_ms, (tc_off AND abs_off) FROM ( \
           SELECT DISTINCT ON (b.user_id) b.lap_ms, b.tc_off, b.abs_off \
           FROM best_laps b WHERE b.gp_index=$1 ORDER BY b.user_id, b.lap_ms ASC \
         ) t",
    )
    .bind(2i16)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, 80_000, "取跨版本最快圈");
    assert!(row.1, "金标跟获胜那条圈走");

    // 显式版本过滤 → 取该版本那条（非零辅助）
    let row2: (i32, bool) = sqlx::query_as(
        "SELECT lap_ms, (tc_off AND abs_off) FROM ( \
           SELECT DISTINCT ON (b.user_id) b.lap_ms, b.tc_off, b.abs_off \
           FROM best_laps b WHERE b.gp_index=$1 AND b.version_code=$2 \
           ORDER BY b.user_id, b.lap_ms ASC \
         ) t",
    )
    .bind(2i16).bind(200150)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row2, (90_000, false), "版本榜只看该版本");
}

/// 展示 token 与原始五维的**往返一致性**：管理端"看到什么就编辑什么"。
///
/// ⚠️ 这条不变量有两处易错点：① `mode=default` 与 `custom+stock` 必须折叠成
/// 同一个 token（否则管理员会看到"默认"，编辑时却变成"自定义/最高"）；
/// ② 折叠后展开必须回到**计分等价**的原始值（`stock` → `default`，而非 `custom+stock`，
/// 因为后者虽然同分，但语义是"开过自定义"）。
#[test]
fn assist_token_round_trip() {
    use crate::api::laps::{assist_display, assist_tokens, tokens_to_raw};

    // ① 默认 与 自定义+最高 → 同一个 token "stock"（计分等价）
    let (_, tc_default, _) =
        assist_tokens(Some("dual"), Some("default"), Some("stock"), None, None);
    let (_, tc_custom_max, _) =
        assist_tokens(Some("dual"), Some("custom"), Some("stock"), None, None);
    assert_eq!(tc_default, "stock");
    assert_eq!(tc_custom_max, tc_default, "默认与自定义+最高必须折叠为同一 token");

    // ② 缺失 → 空 token → 展示"缺失"
    let (p, tc, abs) = assist_tokens(None, None, None, None, None);
    assert_eq!((p.as_str(), tc.as_str(), abs.as_str()), ("", "", ""));
    let (pd, tcd, absd) = assist_display(None, None, None, None, None);
    assert_eq!((pd.as_str(), tcd.as_str(), absd.as_str()), ("缺失", "缺失", "缺失"));

    // ③ 真实取值逐项展示
    let (pd, tcd, absd) =
        assist_display(Some("dual"), Some("custom"), Some("off"), Some("custom"), Some("medium"));
    assert_eq!((pd.as_str(), tcd.as_str(), absd.as_str()), ("双踏板", "关", "中"));

    // ④ token → 原始五维：stock 展开为 default（语义"没调低辅助"，不是"开过自定义"）
    let (p, tcm, tcs, absm, abss) = tokens_to_raw("dual", "stock", "off");
    assert_eq!(p.as_deref(), Some("dual"));
    assert_eq!((tcm.as_deref(), tcs.as_deref()), (Some("default"), Some("stock")));
    assert_eq!((absm.as_deref(), abss.as_deref()), (Some("custom"), Some("off")));
    // 空 token 展开为全 None（缺失，不加分）
    let (p, tcm, tcs, absm, abss) = tokens_to_raw("", "", "");
    assert!(p.is_none() && tcm.is_none() && tcs.is_none() && absm.is_none() && abss.is_none());

    // ⑤ 往返：原始 → token → 原始，派生布尔必须完全一致（这才是"计分等价"的硬校验）
    for (tcm, tcs) in [
        (Some("default"), Some("stock")),
        (Some("custom"), Some("stock")),
        (Some("custom"), Some("off")),
        (Some("custom"), Some("weak")),
        (Some("custom"), Some("medium")),
        (Some("custom"), Some("strong")),
        (None, None),
    ] {
        let (_, tok, _) = assist_tokens(None, tcm, tcs, None, None);
        let (_, tcm2, tcs2, _, _) = tokens_to_raw("", &tok, "");
        let before = score::AssistFlags::from(&score::AssistConfig {
            pedal_mode: None,
            tc_mode: tcm.map(str::to_string),
            tc_strength: tcs.map(str::to_string),
            abs_mode: None,
            abs_strength: None,
        });
        let after = score::AssistFlags::from(&score::AssistConfig {
            pedal_mode: None,
            tc_mode: tcm2,
            tc_strength: tcs2,
            abs_mode: None,
            abs_strength: None,
        });
        assert_eq!(
            (before.tc_off, before.abs_low),
            (after.tc_off, after.abs_low),
            "token 往返后 TC 计分语义必须不变（原 {tcm:?}/{tcs:?} → token {tok:?}）"
        );
    }
}
/// 迁移 0009 的列存在且默认为"缺失"（旧行不加分）——防迁移漏写的守卫。
#[tokio::test]
#[ignore]
async fn migration_adds_assist_columns_with_false_defaults() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 用**不含配置列**的旧式插入模拟历史数据，读出派生位应为 false
    let uid = mk_user(&pool, "legacy", 1).await;
    sqlx::query("INSERT INTO laps (id,user_id,gp_index,version_code,lap_ms) VALUES ($1,$2,0,200150,90000)")
        .bind(Uuid::now_v7()).bind(uid).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO best_laps (user_id,gp_index,version_code,lap_ms) VALUES ($1,0,200150,90000)")
        .bind(uid).execute(&pool).await.unwrap();
    let (pl, tco, abo, abl): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT pedal_linear, tc_off, abs_off, abs_low FROM best_laps WHERE user_id=$1",
    )
    .bind(uid).fetch_one(&pool).await.unwrap();
    assert!(!pl && !tco && !abo && !abl, "旧数据的派生位必须全 false（缺失=不加分）");
    // 配置列本身应为 NULL
    let pm: Option<String> = sqlx::query_scalar("SELECT pedal_mode FROM laps WHERE user_id=$1")
        .bind(uid).fetch_one(&pool).await.unwrap();
    assert!(pm.is_none(), "旧数据配置列必须为 NULL");
}