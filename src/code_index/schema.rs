// Bump when index CONTENT semantics change, not only the DDL: stored indexes
// are reused while the version matches, so new extraction rules (e.g. item
// 505's const/static tags) would otherwise stay invisible until files change.
pub(super) const SCHEMA_VERSION: &str = "10";

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS source_code_meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS source_code_files(
    path TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS source_code_symbols(
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    signature TEXT NOT NULL,
    is_test INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS source_code_symbols_name ON source_code_symbols(name);
CREATE INDEX IF NOT EXISTS source_code_symbols_name_nocase
    ON source_code_symbols(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS source_code_symbols_name_path
    ON source_code_symbols(name,path,line);
CREATE INDEX IF NOT EXISTS source_code_symbols_path ON source_code_symbols(path,line);
CREATE TABLE IF NOT EXISTS source_code_edges(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    src_id TEXT,
    src_path TEXT NOT NULL,
    dst_id TEXT,
    dst_raw TEXT NOT NULL,
    line INTEGER NOT NULL,
    kind TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS source_code_edges_source ON source_code_edges(src_id,src_path);
CREATE INDEX IF NOT EXISTS source_code_edges_src_path ON source_code_edges(src_path);
CREATE INDEX IF NOT EXISTS source_code_edges_target ON source_code_edges(dst_id,dst_raw);
CREATE INDEX IF NOT EXISTS source_code_edges_raw ON source_code_edges(dst_raw);
CREATE INDEX IF NOT EXISTS source_code_edges_raw_nocase
    ON source_code_edges(dst_raw COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS source_code_edge_staging(
    src_id TEXT,
    src_path TEXT NOT NULL,
    dst_raw TEXT NOT NULL,
    line INTEGER NOT NULL,
    kind TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS source_code_edge_staging_path ON source_code_edge_staging(src_path);
CREATE INDEX IF NOT EXISTS source_code_edge_staging_raw ON source_code_edge_staging(dst_raw);
CREATE TABLE IF NOT EXISTS source_code_chunks(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_key TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_test INTEGER NOT NULL,
    symbols TEXT NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS source_code_chunks_path ON source_code_chunks(path,start_line);
CREATE VIRTUAL TABLE IF NOT EXISTS source_code_fts USING fts5(
    symbols,
    body,
    content='source_code_chunks',
    content_rowid='id',
    tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS source_code_chunks_ai AFTER INSERT ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(rowid,symbols,body) VALUES(new.id,new.symbols,new.body);
END;
CREATE TRIGGER IF NOT EXISTS source_code_chunks_ad AFTER DELETE ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(source_code_fts,rowid,symbols,body)
    VALUES('delete',old.id,old.symbols,old.body);
END;
CREATE TRIGGER IF NOT EXISTS source_code_chunks_au AFTER UPDATE ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(source_code_fts,rowid,symbols,body)
    VALUES('delete',old.id,old.symbols,old.body);
    INSERT INTO source_code_fts(rowid,symbols,body) VALUES(new.id,new.symbols,new.body);
END;
"#;

pub(super) const DROP_FTS_TRIGGERS: &str = r#"
DROP TRIGGER IF EXISTS source_code_chunks_ai;
DROP TRIGGER IF EXISTS source_code_chunks_ad;
DROP TRIGGER IF EXISTS source_code_chunks_au;
"#;

pub(super) const CREATE_FTS_TRIGGERS: &str = r#"
CREATE TRIGGER IF NOT EXISTS source_code_chunks_ai AFTER INSERT ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(rowid,symbols,body) VALUES(new.id,new.symbols,new.body);
END;
CREATE TRIGGER IF NOT EXISTS source_code_chunks_ad AFTER DELETE ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(source_code_fts,rowid,symbols,body)
    VALUES('delete',old.id,old.symbols,old.body);
END;
CREATE TRIGGER IF NOT EXISTS source_code_chunks_au AFTER UPDATE ON source_code_chunks BEGIN
    INSERT INTO source_code_fts(source_code_fts,rowid,symbols,body)
    VALUES('delete',old.id,old.symbols,old.body);
    INSERT INTO source_code_fts(rowid,symbols,body) VALUES(new.id,new.symbols,new.body);
END;
"#;

pub(super) const STORAGE_STAGING_SCHEMA: &str = r#"
CREATE TEMP TABLE IF NOT EXISTS source_code_store_delete_paths(
    value TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS source_code_store_metadata(
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS source_code_store_files(
    path TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS source_code_store_symbols(
    id TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    signature TEXT NOT NULL,
    is_test INTEGER NOT NULL
);
CREATE TEMP TABLE IF NOT EXISTS source_code_store_edges(
    src_id TEXT,
    src_path TEXT NOT NULL,
    dst_raw TEXT NOT NULL,
    line INTEGER NOT NULL,
    kind TEXT NOT NULL
);
CREATE TEMP TABLE IF NOT EXISTS source_code_store_chunks(
    chunk_key TEXT NOT NULL,
    path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_test INTEGER NOT NULL,
    symbols TEXT NOT NULL,
    body TEXT NOT NULL
);
DELETE FROM source_code_store_delete_paths;
DELETE FROM source_code_store_metadata;
DELETE FROM source_code_store_files;
DELETE FROM source_code_store_symbols;
DELETE FROM source_code_store_edges;
DELETE FROM source_code_store_chunks;
"#;

pub(super) const APPLY_STORAGE_STAGING: &str = r#"
UPDATE source_code_files
SET size=(SELECT metadata.size FROM source_code_store_metadata AS metadata
          WHERE metadata.path=source_code_files.path),
    modified_ns=(SELECT metadata.modified_ns FROM source_code_store_metadata AS metadata
                 WHERE metadata.path=source_code_files.path)
WHERE path IN (SELECT path FROM source_code_store_metadata);
DELETE FROM source_code_chunks
WHERE path IN (SELECT value FROM source_code_store_delete_paths);
DELETE FROM source_code_edges
WHERE src_path IN (SELECT value FROM source_code_store_delete_paths);
DELETE FROM source_code_edge_staging
WHERE src_path IN (SELECT value FROM source_code_store_delete_paths);
DELETE FROM source_code_symbols
WHERE path IN (SELECT value FROM source_code_store_delete_paths);
DELETE FROM source_code_files
WHERE path IN (SELECT value FROM source_code_store_delete_paths);
INSERT INTO source_code_files(path,language,content_hash,size,modified_ns)
SELECT path,language,content_hash,size,modified_ns FROM source_code_store_files;
INSERT INTO source_code_symbols(id,path,name,kind,line,signature,is_test)
SELECT id,path,name,kind,line,signature,is_test FROM source_code_store_symbols;
INSERT INTO source_code_edge_staging(src_id,src_path,dst_raw,line,kind)
SELECT src_id,src_path,dst_raw,line,kind FROM source_code_store_edges;
INSERT INTO source_code_chunks(chunk_key,path,start_line,end_line,is_test,symbols,body)
SELECT chunk_key,path,start_line,end_line,is_test,symbols,body FROM source_code_store_chunks;
"#;
