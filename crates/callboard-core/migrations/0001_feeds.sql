CREATE TABLE feeds (
    name TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    source_url TEXT,
    stale_after TEXT,
    last_submitted_at_ms INTEGER NOT NULL,
    error_message TEXT,
    error_at_ms INTEGER,
    CHECK ((error_message IS NULL) = (error_at_ms IS NULL))
) STRICT;

CREATE TABLE feed_items (
    feed TEXT NOT NULL REFERENCES feeds(name) ON DELETE CASCADE,
    key TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    content_json TEXT NOT NULL,
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32),
    PRIMARY KEY (feed, key)
) STRICT;

CREATE INDEX feed_items_order ON feed_items(feed, position);
