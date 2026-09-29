CREATE TABLE boards (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE CHECK (length(trim(name)) > 0),
    is_archive INTEGER NOT NULL DEFAULT 0 CHECK (is_archive IN (0, 1)),
    CHECK (is_archive = 0 OR name = '__callboard_archive__')
) STRICT;

-- A private sink preserves items when a board is explicitly deleted with
-- archive confirmation. Ordinary board listings omit this row.
INSERT INTO boards (id, name, is_archive) VALUES (1, '__callboard_archive__', 1);

CREATE TABLE todos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    board_id INTEGER NOT NULL REFERENCES boards(id) ON DELETE RESTRICT,
    title TEXT NOT NULL,
    body TEXT,
    url TEXT,
    done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0, 1)),
    reference_feed TEXT,
    reference_key TEXT,
    position INTEGER NOT NULL CHECK (position >= 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    archived_at_ms INTEGER,
    archived_from_board TEXT,
    CHECK ((reference_feed IS NULL) = (reference_key IS NULL))
) STRICT;

CREATE TABLE notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    board_id INTEGER NOT NULL REFERENCES boards(id) ON DELETE RESTRICT,
    title TEXT,
    body TEXT NOT NULL,
    color TEXT,
    reference_feed TEXT,
    reference_key TEXT,
    position INTEGER NOT NULL CHECK (position >= 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    archived_at_ms INTEGER,
    archived_from_board TEXT,
    CHECK ((reference_feed IS NULL) = (reference_key IS NULL))
) STRICT;

CREATE INDEX todos_board_order ON todos(board_id, archived_at_ms, position, id);
CREATE INDEX notes_board_order ON notes(board_id, archived_at_ms, position, id);
