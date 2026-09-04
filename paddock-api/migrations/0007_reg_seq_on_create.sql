-- 0007: 车手 ID 发放时机后移（2026-09-04 v3）
-- 申请时不发号（pending 作废会烧号），改为 bot 校验建号事务内 nextval 发放。
-- pending_regs 不再需要 reg_seq 列。
ALTER TABLE pending_regs DROP COLUMN IF EXISTS reg_seq;
