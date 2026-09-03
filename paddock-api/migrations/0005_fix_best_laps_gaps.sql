-- 0005：修复 best_laps 缺行——管理端补录/编辑成绩场景下 recalc_dims 原 UPDATE-only
-- 写法对不存在的行静默 0 行，导致 laps 有圈但 best_laps（进而积分榜/赛道榜）无成绩。
-- 一次性回填：凡 laps 有圈而 best_laps 缺行的 (user, gp, version) 维度全部补插。
INSERT INTO best_laps (user_id, gp_index, version_code, lap_ms, updated_at)
SELECT l.user_id, l.gp_index, l.version_code, min(l.lap_ms), now()
FROM laps l
GROUP BY l.user_id, l.gp_index, l.version_code
ON CONFLICT (user_id, gp_index, version_code) DO NOTHING;