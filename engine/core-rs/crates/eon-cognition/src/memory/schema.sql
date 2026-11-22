CREATE TABLE IF NOT EXISTS memory_v1_config(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    model TEXT NOT NULL,
    dims INTEGER NOT NULL,
    capacity INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS memory_v1_entities(
    id TEXT PRIMARY KEY REFERENCES memory_v1_notes(id) ON DELETE CASCADE,
    kind TEXT NOT NULL
) STRICT;

-- Check graph closure at commit, not insertion: a CRDT snapshot can yield
-- edges before either endpoint. Multiple sources may attest the same triple.
CREATE TABLE IF NOT EXISTS memory_v1_relations(
    id TEXT PRIMARY KEY REFERENCES memory_v1_notes(id) ON DELETE CASCADE,
    subject TEXT NOT NULL REFERENCES memory_v1_entities(id) DEFERRABLE INITIALLY DEFERRED,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL REFERENCES memory_v1_entities(id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE INDEX IF NOT EXISTS memory_v1_subject ON memory_v1_relations(subject);
CREATE INDEX IF NOT EXISTS memory_v1_object ON memory_v1_relations(object);

CREATE TABLE IF NOT EXISTS memory_v1_notes(
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    body TEXT NOT NULL,
    vector BLOB
) STRICT;

CREATE VIRTUAL TABLE IF NOT EXISTS memory_v1_fts USING fts5(
    body, content='memory_v1_notes', content_rowid='rowid', tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS memory_v1_insert AFTER INSERT ON memory_v1_notes BEGIN
    INSERT INTO memory_v1_fts(rowid,body) VALUES(new.rowid,new.body);
END;
CREATE TRIGGER IF NOT EXISTS memory_v1_delete AFTER DELETE ON memory_v1_notes BEGIN
    INSERT INTO memory_v1_fts(memory_v1_fts,rowid,body) VALUES('delete',old.rowid,old.body);
END;
CREATE TRIGGER IF NOT EXISTS memory_v1_update AFTER UPDATE ON memory_v1_notes BEGIN
    INSERT INTO memory_v1_fts(memory_v1_fts,rowid,body) VALUES('delete',old.rowid,old.body);
    INSERT INTO memory_v1_fts(rowid,body) VALUES(new.rowid,new.body);
END;
