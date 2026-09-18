-- 0009：圈辅助配置 + 零辅助标识（积分加成与金标的数据面）。
--
-- 背景：积分从"纯圈速名次"扩展为"名次分 + 辅助加成"（最多 +50%），
-- 并新增"零辅助最快圈"金标。数据链：
--   laps（全量留档）        ← 存原始配置枚举（模块上报 / 管理端补录）
--   best_laps（榜单数据源） ← 存派生布尔位（由获胜那条圈的配置派生）
--
-- ⚠️ NULL = 缺失（旧记录 / 旧版模块上传 / 管理端选了"缺失"），缺失不加分。
--    四个派生位在 best_laps 上 NOT NULL DEFAULT FALSE —— 旧行天然"不加分"，
--    与"缺失"语义等价，故无需回填（用户定案：旧记录保持缺失）。
--
-- 派生规则（与 paddock-api/src/score.rs 逐字对应，改必须两边同改）：
--   pedal_linear = pedal_mode IN ('single','dual')            -- 'off' 原生按键不算
--   tc_off       = tc_mode='custom' AND tc_strength='off'
--   abs_off      = abs_mode='custom' AND abs_strength='off'
--   abs_low      = abs_mode='custom' AND abs_strength='weak'
--   ️ 未开自定义（mode='default'）= 游戏默认 = 绝不算"关"——tc_strength 的
--      记忆值在 default 模式下不生效，读它会把"从未改过 TC"的用户判成关 TC。

ALTER TABLE laps ADD COLUMN pedal_mode TEXT;    -- off | single | dual
ALTER TABLE laps ADD COLUMN tc_mode TEXT;       -- default | custom
ALTER TABLE laps ADD COLUMN tc_strength TEXT;   -- off | weak | medium | strong | stock
ALTER TABLE laps ADD COLUMN abs_mode TEXT;      -- default | custom
ALTER TABLE laps ADD COLUMN abs_strength TEXT;  -- off | weak | medium | strong | stock

ALTER TABLE best_laps ADD COLUMN pedal_linear BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE best_laps ADD COLUMN tc_off BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE best_laps ADD COLUMN abs_off BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE best_laps ADD COLUMN abs_low BOOLEAN NOT NULL DEFAULT FALSE;

-- 回填现有 best_laps 的四个派生位：全部保持 FALSE（缺失）——
-- 用户定案「旧记录保持缺失」，且现有 laps 无配置可派生。此段为显式声明
-- 意图，不改任何行（DEFAULT FALSE 已覆盖）；留作审计线索。
-- 若将来要回填历史，正确做法是从 laps 取每条 best_laps 的获胜圈配置再派生。