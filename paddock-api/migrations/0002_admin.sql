-- 管理端账号（单管理员起步：用户名+密码哈希，首次启动时从环境变量播种）
CREATE TABLE admins (
    id         UUID PRIMARY KEY,
    username   TEXT NOT NULL UNIQUE,
    pass_hash  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 管理端会话（cookie，独立于模块 token 体系）
CREATE TABLE admin_sessions (
    token_hash TEXT PRIMARY KEY,
    expires_at TIMESTAMPTZ NOT NULL
);

-- 审计日志：管理端删除成绩等敏感操作留痕
CREATE TABLE admin_audit (
    id         BIGSERIAL PRIMARY KEY,
    admin_user TEXT NOT NULL,
    action     TEXT NOT NULL,          -- delete_user | delete_lap | bind_openid | reset_password ...
    detail     JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);