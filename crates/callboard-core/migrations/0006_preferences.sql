-- One row of per-user preferences. The layout reference follows renames and
-- clears on deletion, so the preference never names a missing layout.
CREATE TABLE preferences (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_layout TEXT REFERENCES layouts(name) ON UPDATE CASCADE ON DELETE SET NULL
) STRICT;

INSERT INTO preferences(id, last_layout) VALUES (1, NULL);
