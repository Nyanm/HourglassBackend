-- enable WAL mode
PRAGMA journal_mode=WAL;
-- create table
CREATE TABLE IF NOT EXISTS raw_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms INTEGER NOT NULL,
    event_type INTEGER NOT NULL,
    flag_switch INTEGER NOT NULL,
    pid INTEGER,
    process_name TEXT,
    exe_path TEXT,
    window_title TEXT,
    tab_title TEXT,
    url TEXT);
CREATE TABLE IF NOT EXISTS usage_segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER,
    seg_state INTEGER NOT NULL,
    pid INTEGER,
    process_name TEXT,
    exe_path TEXT,
    window_title TEXT,
    tab_title TEXT,
    url TEXT);
-- create index
CREATE INDEX IF NOT EXISTS index_segment_time ON raw_events (ts_ms);
CREATE INDEX IF NOT EXISTS index_event_time ON usage_segments (start_ms, end_ms);