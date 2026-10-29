use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use icloud_proto::{RemoteKind, RemoteNode};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{EngineError, Result};

pub const ROOT_INO: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Directory,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "folder",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "file" {
            Self::File
        } else {
            Self::Directory
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    Conflict,
    StalledDownload,
    StalledUpload,
    RemoteMissing,
    FolderSkipped,
    NeedsAuth,
    NeedsTerms,
    NeedsAdp,
}

impl AttentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::StalledDownload => "stalled_download",
            Self::StalledUpload => "stalled_upload",
            Self::RemoteMissing => "remote_missing",
            Self::FolderSkipped => "folder_skipped",
            Self::NeedsAuth => "needs_auth",
            Self::NeedsTerms => "needs_terms",
            Self::NeedsAdp => "needs_adp",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "conflict" => Self::Conflict,
            "stalled_download" => Self::StalledDownload,
            "stalled_upload" => Self::StalledUpload,
            "remote_missing" => Self::RemoteMissing,
            "folder_skipped" => Self::FolderSkipped,
            "needs_terms" => Self::NeedsTerms,
            "needs_adp" => Self::NeedsAdp,
            _ => Self::NeedsAuth,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub ino: i64,
    pub parent: i64,
    pub name: String,
    pub name_apple: String,
    pub remote_parent: Option<String>,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub complete: bool,
    pub pin: bool,
    pub dirty: bool,
    pub blob: Option<String>,
    pub docwsid: Option<String>,
    pub drivewsid: Option<String>,
    pub etag: String,
    pub revision: i64,
    pub last_synced_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Attention {
    pub id: i64,
    pub kind: AttentionKind,
    pub ino: Option<i64>,
    pub path: String,
    pub detail: String,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS items (
                ino INTEGER PRIMARY KEY,
                parent_ino INTEGER NOT NULL,
                drivewsid TEXT UNIQUE,
                docwsid TEXT,
                name_apple TEXT NOT NULL,
                name_local TEXT NOT NULL,
                node_type TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                etag TEXT NOT NULL DEFAULT '',
                mtime_ms INTEGER NOT NULL DEFAULT 0,
                pin INTEGER NOT NULL DEFAULT 0,
                complete INTEGER NOT NULL DEFAULT 0,
                dirty INTEGER NOT NULL DEFAULT 0,
                blob_path TEXT,
                last_synced_ms INTEGER NOT NULL DEFAULT 0,
                revision INTEGER NOT NULL DEFAULT 1
            );
            CREATE UNIQUE INDEX IF NOT EXISTS items_parent_name ON items(parent_ino, name_local);
            CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(name, ino UNINDEXED);
            CREATE TABLE IF NOT EXISTS attention (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                ino INTEGER,
                path TEXT NOT NULL,
                detail TEXT NOT NULL,
                dismissed INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS trash_queue (
                drivewsid TEXT PRIMARY KEY,
                etag TEXT NOT NULL
            );
            ",
        )?;
        self.ensure_column("remote_parent", "remote_parent TEXT")?;
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM items WHERE ino = 1", [], |row| row.get(0))?;
        if count == 0 {
            self.conn.execute(
                "INSERT INTO items (ino, parent_ino, drivewsid, docwsid, name_apple, name_local, node_type, complete, revision)
                 VALUES (1, 0, 'FOLDER::com.apple.CloudDocs::root', 'root', '', '', 'folder', 1, 1)",
                [],
            )?;
        }
        Ok(())
    }

    pub fn node(&self, ino: i64) -> Result<Node> {
        self.query_one("SELECT * FROM items WHERE ino = ?1", params![ino])?
            .ok_or(EngineError::NotFound)
    }

    pub fn child(&self, parent: i64, name: &str) -> Result<Option<Node>> {
        self.query_one(
            "SELECT * FROM items WHERE parent_ino = ?1 AND name_local = ?2",
            params![parent, name],
        )
    }

    pub fn children(&self, parent: i64) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM items WHERE parent_ino = ?1 AND ino != 1 ORDER BY name_local",
        )?;
        let rows = stmt.query_map(params![parent], map_node)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn resolve(&self, path: &str) -> Result<i64> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "/" {
            return Ok(ROOT_INO);
        }
        let mut cur = ROOT_INO;
        for part in trimmed.trim_start_matches('/').split('/') {
            cur = self.child(cur, part)?.ok_or(EngineError::NotFound)?.ino;
        }
        Ok(cur)
    }

    pub fn path_of(&self, ino: i64) -> Result<String> {
        if ino == ROOT_INO {
            return Ok("/".into());
        }
        let mut parts = Vec::new();
        let mut cur = ino;
        while cur != ROOT_INO && cur != 0 {
            let node = self.node(cur)?;
            parts.push(node.name);
            cur = node.parent;
        }
        parts.reverse();
        Ok(format!("/{}", parts.join("/")))
    }

    pub fn ino_for_drivewsid(&self, drivewsid: &str) -> Result<i64> {
        self.conn
            .query_row("SELECT ino FROM items WHERE drivewsid = ?1", params![drivewsid], |row| row.get(0))
            .optional()?
            .ok_or(EngineError::NotFound)
    }

    pub fn search(&self, query: &str) -> Result<Vec<Node>> {
        let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let match_query = tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");
        let inos = match self.conn.prepare("SELECT ino FROM items_fts WHERE items_fts MATCH ?1 LIMIT 100") {
            Ok(mut stmt) => stmt
                .query_map(params![match_query], |row| row.get::<_, String>(0))?
                .filter_map(|ino| ino.ok())
                .filter_map(|ino| ino.parse::<i64>().ok())
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let mut nodes = Vec::new();
        for ino in inos {
            if let Ok(node) = self.node(ino) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    pub fn apply_snapshot(
        &mut self,
        parent: i64,
        started_ms: i64,
        completed: bool,
        children: &[RemoteNode],
    ) -> Result<(Vec<String>, bool)> {
        if !completed {
            return Ok((Vec::new(), false));
        }
        let mut folders = Vec::new();
        let mut changed = false;
        let seen: Vec<String> = children.iter().map(|child| child.drivewsid.clone()).collect();
        let parent_drive = self.node(parent)?.drivewsid.unwrap_or_default();
        for child in children {
            if self.upsert_remote(parent, &parent_drive, child)? {
                changed = true;
            }
            if child.kind == RemoteKind::Folder {
                folders.push(child.drivewsid.clone());
            }
        }
        let local = self.children(parent)?;
        for node in local {
            let Some(drivewsid) = node.drivewsid.clone() else {
                continue;
            };
            if seen.iter().any(|id| id == &drivewsid) {
                continue;
            }
            if node.last_synced_ms >= started_ms {
                continue;
            }
            if node.dirty {
                let path = self.path_of(node.ino).unwrap_or_else(|_| node.name.clone());
                self.raise(
                    AttentionKind::Conflict,
                    Some(node.ino),
                    &path,
                    "This file changed here and on iCloud",
                )?;
                continue;
            }
            self.remove_node(node.ino)?;
            changed = true;
        }
        if changed {
            self.conn.execute(
                "UPDATE items SET mtime_ms = ?1 WHERE ino = ?2",
                params![now_ms(), parent],
            )?;
        }
        Ok((folders, changed))
    }

    pub fn indexed_file_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM items WHERE node_type = 'file'", [], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    fn upsert_remote(&mut self, parent: i64, parent_drivewsid: &str, child: &RemoteNode) -> Result<bool> {
        let kind = match child.kind {
            RemoteKind::File => NodeKind::File,
            RemoteKind::Folder => NodeKind::Directory,
        };
        if let Some(existing) = self.by_drivewsid(&child.drivewsid)? {
            if existing.name == child.name_local
                && existing.etag == child.etag
                && existing.size == child.size
                && existing.mtime_ms == child.mtime_ms
                && existing.parent == parent
            {
                if !parent_drivewsid.is_empty() && existing.remote_parent.as_deref() != Some(parent_drivewsid) {
                    self.set_remote_parent(existing.ino, parent_drivewsid)?;
                }
                return Ok(false);
            }
            if existing.dirty && existing.size != child.size {
                let path = self.path_of(existing.ino)?;
                self.raise(AttentionKind::Conflict, Some(existing.ino), &path, "This file changed here and on iCloud")?;
                self.conn.execute(
                    "UPDATE items SET drivewsid = ?1, docwsid = ?2, etag = ?3 WHERE ino = ?4",
                    params![child.drivewsid, child.docwsid, child.etag, existing.ino],
                )?;
                return Ok(true);
            }
            self.conn.execute(
                "UPDATE items SET parent_ino = ?1, name_apple = ?2, name_local = ?3, node_type = ?4, size = ?5, etag = ?6, mtime_ms = ?7, docwsid = ?8, revision = revision + 1, last_synced_ms = ?9 WHERE ino = ?10",
                params![parent, child.name_apple, child.name_local, kind.as_str(), child.size as i64, child.etag, child.mtime_ms, child.docwsid, now_ms(), existing.ino],
            )?;
            self.reindex(existing.ino, &child.name_local)?;
            return Ok(true);
        }
        if let Some(local) = self.child(parent, &child.name_local)? {
            if local.drivewsid.is_none() {
                self.conn.execute(
                    "UPDATE items SET drivewsid = ?1, docwsid = ?2, etag = ?3, name_apple = ?4, revision = revision + 1 WHERE ino = ?5",
                    params![child.drivewsid, child.docwsid, child.etag, child.name_apple, local.ino],
                )?;
                return Ok(true);
            }
        }
        let ino = self.next_ino()?;
            self.conn.execute(
                "INSERT INTO items (ino, parent_ino, drivewsid, docwsid, name_apple, name_local, node_type, size, etag, mtime_ms, complete, last_synced_ms, revision, remote_parent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, 1, ?12)",
            params![ino, parent, child.drivewsid, child.docwsid, child.name_apple, child.name_local, kind.as_str(), child.size as i64, child.etag, child.mtime_ms, now_ms(), parent_drivewsid],
        )?;
        self.conn.execute("INSERT INTO items_fts (name, ino) VALUES (?1, ?2)", params![child.name_local, ino.to_string()])?;
        Ok(true)
    }

    pub fn create(&mut self, parent: i64, name: &str, kind: NodeKind) -> Result<i64> {
        if self.child(parent, name)?.is_some() {
            return Err(EngineError::AlreadyExists);
        }
        let ino = self.next_ino()?;
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO items (ino, parent_ino, name_apple, name_local, node_type, complete, dirty, mtime_ms, last_synced_ms, revision)
             VALUES (?1, ?2, ?3, ?3, ?4, 0, 1, ?5, ?5, 1)",
            params![ino, parent, name, kind.as_str(), now],
        )?;
        self.conn.execute("INSERT INTO items_fts (name, ino) VALUES (?1, ?2)", params![name, ino.to_string()])?;
        Ok(ino)
    }

    pub fn mark_complete(&mut self, ino: i64, blob: &Path, size: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET complete = 1, blob_path = ?1, size = ?2, mtime_ms = ?3 WHERE ino = ?4",
            params![blob.to_string_lossy().to_string(), size as i64, now_ms(), ino],
        )?;
        Ok(())
    }

    pub fn mark_dirty(&mut self, ino: i64) -> Result<()> {
        self.conn.execute("UPDATE items SET dirty = 1, last_synced_ms = ?1 WHERE ino = ?2", params![now_ms(), ino])?;
        Ok(())
    }

    pub fn clear_dirty(&mut self, ino: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET dirty = 0, last_synced_ms = ?1 WHERE ino = ?2",
            params![now_ms(), ino],
        )?;
        Ok(())
    }

    pub fn set_pin_tree(&mut self, ino: i64, pin: bool) -> Result<()> {
        self.conn.execute("UPDATE items SET pin = ?1 WHERE ino = ?2", params![pin as i64, ino])?;
        for child in self.children(ino)? {
            self.set_pin_tree(child.ino, pin)?;
        }
        Ok(())
    }

    pub fn incomplete_pins(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM items WHERE pin = 1 AND complete = 0 AND node_type = 'file'",
        )?;
        let rows = stmt.query_map([], map_node)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn dirty_directories(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM items WHERE dirty = 1 AND node_type = 'folder' AND ino != 1",
        )?;
        let rows = stmt.query_map([], map_node)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn dirty_files(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM items WHERE dirty = 1 AND node_type = 'file' AND complete = 1",
        )?;
        let rows = stmt.query_map([], map_node)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn remove_node(&mut self, ino: i64) -> Result<()> {
        if ino == ROOT_INO {
            return Err(EngineError::PermissionDenied);
        }
        if let Some(drivewsid) = self.node(ino)?.drivewsid {
            let etag = self.node(ino)?.etag;
            self.conn.execute(
                "INSERT INTO trash_queue (drivewsid, etag) VALUES (?1, ?2) ON CONFLICT(drivewsid) DO UPDATE SET etag = excluded.etag",
                params![drivewsid, etag],
            )?;
        }
        self.conn.execute("DELETE FROM items_fts WHERE ino = ?1", params![ino.to_string()])?;
        self.conn.execute("DELETE FROM items WHERE ino = ?1", params![ino])?;
        Ok(())
    }

    pub fn take_trash(&mut self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT drivewsid, etag FROM trash_queue")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        self.conn.execute("DELETE FROM trash_queue", [])?;
        Ok(items)
    }

    pub fn requeue_trash(&mut self, drivewsid: &str, etag: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO trash_queue (drivewsid, etag) VALUES (?1, ?2) ON CONFLICT(drivewsid) DO UPDATE SET etag = excluded.etag",
            params![drivewsid, etag],
        )?;
        Ok(())
    }

    pub fn move_node(&mut self, ino: i64, parent: i64, name: &str) -> Result<()> {
        if self.child(parent, name)?.is_some() {
            return Err(EngineError::AlreadyExists);
        }
        self.conn.execute(
            "UPDATE items SET parent_ino = ?1, name_local = ?2, dirty = 1, revision = revision + 1, last_synced_ms = ?3 WHERE ino = ?4",
            params![parent, name, now_ms(), ino],
        )?;
        self.reindex(ino, name)?;
        Ok(())
    }

    pub fn commit_replace(&mut self, src: i64, dst: i64, new_blob: &Path, new_size: u64) -> Result<Option<String>> {
        let src_node = self.node(src)?;
        if !src_node.complete {
            return Err(EngineError::Message("refusing to replace a file with an incomplete write".into()));
        }
        let old_blob = self.node(dst)?.blob;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE items SET blob_path = ?1, size = ?2, complete = 1, dirty = 1, mtime_ms = ?3, revision = revision + 1, last_synced_ms = ?3 WHERE ino = ?4",
            params![new_blob.to_string_lossy().to_string(), new_size as i64, now_ms(), dst],
        )?;
        tx.execute("DELETE FROM items_fts WHERE ino = ?1", params![src.to_string()])?;
        tx.execute("DELETE FROM items WHERE ino = ?1", params![src])?;
        tx.commit()?;
        Ok(old_blob)
    }

    pub fn forget_local_bytes(&mut self, ino: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET complete = 0, dirty = 0, blob_path = NULL WHERE ino = ?1",
            params![ino],
        )?;
        Ok(())
    }

    pub fn fork_conflict_copy(&mut self, ino: i64) -> Result<i64> {
        let node = self.node(ino)?;
        let mut name = format!("{} (local conflict)", node.name);
        let mut n = 2i32;
        loop {
            match self.create(node.parent, &name, NodeKind::File) {
                Ok(copy) => return Ok(copy),
                Err(EngineError::AlreadyExists) if n < 50 => {
                    name = format!("{} (local conflict {n})", node.name);
                    n += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn set_docwsid(&mut self, drivewsid: &str, docwsid: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET docwsid = ?1 WHERE drivewsid = ?2",
            params![docwsid, drivewsid],
        )?;
        Ok(())
    }

    pub fn bind_remote(
        &mut self,
        ino: i64,
        drivewsid: &str,
        docwsid: &str,
        etag: &str,
        remote_parent: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET drivewsid = ?1, docwsid = ?2, etag = ?3, remote_parent = ?4, name_apple = name_local, dirty = 0, last_synced_ms = ?5 WHERE ino = ?6",
            params![drivewsid, docwsid, etag, remote_parent, now_ms(), ino],
        )?;
        Ok(())
    }

    pub fn set_name_apple(&mut self, ino: i64, name: &str) -> Result<()> {
        self.conn.execute("UPDATE items SET name_apple = ?1 WHERE ino = ?2", params![name, ino])?;
        Ok(())
    }

    pub fn set_remote_parent(&mut self, ino: i64, parent_drivewsid: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET remote_parent = ?1 WHERE ino = ?2",
            params![parent_drivewsid, ino],
        )?;
        Ok(())
    }

    pub fn has_attention(&self, kind: AttentionKind, ino: i64) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM attention WHERE kind = ?1 AND ino = ?2 AND dismissed = 0",
                params![kind.as_str(), ino],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    fn ensure_column(&self, name: &str, decl: &str) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(items)")?;
        let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for existing in names {
            if existing? == name {
                return Ok(());
            }
        }
        self.conn.execute(&format!("ALTER TABLE items ADD COLUMN {decl}"), [])?;
        Ok(())
    }

    pub fn raise(&self, kind: AttentionKind, ino: Option<i64>, path: &str, detail: &str) -> Result<()> {
        let existing: Option<i64> = self.conn.query_row(
            "SELECT id FROM attention WHERE kind = ?1 AND path = ?2 AND dismissed = 0",
            params![kind.as_str(), path],
            |row| row.get(0),
        ).optional()?;
        if existing.is_some() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO attention (kind, ino, path, detail, dismissed) VALUES (?1, ?2, ?3, ?4, 0)",
            params![kind.as_str(), ino, path, detail],
        )?;
        Ok(())
    }

    pub fn list_attention(&self) -> Result<Vec<Attention>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, ino, path, detail FROM attention WHERE dismissed = 0 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Attention {
                id: row.get(0)?,
                kind: AttentionKind::parse(&row.get::<_, String>(1)?),
                ino: row.get(2)?,
                path: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn attention(&self, id: i64) -> Result<Attention> {
        self.list_attention()?
            .into_iter()
            .find(|item| item.id == id)
            .ok_or(EngineError::NotFound)
    }

    pub fn attention_count(&self) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attention WHERE dismissed = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    pub fn dismiss(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE attention SET dismissed = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn dismiss_kind(&self, kind: AttentionKind) -> Result<()> {
        self.conn.execute(
            "UPDATE attention SET dismissed = 1 WHERE kind = ?1 AND dismissed = 0",
            params![kind.as_str()],
        )?;
        Ok(())
    }

    pub fn dismiss_path(&self, kind: AttentionKind, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attention SET dismissed = 1 WHERE kind = ?1 AND path = ?2",
            params![kind.as_str(), path],
        )?;
        Ok(())
    }

    fn by_drivewsid(&self, drivewsid: &str) -> Result<Option<Node>> {
        self.query_one("SELECT * FROM items WHERE drivewsid = ?1", params![drivewsid])
    }

    fn next_ino(&self) -> Result<i64> {
        let max: i64 = self.conn.query_row("SELECT COALESCE(MAX(ino), 1) FROM items", [], |row| row.get(0))?;
        Ok(max + 1)
    }

    fn reindex(&self, ino: i64, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM items_fts WHERE ino = ?1", params![ino.to_string()])?;
        self.conn.execute("INSERT INTO items_fts (name, ino) VALUES (?1, ?2)", params![name, ino.to_string()])?;
        Ok(())
    }

    fn query_one(&self, sql: &str, params: impl rusqlite::Params) -> Result<Option<Node>> {
        self.conn.query_row(sql, params, map_node).optional().map_err(Into::into)
    }
}

fn map_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        ino: row.get("ino")?,
        parent: row.get("parent_ino")?,
        name: row.get("name_local")?,
        name_apple: row.get("name_apple")?,
        remote_parent: row.get("remote_parent")?,
        kind: NodeKind::parse(&row.get::<_, String>("node_type")?),
        size: row.get::<_, i64>("size")? as u64,
        mtime_ms: row.get("mtime_ms")?,
        complete: row.get::<_, i64>("complete")? != 0,
        pin: row.get::<_, i64>("pin")? != 0,
        dirty: row.get::<_, i64>("dirty")? != 0,
        blob: row.get("blob_path")?,
        docwsid: row.get("docwsid")?,
        drivewsid: row.get("drivewsid")?,
        etag: row.get("etag")?,
        revision: row.get("revision")?,
        last_synced_ms: row.get("last_synced_ms")?,
    })
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use icloud_proto::RemoteKind;

    fn file(id: &str, name: &str, size: u64, etag: &str) -> RemoteNode {
        RemoteNode {
            drivewsid: format!("FILE::com.apple.CloudDocs::{id}"),
            docwsid: id.into(),
            name_apple: name.into(),
            name_local: name.into(),
            extension: None,
            kind: RemoteKind::File,
            size,
            etag: etag.into(),
            mtime_ms: 10,
        }
    }

    #[test]
    fn unchanged_metadata_does_not_bump_revision() {
        let mut db = Db::open_memory().unwrap();
        let child = file("a", "Notes.txt", 4, "e1");
        db.apply_snapshot(ROOT_INO, 1, true, &[child.clone()]).unwrap();
        let first = db.child(ROOT_INO, "Notes.txt").unwrap().unwrap().revision;
        db.apply_snapshot(ROOT_INO, 2, true, &[child]).unwrap();
        let second = db.child(ROOT_INO, "Notes.txt").unwrap().unwrap().revision;
        assert_eq!(first, second);
    }

    #[test]
    fn incomplete_listing_does_not_remove_files() {
        let mut db = Db::open_memory().unwrap();
        db.apply_snapshot(ROOT_INO, 1, true, &[file("a", "Notes.txt", 4, "e1")]).unwrap();
        db.apply_snapshot(ROOT_INO, 50, false, &[]).unwrap();
        assert!(db.child(ROOT_INO, "Notes.txt").unwrap().is_some());
    }

    #[test]
    fn upload_during_crawl_is_kept() {
        let mut db = Db::open_memory().unwrap();
        let started = now_ms();
        db.apply_snapshot(ROOT_INO, started, true, &[file("old", "Old.txt", 1, "e")]).unwrap();
        let ino = db.create(ROOT_INO, "New.txt", NodeKind::File).unwrap();
        db.clear_dirty(ino).unwrap();
        db.conn.execute(
            "UPDATE items SET drivewsid = 'FILE::com.apple.CloudDocs::new', last_synced_ms = ?1, dirty = 0 WHERE ino = ?2",
            params![now_ms(), ino],
        ).unwrap();
        db.apply_snapshot(ROOT_INO, started, true, &[file("old", "Old.txt", 1, "e")]).unwrap();
        assert!(db.child(ROOT_INO, "New.txt").unwrap().is_some());
    }

    #[test]
    fn remote_delete_of_a_clean_file_drops_the_index_row() {
        let mut db = Db::open_memory().unwrap();
        db.apply_snapshot(ROOT_INO, 1, true, &[file("a", "Gone.txt", 1, "e")]).unwrap();
        db.conn.execute("UPDATE items SET last_synced_ms = 1, dirty = 0", []).unwrap();
        db.apply_snapshot(ROOT_INO, 10_000, true, &[]).unwrap();
        assert!(db.child(ROOT_INO, "Gone.txt").unwrap().is_none());
    }

    #[test]
    fn dirty_file_missing_remotely_becomes_a_conflict() {
        let mut db = Db::open_memory().unwrap();
        db.apply_snapshot(ROOT_INO, 1, true, &[file("a", "Edited.txt", 1, "e")]).unwrap();
        let node = db.child(ROOT_INO, "Edited.txt").unwrap().unwrap();
        db.mark_dirty(node.ino).unwrap();
        db.conn.execute("UPDATE items SET last_synced_ms = 1 WHERE ino = ?1", params![node.ino]).unwrap();
        db.apply_snapshot(ROOT_INO, 10_000, true, &[]).unwrap();
        assert!(db.child(ROOT_INO, "Edited.txt").unwrap().is_some());
        assert_eq!(db.attention_count().unwrap(), 1);
    }

    #[test]
    fn replace_refuses_an_incomplete_source() {
        let mut db = Db::open_memory().unwrap();
        let dst = db.create(ROOT_INO, "Sheet.ods", NodeKind::File).unwrap();
        db.mark_complete(dst, Path::new("/tmp/old"), 20).unwrap();
        let src = db.create(ROOT_INO, ".Sheet.tmp", NodeKind::File).unwrap();
        let before = db.node(dst).unwrap().size;
        let err = db.commit_replace(src, dst, Path::new("/tmp/new"), 0).unwrap_err();
        assert!(err.to_string().contains("incomplete"));
        assert_eq!(db.node(dst).unwrap().size, before);
        assert!(db.node(src).is_ok());
    }

    #[test]
    fn successful_replace_keeps_destination_identity() {
        let mut db = Db::open_memory().unwrap();
        let dst = db.create(ROOT_INO, "Sheet.ods", NodeKind::File).unwrap();
        db.mark_complete(dst, Path::new("/tmp/old"), 20).unwrap();
        let src = db.create(ROOT_INO, ".Sheet.tmp", NodeKind::File).unwrap();
        db.mark_complete(src, Path::new("/tmp/new"), 8).unwrap();
        let old = db.commit_replace(src, dst, Path::new("/tmp/new"), 8).unwrap();
        assert_eq!(old.as_deref(), Some("/tmp/old"));
        let dst = db.node(dst).unwrap();
        assert_eq!(dst.size, 8);
        assert!(dst.dirty);
        assert!(db.node(src).is_err());
    }

    #[test]
    fn app_library_round_trip_is_searchable() {
        let mut db = Db::open_memory().unwrap();
        let library = RemoteNode {
            drivewsid: "FOLDER::com.apple.CloudDocs::obs".into(),
            docwsid: "obs".into(),
            name_apple: "Obsidian".into(),
            name_local: "Obsidian".into(),
            extension: None,
            kind: RemoteKind::Folder,
            size: 0,
            etag: "1".into(),
            mtime_ms: 1,
        };
        db.apply_snapshot(ROOT_INO, 1, true, &[library]).unwrap();
        let hits = db.search("Obsidian").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, NodeKind::Directory);
    }
}
