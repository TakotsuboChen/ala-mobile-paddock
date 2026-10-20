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
/// 2026-10-19：窗口定义改为直接复用 `score::scored_cte`，与 handler 同源，
/// 不再手抄一遍 `ranked` + `scored`。
async fn points_board_sql(
    pool: &PgPool,
    version: Option<i32>,
) -> Vec<(Uuid, i64, i64, i64, bool)> {
    let gold_expr = match version {
        Some(_) => "2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)",
        None => {
            "count(*) FILTER (WHERE s.tc_off AND s.abs_off) > 0 \
             AND 2 * count(*) FILTER (WHERE s.tc_off AND s.abs_off) >= count(*)"
        }
    };
    let bonus = score::bonus_sql("s.base_pts", "s");
    let sql = format!(
        "{scored} \
         SELECT u.id, sum(s.base_pts + {bonus})::bigint, count(*)::bigint, \
                count(*) FILTER (WHERE s.tc_off AND s.abs_off)::bigint, ({gold_expr}) \
         FROM scored s JOIN users u ON u.id = s.user_id \
         GROUP BY u.id ORDER BY 2 DESC",
        scored = score::scored_cte(if version.is_some() {
            "WHERE bl.version_code = $1"
        } else {
            ""
        }),
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

/// 分页（2026-10-18 新增）：`total` 必须与分页无关，且跨页不重复、不漏项。
///
/// ## 为什么这条测试必须存在
///
/// 分页有三个只有真库能暴露的错误，Rust 侧单元测试全绿也发现不了：
/// ① **排序没下推到 SQL** —— 原实现 `sort_by` 后再切片，切的是未排序结果的
///    前 N 条，跨页重复/漏项；
/// ② **名次按页重排** —— 原赛道榜在 Rust 里 `enumerate()` 生成 rank，
///    第二页从 1 重新编号；
/// ③ **`count(*) OVER ()` 在空页拿不到值** —— offset 越界返回 0 行，
///    客户端会把"没有更多了"误判成"这个榜是空的"。
///
/// 本用例复刻 handler 的分页外壳（`agg` + `count(*) OVER ()`），逐条断言。
#[tokio::test]
#[ignore]
async fn sql_pagination_is_stable_and_total_is_page_independent() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 5 个用户，同赛道同版本，圈速递增（名次可预测：u1 最快）
    let mut ids = Vec::new();
    for i in 0..5i64 {
        let u = mk_user(&pool, &format!("pager{i}"), i + 1).await;
        ids.push(u);
        mk_lap(
            &pool, u, 3, 200150, 90_000 + (i as i32) * 1_000,
            Some("off"), Some("default"), Some("stock"), Some("default"), Some("stock"),
        ).await;
    }

    // 复刻 points_board 的分页外壳（排序下推 + total 窗口）。
    // 2026-10-19：窗口段复用 score::scored_cte（与 handler 同源）。
    let bonus = score::bonus_sql("s.base_pts", "s");
    let sql = format!(
        "{scored}, \
         agg AS ( \
           SELECT u.id AS user_id, u.username AS username, \
                  sum(s.base_pts + {bonus})::bigint AS points \
           FROM scored s JOIN users u ON u.id = s.user_id \
           GROUP BY u.id, u.username \
         ) \
         SELECT username, points, count(*) OVER ()::bigint AS total FROM agg \
         ORDER BY points DESC, username ASC LIMIT $2 OFFSET $3",
        scored = score::scored_cte("WHERE bl.version_code = $1"),
    );

    let page1: Vec<(String, i64, i64)> = sqlx::query_as(&sql)
        .bind(200150).bind(2i64).bind(0i64)
        .fetch_all(&pool).await.unwrap();
    let page2: Vec<(String, i64, i64)> = sqlx::query_as(&sql)
        .bind(200150).bind(2i64).bind(2i64)
        .fetch_all(&pool).await.unwrap();

    assert_eq!(page1.len(), 2, "第一页拿满 limit");
    assert_eq!(page2.len(), 2, "第二页拿满 limit");
    // ① total 与分页无关：两页必须都报 5
    assert_eq!(page1[0].2, 5, "total 是全量条数，不受 limit 影响");
    assert_eq!(page2[0].2, 5, "第二页的 total 同样是全量条数");
    // ② 跨页不重复、不漏项
    let mut names: Vec<String> = page1.iter().chain(page2.iter()).map(|r| r.0.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 4, "两页合起来必须是 4 个不同用户，无重复");
    // ③ 排序稳定：第一页分数必须 >= 第二页（排序下推到 SQL 的直接后果）
    assert!(
        page1.last().unwrap().1 >= page2.first().unwrap().1,
        "第一页末位分数不得低于第二页首位——否则说明排序没在 SQL 里生效"
    );

    // ④ 空页（offset 越界）时 total 拿不到 → handler 兜底为 0，entries 为空。
    //    这里断言"确实返回 0 行"，handler 侧的 `unwrap_or(0)` 由此成立。
    let empty: Vec<(String, i64, i64)> = sqlx::query_as(&sql)
        .bind(200150).bind(2i64).bind(99i64)
        .fetch_all(&pool).await.unwrap();
    assert!(empty.is_empty(), "越界 offset 返回空页（handler 据此把 total 记 0 并停止翻页）");
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
/// **主页名次必须等于榜单行号**（2026-10-19 新增）。
///
/// 这条测试存在的理由：`score::user_points_and_rank` 用 `row_number()`，
/// 而"看起来很自然"的 `rank()` 在同分时会跳号（1,1,3）。车手主页显示
/// "总排名 N"，用户在积分榜里数行号却是 N+1 —— 肉眼可见的矛盾。
/// 单元测试发现不了（`rank()` 与 `row_number()` 在无并列时结果相同），
/// 只有真库 + 并列数据才能暴露。
#[tokio::test]
#[ignore]
async fn driver_rank_matches_board_row_number() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    // 造三人在同一赛道同一版本，且**成绩完全相同** → 积分并列。
    // 积分相同后由 username ASC 定序，故 c > b > a。
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let uid = mk_user(&pool, name, i as i64 + 1).await;
        mk_lap(
            &pool, uid, 0, 200150, 90_000,
            Some("off"), Some("default"), Some("stock"), Some("default"), Some("stock"),
        ).await;
    }
    let ids: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, username FROM users")
        .fetch_all(&pool).await.unwrap();

    // ① 榜单行号（与 points_board 的 ORDER BY 逐字一致）
    let board = points_board_sql(&pool, Some(200150)).await;
    let board_names: Vec<String> = board.iter().map(|r| {
        ids.iter().find(|(id, _)| id == &r.0).unwrap().1.clone()
    }).collect();
    assert_eq!(board_names, vec!["a", "b", "c"], "同分按 username 升序定序");

    // ② 每个人的 rank 必须 = 他在榜单里的下标 + 1
    for (idx, (uid, name)) in ids.iter().enumerate() {
        let (_points, rank) = score::user_points_and_rank(&pool, *uid, Some(200150))
            .await.unwrap();
        assert_eq!(
            rank, Some(idx as i64 + 1),
            "{name} 的主页名次必须等于榜单行号（rank() 会跳号，row_number() 不会）"
        );
    }

    // ③ 反证：`rank()` 在这里会给出 1,1,1（三人同分），与行号 1,2,3 不符。
    let rank_fn: Vec<i64> = sqlx::query_scalar(
        "SELECT rank() OVER (ORDER BY points DESC)::bigint FROM ( \
           SELECT 100::bigint AS points FROM generate_series(1,3) \
         ) t",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(rank_fn, vec![1, 1, 1], "rank() 对同分并列——这正是不能用它的原因");
}

/// 版本维度四值（版本积分/版本排名/纪录数/纪录排名）的口径守卫。
///
/// 用户定案（2026-10-19）：纪录数 = **版本 × 赛道**，同一条赛道在 8.0.4 与
/// 8.0.6 各算一条、不跨版本混算。这条测试专门盯住"别把版本过滤丢了"。
#[tokio::test]
#[ignore]
async fn version_scope_is_not_cross_version() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let a = mk_user(&pool, "a", 1).await;
    let b = mk_user(&pool, "b", 2).await;
    // a 在**两个版本**的 0 号赛道都是纪录保持者
    for ver in [200146, 200150] {
        mk_lap(&pool, a, 0, ver, 90_000, None, None, None, None, None).await;
        sqlx::query(
            "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id) \
             VALUES (0,'version',$1,90000,$2)",
        ).bind(ver).bind(a).execute(&pool).await.unwrap();
    }
    // b 只在旧版本的 1 号赛道有纪录
    mk_lap(&pool, b, 1, 200146, 95_000, None, None, None, None, None).await;
    sqlx::query(
        "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id) \
         VALUES (1,'version',200146,95000,$1)",
    ).bind(b).execute(&pool).await.unwrap();

    // 最新版本 = max(version_code) = 200150
    let latest: Option<i32> = sqlx::query_scalar("SELECT max(version_code) FROM best_laps")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(latest, Some(200150));

    // a 在最新版本持有 1 条纪录（不是 2 条——旧版本那条不计入）
    let a_cnt: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM records \
         WHERE kind='version' AND version_code=$1 AND user_id=$2",
    ).bind(200150).bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(a_cnt, 1, "纪录数按版本切分，旧版本的纪录不能算进最新版本");

    // ⚠️ 上面这条只验证"单版本切片"的 SQL 正确性；**车手主页展示的 `record_count`
    //    用的是跨版本累计口径**（见下面 record_count_crosses_versions 测试）——
    //    用户 2026-10-19 定案：只查 max(version_code) 会让"只在旧版本有纪录的人"
    //    显示 0（实机：Takotsubo 在 8.0.4 有 5 条、8.0.6 零条 ⇒ 旧实现显示 0）。

    // b 只在旧版本有 1 条纪录 → 新口径下仍参与排名（跨版本累计，不按版本截断）
    let b_rank: Option<i64> = sqlx::query_scalar(
        "WITH per_user AS ( \
           SELECT user_id, count(*)::bigint AS cnt \
           FROM records WHERE kind='version' \
           GROUP BY user_id \
         ), ranked AS ( \
           SELECT p.user_id, \
                  row_number() OVER (ORDER BY p.cnt DESC, u.reg_seq ASC)::bigint AS \"rank\" \
           FROM per_user p JOIN users u ON u.id = p.user_id \
         ) \
         SELECT \"rank\" FROM ranked WHERE user_id=$1",
    ).bind(b).fetch_optional(&pool).await.unwrap();
    assert_eq!(b_rank, Some(2), "旧版本的纪录也计入（b 有 1 条，排在 a 的 2 条之后）");

    // 版本积分：a 在 200150 有 1 条赛道第一（N=1 → 100 分）；总积分含两版本 → 200
    let (v_pts, v_rank) = score::user_points_and_rank(&pool, a, Some(200150)).await.unwrap();
    assert_eq!(v_pts, 100, "版本积分只算该版本");
    assert_eq!(v_rank, Some(1));
    let (t_pts, _) = score::user_points_and_rank(&pool, a, None).await.unwrap();
    assert_eq!(t_pts, 200, "总积分 = 各版本独立计分累加（版本=独立赛季）");
}

/// **全服最快圈数量必须跨版本累计**（2026-10-19 实机 bug 回归测试）。
///
/// 旧实现查 `version_code = max(...)`，导致"只在旧版本有纪录的人"显示 0。
/// 实机证据：Takotsubo 在 8.0.4 持有 5 条版本纪录（gp 0/1/2/5/12）、8.0.6 零条，
/// 车手主页却显示「全服最快圈数量 0」。
///
/// 用户定案口径：「版本-赛道记录，同赛道也分版本，不是所有版本混一起」——
/// 即 `records(kind='version')` 里该用户持有的**组合数**：同赛道在两个版本各算一条，
/// 但**不按版本截断**。
#[tokio::test]
#[ignore]
async fn record_count_crosses_versions() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let a = mk_user(&pool, "a", 1).await;
    let b = mk_user(&pool, "b", 2).await;

    // a：旧版本 2 条 + 最新版本 1 条 → 跨版本累计 3
    for (gp, ver) in [(0i16, 200146), (1, 200146), (2, 200150)] {
        sqlx::query(
            "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id) \
             VALUES ($1,'version',$2,90000,$3)",
        ).bind(gp).bind(ver).bind(a).execute(&pool).await.unwrap();
    }
    // b：只有最新版本 1 条
    sqlx::query(
        "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id) \
         VALUES (0,'version',200150,91000,$1)",
    ).bind(b).execute(&pool).await.unwrap();

    // ① 跨版本计数：a = 3（旧 2 + 新 1），b = 1
    let a_cnt: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM records WHERE kind='version' AND user_id=$1",
    ).bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(a_cnt, 3, "全服最快圈数量必须跨版本累计（旧实现的 max(version) 截断会漏掉旧版本）");

    // ② 反证：只查最新版本会得到 1（这就是 bug 的表现）
    let a_latest: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM records \
         WHERE kind='version' AND version_code=200150 AND user_id=$1",
    ).bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(a_latest, 1, "按最新版本截断只有 1 条 —— 正是用户报的「显示 0/偏小」的成因");

    // ③ 名次按跨版本计数降序：a(3) 第 1、b(1) 第 2
    let ranked: Vec<(Uuid, i64)> = sqlx::query_as(
        "WITH per_user AS ( \
           SELECT user_id, count(*)::bigint AS cnt \
           FROM records WHERE kind='version' GROUP BY user_id \
         ) \
         SELECT p.user_id, \
                row_number() OVER (ORDER BY p.cnt DESC, u.reg_seq ASC)::bigint \
         FROM per_user p JOIN users u ON u.id = p.user_id \
         ORDER BY 2",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(ranked, vec![(a, 1), (b, 2)], "名次按跨版本纪录数降序，同数按车手号定序");
}

// ============ 屏蔽缺失配置记录（POST /v1/me/hide-missing）============
//
// ⚠️ 这几条测试锁定的是 **2026-10-20 修正后的口径**：屏蔽范围 = 该用户**全部**
//    配置缺失的圈，不是「每个维度最快的那一条」。
//
//    首版只屏蔽每维度获胜圈，被屏蔽那条移出 best_laps 后，回放会挑**下一条最快
//    的圈**顶上——而它往往同样缺失，于是「缺失」与「最佳」同挂一条记录。
//    实机数据（飞行雪绒）：65 条缺失圈只标了 7 条 = 7 个维度的获胜圈。
//
//    修复后必须同时满足三条不变量：
//      A. 全部缺失圈进 hidden_missing_laps（不只获胜圈）；
//      B. best_laps 剩下的行**绝不**指向缺失配置的圈；
//      C. records 绝不指向被屏蔽的圈。

/// 复刻 `api::account::hide_missing` 的 SQL 步骤（不含 HTTP 层）。
///
/// 与 handler **逐字同源**（判据复用 `laps::ASSIST_MISSING_SQL`），改动 handler
/// 时必须同步这里，否则测试不再覆盖生产 SQL。
async fn hide_missing_sql(pool: &PgPool, uid: Uuid) -> i64 {
    let missing: Vec<(Uuid, i16, i32)> = sqlx::query_as(&format!(
        "SELECT l.id, l.gp_index, l.version_code FROM laps l \
         WHERE l.user_id = $1 AND {}",
        crate::api::laps::ASSIST_MISSING_SQL
    ))
    .bind(uid)
    .fetch_all(pool)
    .await
    .unwrap();

    for (lap_id, _, _) in &missing {
        sqlx::query(
            "INSERT INTO hidden_missing_laps (user_id, lap_id) VALUES ($1, $2) \
             ON CONFLICT (user_id, lap_id) DO NOTHING",
        )
        .bind(uid).bind(lap_id).execute(pool).await.unwrap();
    }

    let mut dims: Vec<(i16, i32)> = missing.iter().map(|(_, gp, ver)| (*gp, *ver)).collect();
    dims.sort_unstable();
    dims.dedup();

    for (gp, ver) in &dims {
        sqlx::query("DELETE FROM best_laps WHERE user_id=$1 AND gp_index=$2 AND version_code=$3")
            .bind(uid).bind(gp).bind(ver).execute(pool).await.unwrap();

        let next: Option<(i32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as(
                "SELECT l.lap_ms, l.pedal_mode, l.tc_mode, l.tc_strength, l.abs_mode, l.abs_strength \
                 FROM laps l \
                 WHERE l.user_id=$1 AND l.gp_index=$2 AND l.version_code=$3 \
                   AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h \
                                   WHERE h.user_id=l.user_id AND h.lap_id=l.id) \
                 ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
            )
            .bind(uid).bind(gp).bind(ver).fetch_optional(pool).await.unwrap();
        if let Some((ms, pedal, tcm, tcs, absm, abss)) = next {
            let f = crate::api::laps::flags_of(
                pedal.as_deref(), tcm.as_deref(), tcs.as_deref(),
                absm.as_deref(), abss.as_deref(),
            );
            sqlx::query(
                "INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms, \
                                        pedal_linear, tc_off, abs_off, abs_low) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            )
            .bind(uid).bind(gp).bind(ver).bind(ms)
            .bind(f.pedal_linear).bind(f.tc_off).bind(f.abs_off).bind(f.abs_low)
            .execute(pool).await.unwrap();
        }
    }

    let mut gps: Vec<i16> = dims.iter().map(|(gp, _)| *gp).collect();
    gps.sort_unstable();
    gps.dedup();
    for gp in &gps {
        let va: Option<(i32, Uuid)> = sqlx::query_as(
            "SELECT l.lap_ms, l.user_id FROM laps l WHERE l.gp_index=$1 \
               AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h WHERE h.lap_id=l.id) \
             ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
        ).bind(gp).fetch_optional(pool).await.unwrap();
        apply_record_sql(pool, *gp, "alltime", 0, va).await;

        let versions: Vec<i32> =
            sqlx::query_scalar("SELECT DISTINCT version_code FROM laps WHERE gp_index=$1")
                .bind(gp).fetch_all(pool).await.unwrap();
        for ver in versions {
            let vb: Option<(i32, Uuid)> = sqlx::query_as(
                "SELECT l.lap_ms, l.user_id FROM laps l \
                 WHERE l.gp_index=$1 AND l.version_code=$2 \
                   AND NOT EXISTS (SELECT 1 FROM hidden_missing_laps h WHERE h.lap_id=l.id) \
                 ORDER BY l.lap_ms ASC, l.created_at ASC LIMIT 1",
            ).bind(gp).bind(ver).fetch_optional(pool).await.unwrap();
            apply_record_sql(pool, *gp, "version", ver, vb).await;
        }
    }
    missing.len() as i64
}

/// 与 `api::account::apply_record` 同语义。
async fn apply_record_sql(
    pool: &PgPool, gp: i16, kind: &str, vc: i32, best: Option<(i32, Uuid)>,
) {
    match best {
        None => {
            sqlx::query("DELETE FROM records WHERE gp_index=$1 AND kind=$2 AND version_code=$3")
                .bind(gp).bind(kind).bind(vc).execute(pool).await.unwrap();
        }
        Some((ms, uid)) => {
            sqlx::query(
                "INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,now()) \
                 ON CONFLICT (gp_index, kind, version_code) \
                 DO UPDATE SET lap_ms=EXCLUDED.lap_ms, user_id=EXCLUDED.user_id, updated_at=now()",
            ).bind(gp).bind(kind).bind(vc).bind(ms).bind(uid)
             .execute(pool).await.unwrap();
        }
    }
}

/// **核心回归**：屏蔽后 best_laps 不得再指向任何缺失配置的圈。
///
/// 场景精确复刻实机数据形态：某维度最快圈缺失、次快圈**也缺失**、第三快完整。
/// 旧实现只屏蔽最快那条 → 次快圈接棒成为「最佳」，用户看到「缺失 + 最佳」同挂。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn hide_missing_removes_all_missing_laps_not_just_winners() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "fly", 1).await;

    // GP 0：70000 缺失 / 72000 缺失 / 75000 完整 —— 三层梯度是关键
    mk_lap(&pool, u, 0, 200150, 70000, None, None, None, None, None).await;
    mk_lap(&pool, u, 0, 200150, 72000, None, None, None, None, None).await;
    mk_lap(&pool, u, 0, 200150, 75000, Some("dual"), Some("custom"), Some("medium"),
           Some("custom"), Some("weak")).await;
    // GP 1：80000 缺失 / 81000 完整 —— 屏蔽后完整圈接棒
    mk_lap(&pool, u, 1, 200150, 80000, None, None, None, None, None).await;
    mk_lap(&pool, u, 1, 200150, 81000, Some("off"), Some("default"), Some("stock"),
           Some("default"), Some("stock")).await;
    // GP 2：90000 缺失（唯一一条）—— 屏蔽后维度整个消失
    mk_lap(&pool, u, 2, 200150, 90000, None, None, None, None, None).await;

    let n = hide_missing_sql(&pool, u).await;
    assert_eq!(n, 4, "应屏蔽全部 4 条缺失圈（GP0×2 + GP1×1 + GP2×1）");

    // 不变量 A：全部缺失圈都在表里
    let hidden_cnt: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM hidden_missing_laps WHERE user_id=$1",
    ).bind(u).fetch_one(&pool).await.unwrap();
    assert_eq!(hidden_cnt, 4, "A：全部缺失圈都要标「已屏蔽」，不只获胜圈");

    // 不变量 B：best_laps 剩下的行绝不指向缺失配置的圈
    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM best_laps b \
         JOIN laps l ON l.user_id=b.user_id AND l.gp_index=b.gp_index \
                    AND l.version_code=b.version_code AND l.lap_ms=b.lap_ms \
         WHERE b.user_id=$1 AND (l.pedal_mode IS NULL OR l.tc_mode IS NULL \
               OR l.tc_strength IS NULL OR l.abs_mode IS NULL OR l.abs_strength IS NULL)",
    ).bind(u).fetch_one(&pool).await.unwrap();
    assert_eq!(bad, 0, "B：榜上的最佳圈绝不能是缺失配置的（旧实现的核心 bug）");

    // 具体值：GP0 → 75000（完整），GP1 → 81000（完整），GP2 → 无行
    let best: Vec<(i16, i32)> = sqlx::query_as(
        "SELECT gp_index, lap_ms FROM best_laps WHERE user_id=$1 ORDER BY gp_index",
    ).bind(u).fetch_all(&pool).await.unwrap();
    assert_eq!(best, vec![(0, 75000), (1, 81000)], "GP2 无完整圈 → 维度整体退出榜单");

    // 不变量 C：records 不指向被屏蔽的圈
    let rec_bad: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM records r \
         JOIN hidden_missing_laps h ON h.user_id=$1 \
         JOIN laps l ON l.id=h.lap_id \
         WHERE r.user_id=$1 AND r.gp_index=l.gp_index AND r.lap_ms=l.lap_ms",
    ).bind(u).fetch_one(&pool).await.unwrap();
    assert_eq!(rec_bad, 0, "C：全服纪录必须让位给未被屏蔽的圈");
    let rec: Vec<(i16, i32)> = sqlx::query_as(
        "SELECT gp_index, lap_ms FROM records WHERE kind='alltime' ORDER BY gp_index",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(rec, vec![(0, 75000), (1, 81000)], "GP2 无可用圈 → 纪录行删除");

    wipe(&pool).await;
}

/// 候选预判（`count_missing_candidates`）必须与实际屏蔽行集同源。
///
/// 旧实现算的是「获胜圈缺失的维度数」，新实现算「全部缺失圈数」。若两者分叉，
/// 用户会看到「有候选」→ 点了却报「没有可屏蔽的记录」，或反之。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn missing_candidate_count_matches_actual_hidden_rows() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "fly2", 2).await;

    // 4 条缺失圈分布在 2 个维度，另 2 条完整
    mk_lap(&pool, u, 0, 200150, 70000, None, None, None, None, None).await;
    mk_lap(&pool, u, 0, 200150, 71000, None, None, None, None, None).await;
    mk_lap(&pool, u, 1, 200150, 80000, None, None, None, None, None).await;
    mk_lap(&pool, u, 1, 200150, 81000, None, None, None, None, None).await;
    mk_lap(&pool, u, 2, 200150, 90000, Some("dual"), Some("custom"), Some("weak"),
           Some("custom"), Some("strong")).await;
    mk_lap(&pool, u, 3, 200150, 95000, Some("off"), Some("default"), Some("stock"),
           Some("default"), Some("stock")).await;

    // 候选数（与 handler 的 count_missing_candidates 逐字同源）
    let predicted: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*)::bigint FROM laps l WHERE l.user_id = $1 AND {}",
        crate::api::laps::ASSIST_MISSING_SQL
    )).bind(u).fetch_one(&pool).await.unwrap();
    assert_eq!(predicted, 4, "候选数 = 全部缺失圈数（不是获胜圈维度数 2）");

    let actual = hide_missing_sql(&pool, u).await;
    assert_eq!(predicted, actual, "预判数与实际屏蔽数必须相等");

    wipe(&pool).await;
}

/// **快照语义**：屏蔽后新上传的缺失圈照常进榜（用户定案），不能被历史屏蔽挡住。
///
/// 这也是「排除条件必须用 hidden_missing_laps 的 lap_id、不能用配置判据」的
/// 直接体现——拿「配置缺失」当过滤器会把新圈一起挡掉。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn hide_missing_is_a_snapshot_new_missing_laps_still_count() {
    let Some(pool) = setup().await else { return };
    wipe(&pool).await;
    let u = mk_user(&pool, "fly3", 3).await;

    mk_lap(&pool, u, 0, 200150, 70000, None, None, None, None, None).await;
    mk_lap(&pool, u, 0, 200150, 75000, Some("dual"), Some("custom"), Some("weak"),
           Some("custom"), Some("off")).await;
    hide_missing_sql(&pool, u).await;

    // 屏蔽之后新传一条更快的缺失圈（旧版模块补传的场景）
    mk_lap(&pool, u, 0, 200150, 68000, None, None, None, None, None).await;

    // 它**不是**被屏蔽的（快照里没有它的 lap_id）→ 照常参与竞争
    let best_ms: i32 = sqlx::query_scalar(
        "SELECT lap_ms FROM best_laps WHERE user_id=$1 AND gp_index=0 AND version_code=200150",
    ).bind(u).fetch_one(&pool).await.unwrap();
    assert_eq!(best_ms, 68000, "新上传的缺失圈不在快照里，照常进榜");

    let hidden: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM hidden_missing_laps h JOIN laps l ON l.id=h.lap_id \
         WHERE h.user_id=$1 AND l.lap_ms=68000)",
    ).bind(u).fetch_one(&pool).await.unwrap();
    assert!(!hidden, "新圈不得被自动标记为已屏蔽");

    wipe(&pool).await;
}
