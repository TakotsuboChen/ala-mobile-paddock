-- 0004：注册流程 v2（2026-09-01 定案：bot 校验即建号）。
-- 旧流程：申请只带 username → 模块 verify 时传密码+校验码建号。
-- 新流程：申请时一并设密 + 发车手 ID → bot 群校验成功即建号 → 用户回模块直接登录。
-- 用户名可改（管理端设置页 v9 已上线）：pending 锁存 username 语义不变。
ALTER TABLE pending_regs ADD COLUMN pass_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE pending_regs ADD COLUMN reg_seq BIGINT NOT NULL DEFAULT 0;