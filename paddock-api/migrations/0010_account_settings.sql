-- 0010：账号设置（头像/用户名 30 天冷却 + 屏蔽缺失配置记录）
--
-- ## 为什么冷却时间戳放在 users 上而不是单独表
--
-- 三个字段都是「每用户最多一个」的标量，独立表会让每次账号页查询多一次 JOIN，
-- 而账号页是低频操作、读放大无所谓——真正的理由是**写入必须与 username/avatar_key
-- 同事务**：改用户名与记时间戳若分两表，任一失败就会出现「名字改了但冷却没记」
-- 或反之。同表同行天然原子。
--
-- ## 为什么屏蔽按 `lap_id` 记，而不是按 (赛道, 版本) 维度
--
-- 用户定案（2026-10-20）：屏蔽是**快照**——只作用于点击当时已存在的缺失配置记录，
-- 之后旧版模块再上传的缺失圈照常进榜。若按维度记，新圈会被误判成「已屏蔽」：
-- 车手主页的红标会挂到一条从未被屏蔽过的记录上，且管理端触发重算时无法区分
-- 「这条该保持隐藏」与「这条是新来的」。
--
-- 按 lap_id 记则三个语义天然自洽：
--   1. 红标判据 = `lap_id ∈ hidden_missing_laps`，精确到条；
--   2. `recalc_dims` 重算 best_laps / records 时按 lap_id 排除，屏蔽状态在
--      管理端编辑/删除触发重算后依然保持；
--   3. 用户上传新圈（新 lap_id）不受影响，照常走 upsert 进榜。
--
-- `ON DELETE CASCADE`：删圈（管理端）或删用户时屏蔽记录自动清理，
-- 不会留下指向不存在圈的死行。
--
-- `users.hidden_missing_at` 只回答「这个人屏蔽过没有」（决定弹 Toast 还是执行），
-- 不参与任何红标/重算判定。

ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar_updated_at   TIMESTAMPTZ;
ALTER TABLE users ADD COLUMN IF NOT EXISTS username_updated_at TIMESTAMPTZ;
ALTER TABLE users ADD COLUMN IF NOT EXISTS hidden_missing_at   TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS hidden_missing_laps (
    user_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    lap_id    UUID NOT NULL REFERENCES laps(id) ON DELETE CASCADE,
    hidden_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, lap_id)
);

-- 反查「这条圈被屏蔽了没有」是车手主页每行都要做的（30 行/页），
-- 主键前缀是 user_id 用不上，按 lap_id 单列索引。
CREATE INDEX IF NOT EXISTS idx_hidden_missing_laps_lap ON hidden_missing_laps(lap_id);
