-- 0008: 头像版本号——版本化 URL 供客户端磁盘缓存失效（2026-09-06）
-- VPS 流量出口告急：客户端每次进页都全量下载一轮头像。改为 avatar_url 带 ?v=，
-- 上传头像时置为 epoch millis → URL 变化即天然缓存失效，客户端无需 304 协商。
-- 老数据 COALESCE 0 兜底（URL 确定性，避免 ?v= 空串脏 URL）。
ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar_version BIGINT NOT NULL DEFAULT 0;
-- 已有头像的老用户：版本号回填为 1（首次 URL 与 ?v=0 区分开，行为等价于"有头像"）
UPDATE users SET avatar_version = 1 WHERE avatar_key IS NOT NULL AND avatar_version = 0;
