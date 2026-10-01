-- Feed descriptions and change tracking (DESIGN.md §3.1, §4.3).
ALTER TABLE feeds ADD COLUMN description TEXT;
-- The last submission that changed anything, as JSON (`LastChange`).
ALTER TABLE feeds ADD COLUMN last_change_json TEXT;

-- Existing items predate tracking: unknown times (0) and already seen.
ALTER TABLE feed_items ADD COLUMN added_at_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE feed_items ADD COLUMN changed_at_ms INTEGER NOT NULL DEFAULT 0;
-- NULL: never seen (new).
ALTER TABLE feed_items ADD COLUMN seen_at_ms INTEGER;
UPDATE feed_items SET seen_at_ms = 0;
