-- Layouts become canvases of cards (DESIGN.md §6.4). callboard is unreleased,
-- so saved split/tab trees are dropped rather than converted.
UPDATE preferences SET last_layout = NULL;
DELETE FROM layouts;
ALTER TABLE layouts RENAME COLUMN tree_json TO layout_json;
