-- Drop verbose per-execution columns from tradelog; keep minimal core fields
ALTER TABLE tradelog
    DROP COLUMN IF EXISTS ord_status,
    DROP COLUMN IF EXISTS side,
    DROP COLUMN IF EXISTS is_taker,
    DROP COLUMN IF EXISTS last_px,
    DROP COLUMN IF EXISTS last_qty,
    DROP COLUMN IF EXISTS leaves_qty,
    DROP COLUMN IF EXISTS ord_px,
    DROP COLUMN IF EXISTS ord_qty,
    DROP COLUMN IF EXISTS fee,
    DROP COLUMN IF EXISTS response_status;


