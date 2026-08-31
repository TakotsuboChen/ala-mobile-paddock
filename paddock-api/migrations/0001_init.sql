-- users: 车手账号。身份三元组 = username + argon2id 密码 + member_openid（QQ 群内身份）。
CREATE TABLE users (
    id            UUID PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    pass_hash     TEXT NOT NULL,
    member_openid TEXT UNIQUE,          -- bot 校验绑定；bot 上线前由管理端代绑，可 NULL
    avatar_key    TEXT,                 -- Garage 对象 key，NULL = 未设置头像
    reg_seq       BIGINT NOT NULL,      -- 第 x 位车手（注册顺序）
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- sessions: 90 天滑动 token；存哈希不存原文。
CREATE TABLE sessions (
    token_hash  TEXT PRIMARY KEY,       -- SHA-256(token)
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);

-- pending_regs: 注册会话。reg_code 绑定会话一次性使用，30 分钟时效。
-- 流程：模块申请码 → 用户发群里 → bot 用码定位会话并绑定 member_openid → 模块 verify 建号。
-- username 在申请时即锁存：同名在途会话存在时拒绝再申请（防同名并发注册）。
CREATE TABLE pending_regs (
    reg_code      TEXT PRIMARY KEY,
    username      TEXT NOT NULL,
    member_openid TEXT,                 -- bot 校验成功后写入
    status        TEXT NOT NULL DEFAULT 'pending',  -- pending|verified
    expires_at    TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_pending_regs_openid ON pending_regs(member_openid);
CREATE INDEX idx_pending_regs_username ON pending_regs(username);

-- laps: 全量留档（防伪=全放行的配套：事后审计/删除重算的依据）。
CREATE TABLE laps (
    id           UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    gp_index     SMALLINT NOT NULL,     -- 0..15 = buildIndex-2
    version_code INTEGER NOT NULL,      -- 游戏 6 位 versionCode（200146=8.0.4）
    lap_ms       INTEGER NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_laps_user_track ON laps(user_id, gp_index, version_code);

-- best_laps: 榜单数据源。每人每赛道每版本只留最佳；服务端比对后 upsert。
CREATE TABLE best_laps (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    gp_index     SMALLINT NOT NULL,
    version_code INTEGER NOT NULL,
    lap_ms       INTEGER NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, gp_index, version_code)
);
CREATE INDEX idx_best_laps_rank ON best_laps(gp_index, version_code, lap_ms);

-- records: 全服最佳（历史 alltime / 版本 version），Toast 判定与登顶提示依据。
-- ⚠️ 复合主键列隐式 NOT NULL，而 alltime 行 version_code 必须可 NULL——
-- 因此主键用 version_code 的显式 NULL 处理：alltime 行 version_code 统一存 0 代替 NULL，
-- kind 列才是真正的区分键（alltime 行读时忽略 version_code）。
CREATE TABLE records (
    gp_index     SMALLINT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('alltime','version')),
    version_code INTEGER NOT NULL DEFAULT 0,  -- alltime 行恒 0（占位），version 行=游戏 versionCode
    lap_ms       INTEGER NOT NULL,
    user_id      UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (gp_index, kind, version_code)
);