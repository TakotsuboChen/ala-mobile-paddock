-- Telegram bot 身份列。
--
-- 与 QQ 的 member_openid 并列，语义**不同**：
--   member_openid  —— QQ 平台按群隔离（同一个人在不同群是不同的 openid）
--   telegram_id    —— Telegram 全局唯一（同一个人在所有群/私聊都是同一个 id）
-- 因此同一行上两列互不重叠：QQ 注册的用户只有 member_openid，TG 注册的只有 telegram_id。
-- 不做跨平台账号合并（用户 2026-11-03 定案：两平台各自独立账号体系）。
--
-- ⚠️ BIGINT 不是 INTEGER：Telegram user id 最多 52 位有效位，会超 32 位。
ALTER TABLE users ADD COLUMN telegram_id BIGINT UNIQUE;

-- 反查索引（UNIQUE 已隐式建索引，此条是部分索引，只为让 NULL 行不占索引空间）。
CREATE INDEX idx_users_telegram_id ON users(telegram_id) WHERE telegram_id IS NOT NULL;
