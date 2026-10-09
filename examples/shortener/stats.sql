-- stats' database.
CREATE TABLE IF NOT EXISTS clicks (
    event_id TEXT    PRIMARY KEY,  -- makes redelivered queue messages no-ops
    code     TEXT    NOT NULL,
    at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS clicks_at ON clicks (at);
CREATE TABLE IF NOT EXISTS totals (
    code       TEXT    PRIMARY KEY,
    clicks     INTEGER NOT NULL,
    last_click INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS top_links (
    rank       INTEGER PRIMARY KEY,
    code       TEXT    NOT NULL,
    clicks     INTEGER NOT NULL,
    last_click INTEGER NOT NULL
);
