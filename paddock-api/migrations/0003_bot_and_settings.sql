-- S4：bot 配置落库 + 密码重置一次性码 + 车手 ID 正式化（用户 2026-08-31 定案：ID=注册顺序，从 1 起）。

-- 通用键值配置：bot AppID/Secret、CAMDA 群 group_openid 等经管理端设置页维护。
CREATE TABLE configs (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 密码重置一次性码：群内 "重置密码 用户名" → bot 私发码（本群内回复）→ 模块提交新密码。
CREATE TABLE reset_codes (
    code       TEXT PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL
);

-- 车手 ID（reg_seq）正式化：序列发放（并发注册不重号）+ 唯一约束。
CREATE SEQUENCE user_reg_seq AS BIGINT START 1 OWNED BY users.reg_seq;
CREATE UNIQUE INDEX idx_users_reg_seq ON users(reg_seq);