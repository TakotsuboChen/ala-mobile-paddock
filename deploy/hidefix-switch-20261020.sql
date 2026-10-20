-- 2026-10-20 屏蔽语义升级（快照 → 持续生效的开关）的存量数据修复。
--
-- ## 背景
--
-- 旧实现：屏蔽时把缺失圈的 lap_id 记进 hidden_missing_laps，判据 = 「lap_id 在表里」。
-- 结果：管理端补全配置后，红标不消失、该圈也回不了榜——因为判据与配置完全解耦。
--
-- 新实现：判据 = `users.hidden_missing_at IS NOT NULL` AND 该圈五维配置缺失
-- （`laps::AUTO_HIDDEN_SQL`）。补全配置即自动解除，新传缺失圈自动被屏蔽。
-- `hidden_missing_laps` 降级为纯审计记录，不参与任何判据。
--
-- ## 本脚本做什么
--
-- 对**所有**已开启屏蔽的用户，重算其全部受影响的 (赛道, 版本) 维度：
--   1. 删掉指向「配置已补全」圈的审计行（它们在新语义下已解除屏蔽）；
--   2. 按新谓词回放 best_laps；
--   3. 按新谓词重算 records。
--
-- ⚠️ 为什么必须重算而不能只清审计行：旧逻辑重算时把那些圈**排除**过，
--    `best_laps` 已按「排除后」的结果写过。光清标记会留下「不带红标但也不在
--    榜上」的矛盾状态——比修复前更难解释。
--
-- 幂等：可重复执行。回放口径与生产 handler 逐字一致（lap_ms ASC, created_at ASC）。
--
-- 用法（VPS）：
--   sudo docker compose -f docker-compose.vps.yml exec -T postgres \
--     psql -U paddock -d paddock -v ON_ERROR_STOP=1 < hidefix-switch-20261020.sql

BEGIN;

-- 1) 清理已失效的审计行：配置已完整的圈在新语义下不再被屏蔽。
DELETE FROM hidden_missing_laps h
USING laps l
WHERE l.id = h.lap_id
  AND l.pedal_mode      IS NOT NULL
  AND l.tc_mode         IS NOT NULL
  AND l.tc_strength     IS NOT NULL
  AND l.abs_mode        IS NOT NULL
  AND l.abs_strength    IS NOT NULL;

-- 2) 待重算维度 = 每个已开启屏蔽的用户 × 其全部 (赛道, 版本)。
--    不做「只算受影响的」优化：存量数据量小，全量重算更不容易漏。
CREATE TEMP TABLE _recalc_dims ON COMMIT DROP AS
SELECT DISTINCT l.user_id, l.gp_index, l.version_code
FROM laps l
JOIN users u ON u.id = l.user_id
WHERE u.hidden_missing_at IS NOT NULL;

-- 3) 重算 best_laps：先删后回放（某维度可能一条可用圈都不剩，必须让行消失）。
DELETE FROM best_laps b
USING _recalc_dims d
WHERE b.user_id = d.user_id
  AND b.gp_index = d.gp_index
  AND b.version_code = d.version_code;

INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms,
                       pedal_linear, tc_off, abs_off, abs_low)
SELECT DISTINCT ON (l.user_id, l.gp_index, l.version_code)
       l.user_id, l.gp_index, l.version_code, l.lap_ms,
       -- 派生位与 score.rs::AssistFlags::from 逐字对应：
       --   pedal_linear = pedal_mode ∈ {single, dual}
       --   tc_off       = tc_mode='custom'  AND tc_strength='off'
       --   abs_off      = abs_mode='custom' AND abs_strength='off'
       --   abs_low      = abs_mode='custom' AND abs_strength='weak'
       -- ⚠️ 一律 COALESCE(..., false)：列值为 NULL 时 IN/= 的结果是 NULL 不是
       --    false，写进 NOT NULL 列会整事务回滚（2026-10-20 踩过）。
       COALESCE(l.pedal_mode IN ('single','dual'), false),
       COALESCE(l.tc_mode = 'custom' AND l.tc_strength = 'off', false),
       COALESCE(l.abs_mode = 'custom' AND l.abs_strength = 'off', false),
       COALESCE(l.abs_mode = 'custom' AND l.abs_strength = 'weak', false)
FROM laps l
JOIN users u ON u.id = l.user_id
JOIN _recalc_dims d
  ON d.user_id = l.user_id AND d.gp_index = l.gp_index AND d.version_code = l.version_code
-- 新谓词：开关打开且该圈缺失 → 排除
WHERE NOT (u.hidden_missing_at IS NOT NULL AND (
             l.pedal_mode IS NULL OR l.tc_mode IS NULL OR l.tc_strength IS NULL
          OR l.abs_mode IS NULL OR l.abs_strength IS NULL))
ORDER BY l.user_id, l.gp_index, l.version_code, l.lap_ms ASC, l.created_at ASC;

-- 4) 重算 records（alltime + 每个受影响的版本）。
--    范围 = 所有出现过圈的赛道（跨用户，故用相关子查询取圈主的开关状态）。
CREATE TEMP TABLE _rec_tracks ON COMMIT DROP AS
SELECT DISTINCT gp_index FROM _recalc_dims;

-- 4a. alltime
DELETE FROM records r
USING _rec_tracks t
WHERE r.gp_index = t.gp_index AND r.kind = 'alltime';

INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id, updated_at)
SELECT DISTINCT ON (l.gp_index)
       l.gp_index, 'alltime', 0, l.lap_ms, l.user_id, now()
FROM laps l
JOIN _rec_tracks t ON t.gp_index = l.gp_index
JOIN users u ON u.id = l.user_id
WHERE NOT (u.hidden_missing_at IS NOT NULL AND (
             l.pedal_mode IS NULL OR l.tc_mode IS NULL OR l.tc_strength IS NULL
          OR l.abs_mode IS NULL OR l.abs_strength IS NULL))
ORDER BY l.gp_index, l.lap_ms ASC, l.created_at ASC;

-- 4b. version（该赛道下有圈的每个版本）
DELETE FROM records r
USING _rec_tracks t
WHERE r.gp_index = t.gp_index AND r.kind = 'version';

INSERT INTO records (gp_index, kind, version_code, lap_ms, user_id, updated_at)
SELECT DISTINCT ON (l.gp_index, l.version_code)
       l.gp_index, 'version', l.version_code, l.lap_ms, l.user_id, now()
FROM laps l
JOIN _rec_tracks t ON t.gp_index = l.gp_index
JOIN users u ON u.id = l.user_id
WHERE NOT (u.hidden_missing_at IS NOT NULL AND (
             l.pedal_mode IS NULL OR l.tc_mode IS NULL OR l.tc_strength IS NULL
          OR l.abs_mode IS NULL OR l.abs_strength IS NULL))
ORDER BY l.gp_index, l.version_code, l.lap_ms ASC, l.created_at ASC;

COMMIT;
