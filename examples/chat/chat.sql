-- chat's database.
CREATE TABLE IF NOT EXISTS messages (
    id   INTEGER PRIMARY KEY,
    room TEXT    NOT NULL,
    name TEXT    NOT NULL,
    text TEXT    NOT NULL,
    at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_room ON messages (room, id);
