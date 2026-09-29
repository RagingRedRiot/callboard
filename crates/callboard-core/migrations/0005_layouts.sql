-- No target foreign keys: deleted targets remain layout placeholders.
CREATE TABLE layouts (
    name TEXT PRIMARY KEY NOT NULL,
    tree_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
