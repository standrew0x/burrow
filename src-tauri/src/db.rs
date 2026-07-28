//! SQLite schema, pragmas, and migrations.

use rusqlite::Connection;

use crate::error::Result;

/// Ordered migration list. Append only -- never edit a shipped entry, or
/// libraries already at that version will diverge from fresh ones.
///
/// Index + 1 is the `user_version` the migration brings the database to.
const MIGRATIONS: &[&str] = &[
    // --- v1: assets, tags, palette ---
    r#"
    CREATE TABLE assets (
        id            INTEGER PRIMARY KEY,
        hash          TEXT    NOT NULL UNIQUE,
        ext           TEXT    NOT NULL,
        mime          TEXT    NOT NULL,
        width         INTEGER NOT NULL,
        height        INTEGER NOT NULL,
        bytes         INTEGER NOT NULL,
        original_name TEXT,
        source_url    TEXT,
        imported_at   INTEGER NOT NULL
    );
    CREATE INDEX assets_imported_at ON assets(imported_at DESC);

    CREATE TABLE tags (
        id   INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );

    CREATE TABLE asset_tags (
        asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        tag_id   INTEGER NOT NULL REFERENCES tags(id)   ON DELETE CASCADE,
        -- 'manual' or 'auto'; auto tags get replaced wholesale on re-index,
        -- manual ones must survive it.
        source   TEXT NOT NULL DEFAULT 'manual',
        PRIMARY KEY (asset_id, tag_id)
    );
    CREATE INDEX asset_tags_tag ON asset_tags(tag_id);

    CREATE TABLE swatches (
        asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        ordinal  INTEGER NOT NULL,
        weight   REAL    NOT NULL,
        l        REAL    NOT NULL,
        a        REAL    NOT NULL,
        b        REAL    NOT NULL,
        hex      TEXT    NOT NULL,
        PRIMARY KEY (asset_id, ordinal)
    );
    -- Colour search prefilters on a lightness band before computing exact
    -- distance in Rust, so l wants an index; a and b are checked in the same
    -- pass and do not.
    CREATE INDEX swatches_l ON swatches(l);
    "#,
];

/// Opens a connection, applies pragmas, and migrates to the current schema.
pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// In-memory database, for tests.
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL: readers do not block the writer, so browsing the grid stays
    // responsive while an import is running.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL rather than FULL: a power-loss mid-import can lose the last
    // transaction, but every blob is content-addressed and re-importable, so
    // the cost of that is a re-scan rather than corruption.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Off by default in SQLite, and the ON DELETE CASCADEs above are load-bearing.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let current = usize::try_from(current).unwrap_or(0);

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let version = i + 1;
        conn.execute_batch(&format!(
            "BEGIN;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;"
        ))?;
    }
    Ok(())
}

pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_to_latest_version() {
        let conn = open_in_memory().expect("open");
        assert_eq!(
            schema_version(&conn).unwrap(),
            MIGRATIONS.len() as i64,
            "user_version should equal the migration count"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = open_in_memory().expect("open");
        // Running again must be a no-op, not a "table already exists" error.
        migrate(&conn).expect("second migrate");
        migrate(&conn).expect("third migrate");
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn expected_tables_exist() {
        let conn = open_in_memory().expect("open");
        for table in ["assets", "tags", "asset_tags", "swatches"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
    }

    #[test]
    fn hash_uniqueness_is_enforced() {
        let conn = open_in_memory().expect("open");
        let insert = "INSERT INTO assets (hash, ext, mime, width, height, bytes, imported_at)
                      VALUES (?1, 'png', 'image/png', 1, 1, 1, 0)";
        conn.execute(insert, ["deadbeef"]).expect("first insert");
        // The dedupe guarantee is a DB constraint, not just application logic.
        assert!(conn.execute(insert, ["deadbeef"]).is_err());
    }

    #[test]
    fn deleting_an_asset_cascades_to_swatches() {
        let conn = open_in_memory().expect("open");
        conn.execute(
            "INSERT INTO assets (id, hash, ext, mime, width, height, bytes, imported_at)
             VALUES (1, 'abc', 'png', 'image/png', 4, 4, 16, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO swatches (asset_id, ordinal, weight, l, a, b, hex)
             VALUES (1, 0, 1.0, 0.5, 0.0, 0.0, '#808080')",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM assets WHERE id = 1", []).unwrap();

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM swatches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "foreign_keys pragma is not taking effect");
    }
}
