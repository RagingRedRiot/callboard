-- Colors for todos and boards, set from the GUI (DESIGN.md §5).
ALTER TABLE todos ADD COLUMN color TEXT;
ALTER TABLE boards ADD COLUMN color TEXT;
