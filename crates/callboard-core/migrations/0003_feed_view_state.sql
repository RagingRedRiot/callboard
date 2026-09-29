ALTER TABLE feeds ADD COLUMN manual_order_enabled INTEGER NOT NULL DEFAULT 0
    CHECK (manual_order_enabled IN (0, 1));

CREATE TABLE feed_item_view_state (
    feed TEXT NOT NULL,
    key TEXT NOT NULL,
    snoozed_until_ms INTEGER,
    wake_on_update INTEGER NOT NULL DEFAULT 0 CHECK (wake_on_update IN (0, 1)),
    PRIMARY KEY (feed, key),
    FOREIGN KEY (feed, key) REFERENCES feed_items(feed, key) ON DELETE CASCADE
) STRICT;

CREATE TABLE feed_manual_order (
    feed TEXT NOT NULL,
    key TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    PRIMARY KEY (feed, key),
    UNIQUE (feed, position),
    FOREIGN KEY (feed, key) REFERENCES feed_items(feed, key) ON DELETE CASCADE
) STRICT;
