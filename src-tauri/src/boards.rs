//! Named groups of references — the Pinterest-board equivalent.
//!
//! A board is membership, not ownership: an asset can sit on any number of
//! boards, and removing it from one leaves the file and every other board
//! untouched. Deleting a board never deletes assets.

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::ingest::AssetRow;
use crate::store::Library;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub item_count: i64,
    /// Thumbnail of the most recently added item, for the board's cover.
    pub cover_thumb_path: Option<String>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Returns the board with this name, creating it if absent.
///
/// Get-or-create rather than erroring on a duplicate: typing the name of an
/// existing board means "put it there", and a UNIQUE-violation dialog would be
/// a worse answer to that than just doing it. Names are compared case-
/// insensitively, so "Title cards" finds "Title Cards".
pub fn create_board(conn: &Connection, name: &str) -> Result<Board> {
    let name = name.trim();
    conn.execute(
        "INSERT INTO boards (name, created_at) VALUES (?1, ?2)
         ON CONFLICT(name) DO NOTHING",
        rusqlite::params![name, now_unix()],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM boards WHERE name = ?1 COLLATE NOCASE",
        [name],
        |r| r.get(0),
    )?;
    board_by_id(conn, id)
}

pub fn rename_board(conn: &Connection, id: i64, name: &str) -> Result<Board> {
    conn.execute(
        "UPDATE boards SET name = ?1 WHERE id = ?2",
        rusqlite::params![name.trim(), id],
    )?;
    board_by_id(conn, id)
}

/// Deletes the board and its membership rows. Assets are untouched -- the
/// cascade runs board -> board_items, never board_items -> assets.
pub fn delete_board(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM boards WHERE id = ?1", [id])?;
    Ok(())
}

const BOARD_SELECT: &str = "
    SELECT b.id, b.name, b.created_at,
           (SELECT count(*) FROM board_items bi WHERE bi.board_id = b.id),
           (SELECT a.hash
              FROM board_items bi
              JOIN assets a ON a.id = bi.asset_id
             WHERE bi.board_id = b.id
             ORDER BY bi.added_at DESC, bi.asset_id DESC
             LIMIT 1)
      FROM boards b";

fn row_to_board(lib: &Library, r: &rusqlite::Row<'_>) -> rusqlite::Result<Board> {
    let cover_hash: Option<String> = r.get(4)?;
    Ok(Board {
        id: r.get(0)?,
        name: r.get(1)?,
        created_at: r.get(2)?,
        item_count: r.get(3)?,
        cover_thumb_path: cover_hash.map(|h| lib.thumb_path(&h).display().to_string()),
    })
}

pub fn list_boards(lib: &Library, conn: &Connection) -> Result<Vec<Board>> {
    let sql = format!("{BOARD_SELECT} ORDER BY b.name COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| row_to_board(lib, r))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn board_by_id(conn: &Connection, id: i64) -> Result<Board> {
    // Cover paths need a Library; callers that want one use list_boards. This
    // is the cheap shape used right after a mutation, where the UI refreshes
    // the full list anyway.
    let sql = format!("{BOARD_SELECT} WHERE b.id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let board = stmt.query_row([id], |r| {
        let cover_hash: Option<String> = r.get(4)?;
        Ok(Board {
            id: r.get(0)?,
            name: r.get(1)?,
            created_at: r.get(2)?,
            item_count: r.get(3)?,
            cover_thumb_path: cover_hash,
        })
    })?;
    Ok(board)
}

/// Adds assets to a board. Already-present assets are silently kept, so
/// re-adding a selection is safe and does not disturb its original position.
pub fn add_to_board(conn: &mut Connection, board_id: i64, asset_ids: &[i64]) -> Result<usize> {
    let added_at = now_unix();
    let tx = conn.transaction()?;
    let mut added = 0usize;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO board_items (board_id, asset_id, added_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(board_id, asset_id) DO NOTHING",
        )?;
        for id in asset_ids {
            added += stmt.execute(rusqlite::params![board_id, id, added_at])?;
        }
    }
    tx.commit()?;
    Ok(added)
}

pub fn remove_from_board(conn: &mut Connection, board_id: i64, asset_ids: &[i64]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut removed = 0usize;
    {
        let mut stmt =
            tx.prepare("DELETE FROM board_items WHERE board_id = ?1 AND asset_id = ?2")?;
        for id in asset_ids {
            removed += stmt.execute(rusqlite::params![board_id, id])?;
        }
    }
    tx.commit()?;
    Ok(removed)
}

/// Moves assets from one board to another in a single transaction.
///
/// Not add-then-remove from the caller: two round trips can interleave with a
/// refresh and briefly show an asset on both boards or neither. Moving to the
/// board an asset already sits on is a no-op rather than a delete.
pub fn move_to_board(
    conn: &mut Connection,
    from_board: i64,
    to_board: i64,
    asset_ids: &[i64],
) -> Result<usize> {
    if from_board == to_board || asset_ids.is_empty() {
        return Ok(0);
    }
    let added_at = now_unix();
    let tx = conn.transaction()?;
    let mut moved = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT INTO board_items (board_id, asset_id, added_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(board_id, asset_id) DO NOTHING",
        )?;
        let mut delete =
            tx.prepare("DELETE FROM board_items WHERE board_id = ?1 AND asset_id = ?2")?;
        for id in asset_ids {
            insert.execute(rusqlite::params![to_board, id, added_at])?;
            moved += delete.execute(rusqlite::params![from_board, id])?;
        }
    }
    tx.commit()?;
    Ok(moved)
}

/// Assets on a board, most recently added first.
pub fn list_board_assets(
    lib: &Library,
    conn: &Connection,
    board_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<AssetRow>> {
    let mut stmt = conn.prepare(
        "SELECT a.id FROM board_items bi
           JOIN assets a ON a.id = bi.asset_id
          WHERE bi.board_id = ?1
          ORDER BY bi.added_at DESC, bi.asset_id DESC
          LIMIT ?2 OFFSET ?3",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![board_id, limit, offset], |r| {
            r.get::<_, i64>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(asset) = crate::ingest::asset_by_id(lib, conn, id)? {
            out.push(asset);
        }
    }
    Ok(out)
}

/// Board ids each of these assets belongs to, so the UI can show membership
/// without a query per tile.
pub fn boards_for_assets(conn: &Connection, asset_ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    if asset_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", asset_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql =
        format!("SELECT asset_id, board_id FROM board_items WHERE asset_id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = asset_ids
        .iter()
        .map(|i| i as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        lib: Library,
        conn: Connection,
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("burrow-boards-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let lib = Library::open(dir.join("lib")).expect("library");
            let conn = crate::db::open(&lib.db_path()).expect("db");
            Self { lib, conn, dir }
        }

        /// Inserts a bare asset row; boards only care about ids.
        fn asset(&self, hash: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO assets (hash, kind, ext, mime, width, height, bytes, imported_at)
                     VALUES (?1, 'image', 'png', 'image/png', 4, 4, 16, 0)",
                    [hash],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn creating_and_listing_boards() {
        let fx = Fixture::new("create");
        let b = create_board(&fx.conn, "Title Cards").expect("create");
        assert_eq!(b.name, "Title Cards");
        assert_eq!(b.item_count, 0);
        assert!(b.cover_thumb_path.is_none());

        let all = list_boards(&fx.lib, &fx.conn).expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "Title Cards");
    }

    #[test]
    fn creating_an_existing_name_returns_the_same_board() {
        let fx = Fixture::new("get-or-create");
        let first = create_board(&fx.conn, "Motion").expect("first");
        // Different casing and surrounding space must still find it.
        let second = create_board(&fx.conn, "  motion  ").expect("second");

        assert_eq!(first.id, second.id, "should not create a second board");
        assert_eq!(list_boards(&fx.lib, &fx.conn).unwrap().len(), 1);
    }

    #[test]
    fn adding_is_idempotent_and_counted() {
        let fx = Fixture::new("add");
        let mut fx = fx;
        let board = create_board(&fx.conn, "Refs").unwrap();
        let a = fx.asset("aaa");
        let b = fx.asset("bbb");

        assert_eq!(add_to_board(&mut fx.conn, board.id, &[a, b]).unwrap(), 2);
        // Re-adding the same assets adds nothing.
        assert_eq!(add_to_board(&mut fx.conn, board.id, &[a, b]).unwrap(), 0);

        let listed = list_boards(&fx.lib, &fx.conn).unwrap();
        assert_eq!(listed[0].item_count, 2);
    }

    #[test]
    fn an_asset_can_live_on_several_boards() {
        let mut fx = Fixture::new("multi");
        let one = create_board(&fx.conn, "One").unwrap();
        let two = create_board(&fx.conn, "Two").unwrap();
        let a = fx.asset("shared");

        add_to_board(&mut fx.conn, one.id, &[a]).unwrap();
        add_to_board(&mut fx.conn, two.id, &[a]).unwrap();

        let pairs = boards_for_assets(&fx.conn, &[a]).unwrap();
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn removing_from_one_board_leaves_the_others() {
        let mut fx = Fixture::new("remove");
        let one = create_board(&fx.conn, "One").unwrap();
        let two = create_board(&fx.conn, "Two").unwrap();
        let a = fx.asset("shared");
        add_to_board(&mut fx.conn, one.id, &[a]).unwrap();
        add_to_board(&mut fx.conn, two.id, &[a]).unwrap();

        assert_eq!(remove_from_board(&mut fx.conn, one.id, &[a]).unwrap(), 1);

        let pairs = boards_for_assets(&fx.conn, &[a]).unwrap();
        assert_eq!(pairs, vec![(a, two.id)]);
    }

    #[test]
    fn deleting_a_board_keeps_the_assets() {
        let mut fx = Fixture::new("delete");
        let board = create_board(&fx.conn, "Doomed").unwrap();
        let a = fx.asset("survivor");
        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();

        delete_board(&fx.conn, board.id).unwrap();

        assert!(list_boards(&fx.lib, &fx.conn).unwrap().is_empty());
        let assets: i64 = fx
            .conn
            .query_row("SELECT count(*) FROM assets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(assets, 1, "deleting a board must never delete references");
        let items: i64 = fx
            .conn
            .query_row("SELECT count(*) FROM board_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(items, 0, "membership rows should cascade away");
    }

    #[test]
    fn deleting_an_asset_removes_it_from_boards() {
        let mut fx = Fixture::new("asset-delete");
        let board = create_board(&fx.conn, "Refs").unwrap();
        let a = fx.asset("gone");
        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();

        fx.conn
            .execute("DELETE FROM assets WHERE id = ?1", [a])
            .unwrap();

        let listed = list_boards(&fx.lib, &fx.conn).unwrap();
        assert_eq!(listed[0].item_count, 0, "stale membership row survived");
    }

    #[test]
    fn board_contents_are_newest_added_first() {
        let mut fx = Fixture::new("order");
        let board = create_board(&fx.conn, "Ordered").unwrap();
        let a = fx.asset("first");
        let b = fx.asset("second");

        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();
        add_to_board(&mut fx.conn, board.id, &[b]).unwrap();

        let items = list_board_assets(&fx.lib, &fx.conn, board.id, 10, 0).unwrap();
        assert_eq!(items.len(), 2);
        // Same second-resolution timestamp, so asset_id DESC breaks the tie.
        assert_eq!(items[0].id, b);
    }

    #[test]
    fn moving_transfers_membership_exactly_once() {
        let mut fx = Fixture::new("move");
        let from = create_board(&fx.conn, "From").unwrap();
        let to = create_board(&fx.conn, "To").unwrap();
        let a = fx.asset("moving");
        add_to_board(&mut fx.conn, from.id, &[a]).unwrap();

        assert_eq!(
            move_to_board(&mut fx.conn, from.id, to.id, &[a]).unwrap(),
            1
        );

        let pairs = boards_for_assets(&fx.conn, &[a]).unwrap();
        assert_eq!(
            pairs,
            vec![(a, to.id)],
            "asset should be on the target only"
        );
    }

    #[test]
    fn moving_onto_a_board_that_already_has_it_does_not_lose_the_asset() {
        let mut fx = Fixture::new("move-conflict");
        let from = create_board(&fx.conn, "From").unwrap();
        let to = create_board(&fx.conn, "To").unwrap();
        let a = fx.asset("both");
        add_to_board(&mut fx.conn, from.id, &[a]).unwrap();
        add_to_board(&mut fx.conn, to.id, &[a]).unwrap();

        move_to_board(&mut fx.conn, from.id, to.id, &[a]).unwrap();

        // The insert conflicts and is skipped; the delete must still leave it
        // on the target rather than removing it from everywhere.
        let pairs = boards_for_assets(&fx.conn, &[a]).unwrap();
        assert_eq!(pairs, vec![(a, to.id)]);
    }

    #[test]
    fn moving_to_the_same_board_is_a_no_op() {
        let mut fx = Fixture::new("move-self");
        let board = create_board(&fx.conn, "Only").unwrap();
        let a = fx.asset("stays");
        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();

        assert_eq!(
            move_to_board(&mut fx.conn, board.id, board.id, &[a]).unwrap(),
            0
        );
        // Critically, it must not have deleted the membership.
        assert_eq!(
            boards_for_assets(&fx.conn, &[a]).unwrap(),
            vec![(a, board.id)]
        );
    }

    #[test]
    fn renaming_keeps_membership() {
        let mut fx = Fixture::new("rename");
        let board = create_board(&fx.conn, "Old Name").unwrap();
        let a = fx.asset("kept");
        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();

        let renamed = rename_board(&fx.conn, board.id, "New Name").unwrap();
        assert_eq!(renamed.name, "New Name");
        assert_eq!(renamed.item_count, 1);
    }

    #[test]
    fn cover_follows_the_most_recent_addition() {
        let mut fx = Fixture::new("cover");
        let board = create_board(&fx.conn, "Cover").unwrap();
        let a = fx.asset("aaa");
        let b = fx.asset("bbb");
        add_to_board(&mut fx.conn, board.id, &[a]).unwrap();
        add_to_board(&mut fx.conn, board.id, &[b]).unwrap();

        let listed = list_boards(&fx.lib, &fx.conn).unwrap();
        let cover = listed[0].cover_thumb_path.as_deref().expect("a cover");
        assert!(
            cover.contains("bbb"),
            "cover should be the newest item, got {cover}"
        );
    }
}
