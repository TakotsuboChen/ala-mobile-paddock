-- 0011：圈车型 + 车辆调教（2026-10-26）。
--
-- 背景：车手主页的圈速记录改为点击弹窗展示详情，需要车型与调教。
-- 两者此前只在模块 native 日志里（`LAPcar` / `LAPsetup` 行），未进上传通道。
--
-- ⚠️ **车型只存整数，不存展示名**（用户 2026-10-26 定案）：
--    car_id = CarDefinition.carId（0..41），car_era = CarType（0=地效 / 1=主动空力）。
--    展示名「2026 梅赛德斯-AMG W17」由**模块侧** CarCatalog 反查渲染。
--    ⇒ 改车队译名只发模块，历史圈自动跟着变；服务端永远不需要认识车型字。
--    ⚠️ 不要在这里加 team_name / model_name 列——那会与模块表双写必然漂移。
--
-- ⚠️ **调教存的是 UI 显示值本身**（用户 2026-10-26 定案，实机对拍确认）：
--    下压力存 65.0 不是 0.65，外倾角存 -3.0 度不是弧度。服务端**不做任何换算**，
--    只在展示时格式化（整数项取整、角度项两位小数由模块做）。
--
-- ⚠️ **刹车平衡只有一个 brake_bias 列存「前」值**，后值 = 100 − 前。
--    游戏 UI 显示的 `60 / 40` 是派生量——不要加 rear 列，那会出现两个真值源。
--
-- ⚠️ NULL = 缺失（旧记录 / 旧版模块上传 / 该圈未读到）。
--    整组调教用一个语义：13 列**全有或全无**（模块侧 `has_setup` 判整项）。
--    半套调教比缺失更误导（看起来有数据其实不全），模块侧解析时也会拒绝半套。

ALTER TABLE laps ADD COLUMN car_id  SMALLINT;   -- CarDefinition.carId (0..41)
ALTER TABLE laps ADD COLUMN car_era SMALLINT;   -- CarType (0=地效 1=主动空力)

-- 车辆调教 13 项（7 组，其中 6 组有前后两值）
ALTER TABLE laps ADD COLUMN setup_front_wing_aero   REAL;  -- 前翼下压力
ALTER TABLE laps ADD COLUMN setup_rear_wing_aero    REAL;  -- 后翼下压力
ALTER TABLE laps ADD COLUMN setup_front_ride_height REAL;  -- 前底盘高度
ALTER TABLE laps ADD COLUMN setup_rear_ride_height  REAL;  -- 后底盘高度
ALTER TABLE laps ADD COLUMN setup_brake_bias        REAL;  -- 刹车平衡（**前**值，后 = 100 − 前）
ALTER TABLE laps ADD COLUMN setup_front_stiffness   REAL;  -- 前悬架刚度
ALTER TABLE laps ADD COLUMN setup_rear_stiffness    REAL;  -- 后悬架刚度
ALTER TABLE laps ADD COLUMN setup_front_rollbar     REAL;  -- 前防倾杆
ALTER TABLE laps ADD COLUMN setup_rear_rollbar      REAL;  -- 后防倾杆
ALTER TABLE laps ADD COLUMN setup_front_camber      REAL;  -- 前外倾角（度）
ALTER TABLE laps ADD COLUMN setup_rear_camber       REAL;  -- 后外倾角（度）
ALTER TABLE laps ADD COLUMN setup_front_toe         REAL;  -- 前束角（度）
ALTER TABLE laps ADD COLUMN setup_rear_toe          REAL;  -- 后束角（度）
