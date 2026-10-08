-- Watches: user-chosen items that a script reports on (DESIGN.md §4a).
CREATE TABLE watches (
    name TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    source_url TEXT,
    stale_after TEXT,
    -- humantime durations, or 'never'.
    waiting_after TEXT NOT NULL,
    quiet_after TEXT NOT NULL,
    last_reported_at_ms INTEGER NOT NULL,
    error_message TEXT,
    error_at_ms INTEGER,
    CHECK ((error_message IS NULL) = (error_at_ms IS NULL))
) STRICT;

-- AUTOINCREMENT: a removed item's ID never names a later item, so a report
-- for it is ignored rather than applied to the wrong item.
CREATE TABLE watch_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    watch TEXT NOT NULL REFERENCES watches(name) ON DELETE CASCADE,
    url TEXT NOT NULL,
    label TEXT,
    added_at_ms INTEGER NOT NULL,
    -- The last good report (`ItemContent`), and its fingerprint.
    content_json TEXT,
    fingerprint TEXT,
    reported_at_ms INTEGER,
    changed_at_ms INTEGER,
    error_message TEXT,
    error_at_ms INTEGER,
    -- Set while the item needs attention.
    attention_since_ms INTEGER,
    -- Set when an acknowledgement or Keep waiting starts the waiting clock.
    waiting_since_ms INTEGER,
    -- The item is quiet and a report has already listed it as such.
    quiet_reported INTEGER NOT NULL DEFAULT 0 CHECK (quiet_reported IN (0, 1)),
    UNIQUE (watch, url),
    CHECK ((content_json IS NULL) = (fingerprint IS NULL)),
    CHECK ((error_message IS NULL) = (error_at_ms IS NULL))
) STRICT;
