-- 统一业务事件日志（管理端"日志"页数据源）。
-- 定案（参照业界审计日志实践）：
--   * 追加写（append-only）：只 INSERT，业务代码不 UPDATE/DELETE；
--   * 审计与应用日志分离：容器 stdout 的 tracing 保持不动，本表只收"谁在何时做了什么"；
--   * 脱敏：绝不写密码/token/secret 原文（actor 只到用户名/管理员名粒度）；
--   * 保留策略：90 天，app 启动时清理（applog::purge_expired）。
-- 旧 admin_audit（只有管理端敏感操作留痕、从不展示）在此统一并弃用：历史行
-- 回填为 category='admin' 事件后 DROP。
CREATE TABLE app_logs (
    id         BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    level      TEXT NOT NULL DEFAULT 'info',   -- info | warn | error
    category   TEXT NOT NULL,                  -- admin | auth | lap | bot
    event      TEXT NOT NULL,                  -- 机器可读事件名（如 user_register / lap_upload）
    actor      TEXT NOT NULL DEFAULT '',       -- 操作者：用户名 / 'admin' / 'bot' / ''（匿名）
    message    TEXT NOT NULL,                  -- 人类可读一句话摘要（管理端直接展示）
    detail     JSONB NOT NULL DEFAULT '{}'     -- 结构化补充（id/赛道/圈时等），UI 折叠展示
);
CREATE INDEX idx_app_logs_created ON app_logs (created_at DESC);
CREATE INDEX idx_app_logs_cat ON app_logs (category, created_at DESC);

-- 回填历史审计：admin_audit 行 → app_logs(category='admin')，message 由 action 翻译。
INSERT INTO app_logs (created_at, level, category, event, actor, message, detail)
SELECT created_at, 'info', 'admin', action, admin_user,
       CASE action
           WHEN 'rename_user'          THEN '重命名用户'
           WHEN 'reset_password'       THEN '重置用户密码'
           WHEN 'delete_user'          THEN '删除用户'
           WHEN 'delete_lap'           THEN '删除成绩'
           WHEN 'edit_lap'             THEN '修改成绩圈时'
           WHEN 'add_lap'              THEN '补录成绩'
           WHEN 'change_admin_password'THEN '修改管理员密码'
           WHEN 'change_admin_username'THEN '修改管理员用户名'
           WHEN 'save_bot_config'      THEN '保存 bot 配置'
           WHEN 'save_bot_rules'       THEN '保存消息规则'
           WHEN 'save_broadcast_groups'THEN '保存播报目标群'
           WHEN 'save_brand'           THEN '保存品牌配置'
           ELSE action
       END,
       detail
FROM admin_audit
ORDER BY created_at ASC;

DROP TABLE admin_audit;
