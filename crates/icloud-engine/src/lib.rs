//! Local index, pin rules, and the sync decisions that must not lose data.

mod blobs;
mod db;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use icloud_proto::{AuthStep, IcloudClient, ProtoError, SavedSession, ROOT_DRIVEWSID};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub use db::{AttentionKind, Node, NodeKind};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("already exists")]
    AlreadyExists,
    #[error("directory not empty")]
    NotEmpty,
    #[error("is a directory")]
    IsDirectory,
    #[error("not a directory")]
    NotADirectory,
    #[error("invalid name")]
    InvalidName,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    NeedsAuth,
    Indexing,
    Downloading,
    Uploading,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::NeedsAuth => "needs_auth",
            Self::Indexing => "indexing",
            Self::Downloading => "downloading",
            Self::Uploading => "uploading",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    pub phase: Phase,
    pub label: String,
    pub current_path: String,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub items_done: u64,
    pub items_total: Option<u64>,
    pub attention: u32,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            label: "Idle".into(),
            current_path: String::new(),
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: None,
            attention: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchReason {
    Open,
    Pin,
    Explicit,
}

pub struct Engine {
    db: Mutex<db::Db>,
    blobs: blobs::BlobStore,
    client: tokio::sync::Mutex<IcloudClient>,
    allow_deletion: AtomicBool,
    status: Mutex<SyncStatus>,
    transfers: Arc<Semaphore>,
    cancel: Mutex<CancellationToken>,
    wake: Notify,
    poll_seconds: AtomicU32,
    shutdown: CancellationToken,
    dir_changed: Mutex<Option<Arc<dyn Fn(i64) + Send + Sync>>>,
}

impl Engine {
    pub fn open(
        database: &Path,
        blobs: &Path,
        client: IcloudClient,
        allow_deletion: bool,
        poll_seconds: u32,
    ) -> Result<Self> {
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(blobs)?;
        let db = db::Db::open(database)?;
        Ok(Self {
            db: Mutex::new(db),
            blobs: blobs::BlobStore::new(blobs),
            client: tokio::sync::Mutex::new(client),
            allow_deletion: AtomicBool::new(allow_deletion),
            status: Mutex::new(SyncStatus::default()),
            transfers: Arc::new(Semaphore::new(1)),
            cancel: Mutex::new(CancellationToken::new()),
            wake: Notify::new(),
            poll_seconds: AtomicU32::new(poll_seconds.max(30)),
            shutdown: CancellationToken::new(),
            dir_changed: Mutex::new(None),
        })
    }

    /// Called after a folder's names are written, so the mount can refresh that directory.
    pub fn on_directory_changed(&self, listener: impl Fn(i64) + Send + Sync + 'static) {
        *self.dir_changed.lock().expect("dir") = Some(Arc::new(listener));
    }

    pub fn status(&self) -> SyncStatus {
        let mut status = self.status.lock().expect("status").clone();
        status.attention = self.db.lock().expect("db").attention_count().unwrap_or(0);
        status
    }

    pub fn allow_deletion(&self) -> bool {
        self.allow_deletion.load(Ordering::Relaxed)
    }

    pub fn set_allow_deletion(&self, enabled: bool) {
        self.allow_deletion.store(enabled, Ordering::Relaxed);
    }

    pub fn search(&self, query: &str) -> Result<Vec<Node>> {
        self.db.lock().expect("db").search(query)
    }

    pub fn attention(&self) -> Result<Vec<db::Attention>> {
        self.db.lock().expect("db").list_attention()
    }

    pub fn lookup(&self, parent: i64, name: &str) -> Result<Option<Node>> {
        self.db.lock().expect("db").child(parent, name)
    }

    pub fn getattr(&self, ino: i64) -> Result<Node> {
        self.db.lock().expect("db").node(ino)
    }

    pub fn readdir(&self, ino: i64) -> Result<Vec<Node>> {
        self.db.lock().expect("db").children(ino)
    }

    pub fn resolve(&self, path: &str) -> Result<Node> {
        let ino = self.db.lock().expect("db").resolve(path)?;
        self.getattr(ino)
    }

    pub async fn export_session(&self) -> SavedSession {
        self.client.lock().await.export()
    }

    pub async fn is_signed_in(&self) -> bool {
        self.client.lock().await.is_signed_in()
    }

    pub async fn sign_in(&self, apple_id: &str, password: &str) -> Result<AuthStep> {
        let step = self.client.lock().await.sign_in(apple_id, password).await?;
        self.note_auth(&step);
        Ok(step)
    }

    pub async fn submit_code(&self, code: &str) -> Result<AuthStep> {
        let step = self.client.lock().await.submit_code(code).await?;
        self.note_auth(&step);
        Ok(step)
    }

    pub async fn request_sms(&self) -> Result<AuthStep> {
        self.client.lock().await.request_sms().await.map_err(Into::into)
    }

    pub async fn accept_terms(&self) -> Result<AuthStep> {
        let step = self.client.lock().await.accept_terms().await?;
        self.note_auth(&step);
        Ok(step)
    }

    pub async fn request_device_approval(&self) -> Result<AuthStep> {
        let step = self.client.lock().await.request_device_approval().await?;
        self.note_auth(&step);
        Ok(step)
    }

    pub fn poke_sync(&self) {
        self.wake.notify_waiters();
    }

    pub fn cancel_transfers(&self) {
        let mut token = self.cancel.lock().expect("cancel");
        token.cancel();
        *token = CancellationToken::new();
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        self.cancel_transfers();
    }

    pub async fn run_until_shutdown(self: &Arc<Self>) {
        let _ = self.sync_once().await;
        loop {
            let seconds = self.poll_seconds.load(Ordering::Relaxed) as u64;
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = self.wake.notified() => {
                    let _ = self.sync_once().await;
                }
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => {
                    let _ = self.sync_once().await;
                }
            }
        }
    }

    pub async fn sync_once(&self) -> Result<()> {
        if !self.is_signed_in().await {
            self.set_status(Phase::NeedsAuth, "Sign in again", "", 0, None, 0, None);
            self.db.lock().expect("db").raise(
                AttentionKind::NeedsAuth,
                None,
                "/",
                "Apple needs you to sign in again",
            )?;
            return Ok(());
        }
        self.db.lock().expect("db").dismiss_kind(AttentionKind::NeedsAuth)?;
        let mut queue = VecDeque::from([ROOT_DRIVEWSID.to_string()]);
        let mut seen = std::collections::HashSet::from([ROOT_DRIVEWSID.to_string()]);
        let mut total_files: Option<u64> = None;
        while let Some(drivewsid) = queue.pop_front() {
            if self.shutdown.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let started = now_ms();
            let parent = self.db.lock().expect("db").ino_for_drivewsid(&drivewsid)?;
            let path = self.db.lock().expect("db").path_of(parent).unwrap_or_else(|_| "/".into());
            self.report_index(&path, total_files);
            let listed = {
                let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
                self.client.lock().await.list_folder(&drivewsid).await
            };
            match listed {
                Ok(listing) => {
                    if drivewsid == ROOT_DRIVEWSID {
                        total_files = listing.file_count.or(total_files);
                    }
                    if let Some(doc) = listing.docwsid.filter(|doc| !doc.is_empty() && doc != "root") {
                        self.db.lock().expect("db").set_docwsid(&drivewsid, &doc)?;
                    }
                    let (folders, changed) = self.db.lock().expect("db").apply_snapshot(
                        parent,
                        started,
                        true,
                        &listing.children,
                    )?;
                    for folder in folders {
                        if seen.insert(folder.clone()) {
                            queue.push_back(folder);
                        }
                    }
                    if changed {
                        self.note_directory(parent);
                    }
                }
                Err(ProtoError::Api { status, message }) if status == 423 || status == 401 => {
                    let kind = if status == 401 {
                        AttentionKind::NeedsAuth
                    } else {
                        AttentionKind::NeedsAdp
                    };
                    self.db.lock().expect("db").raise(kind, Some(parent), "/", &message)?;
                    let files = self.db.lock().expect("db").indexed_file_count().unwrap_or(0);
                    self.set_status(Phase::NeedsAuth, &message, "", 0, None, files, total_files);
                    return Ok(());
                }
                Err(err) => {
                    self.db.lock().expect("db").apply_snapshot(parent, started, false, &[])?;
                    self.db.lock().expect("db").raise(
                        AttentionKind::FolderSkipped,
                        Some(parent),
                        &path,
                        &err.to_string(),
                    )?;
                }
            }
            self.report_index(&path, total_files);
        }
        self.hydrate_pins().await?;
        self.upload_dirty().await?;
        self.flush_trash().await?;
        let files = self.db.lock().expect("db").indexed_file_count().unwrap_or(0);
        self.set_status(Phase::Idle, "Up to date", "", 0, None, files, Some(files));
        Ok(())
    }

    pub async fn ensure_content(&self, ino: i64, reason: FetchReason) -> Result<PathBuf> {
        let node = self.db.lock().expect("db").node(ino)?;
        if node.kind == NodeKind::Directory {
            return Err(EngineError::IsDirectory);
        }
        if node.complete {
            if let Some(path) = &node.blob {
                if Path::new(path).is_file() {
                    return Ok(PathBuf::from(path));
                }
            }
            if node.size == 0 {
                let path = self.blobs.path_for(ino);
                std::fs::write(&path, [])?;
                return Ok(path);
            }
        }
        if node.size == 0 {
            let path = self.blobs.path_for(ino);
            std::fs::write(&path, [])?;
            self.db.lock().expect("db").mark_complete(ino, &path, 0)?;
            return Ok(path);
        }
        let Some(docwsid) = node.docwsid.clone().filter(|id| !id.is_empty()) else {
            return Err(EngineError::Message("this file has no iCloud id yet".into()));
        };
        if self.db.lock().expect("db").has_attention(AttentionKind::RemoteMissing, ino)? {
            return Err(EngineError::Message(
                "this iCloud file is gone; refresh it from the attention list".into(),
            ));
        }
        let path = self.db.lock().expect("db").path_of(ino)?;
        self.set_status(
            Phase::Downloading,
            &format!("Downloading {path}"),
            &path,
            0,
            Some(node.size),
            0,
            None,
        );
        let temp = self.blobs.temp_for(ino);
        let cancel = self.cancel.lock().expect("cancel").child_token();
        let download = {
            let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
            self.client
                .lock()
                .await
                .download_to(&docwsid, &temp, &cancel)
                .await
        };
        match download {
            Ok(written) if written == node.size => {
                let final_path = self.blobs.path_for(ino);
                if let Some(parent) = final_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&temp, &final_path)?;
                self.db.lock().expect("db").mark_complete(ino, &final_path, written)?;
                self.db.lock().expect("db").dismiss_path(AttentionKind::StalledDownload, &path)?;
                let _ = reason;
                self.set_status(Phase::Idle, "Up to date", "", written, Some(written), 0, None);
                Ok(final_path)
            }
            Ok(written) => {
                self.db.lock().expect("db").raise(
                    AttentionKind::StalledDownload,
                    Some(ino),
                    &path,
                    &format!("download stopped at {written} of {} bytes", node.size),
                )?;
                Err(EngineError::Message("download was incomplete".into()))
            }
            Err(err) => {
                let gone = match &err {
                    ProtoError::Api { status, message } => {
                        *status == 404
                            || *status == 409
                            || message.to_ascii_lowercase().contains("deleted")
                            || message.to_ascii_lowercase().contains("not found")
                    }
                    _ => false,
                };
                let kind = if gone {
                    AttentionKind::RemoteMissing
                } else {
                    AttentionKind::StalledDownload
                };
                self.db.lock().expect("db").raise(kind, Some(ino), &path, &err.to_string())?;
                self.set_status(Phase::Error, &err.to_string(), &path, 0, Some(node.size), 0, None);
                Err(err.into())
            }
        }
    }

    pub fn set_pin(&self, path: &str, pin: bool) -> Result<()> {
        let ino = self.db.lock().expect("db").resolve(path)?;
        self.db.lock().expect("db").set_pin_tree(ino, pin)?;
        if pin {
            self.wake.notify_waiters();
        } else {
            self.evict_unpinned(ino)?;
        }
        Ok(())
    }

    /// Drop cached bytes for a file that is no longer pinned and has no local edits.
    /// Metadata stays, so the name remains a placeholder.
    fn evict_unpinned(&self, ino: i64) -> Result<()> {
        let node = self.db.lock().expect("db").node(ino)?;
        if node.kind == NodeKind::Directory {
            let children = self.db.lock().expect("db").children(ino)?;
            for child in children {
                self.evict_unpinned(child.ino)?;
            }
            return Ok(());
        }
        if node.pin || node.dirty {
            return Ok(());
        }
        if let Some(blob) = node.blob.as_deref() {
            let _ = std::fs::remove_file(blob);
        }
        let _ = std::fs::remove_file(self.blobs.path_for(ino));
        let _ = std::fs::remove_file(self.blobs.temp_for(ino));
        self.db.lock().expect("db").forget_local_bytes(ino)?;
        Ok(())
    }

    pub fn create_file(&self, parent: i64, name: &str) -> Result<Node> {
        let name = clean_name(name)?;
        let ino = self.db.lock().expect("db").create(parent, &name, NodeKind::File)?;
        let path = self.blobs.path_for(ino);
        std::fs::write(&path, [])?;
        self.db.lock().expect("db").mark_complete(ino, &path, 0)?;
        self.db.lock().expect("db").mark_dirty(ino)?;
        self.getattr(ino)
    }

    pub fn mkdir(&self, parent: i64, name: &str) -> Result<Node> {
        let name = clean_name(name)?;
        let ino = self.db.lock().expect("db").create(parent, &name, NodeKind::Directory)?;
        self.getattr(ino)
    }

    pub fn write_at(&self, ino: i64, offset: u64, data: &[u8]) -> Result<u32> {
        let node = self.db.lock().expect("db").node(ino)?;
        if node.kind != NodeKind::File {
            return Err(EngineError::IsDirectory);
        }
        let path = if let Some(path) = node.blob.clone() {
            PathBuf::from(path)
        } else {
            let path = self.blobs.path_for(ino);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, [])?;
            path
        };
        let mut file = std::fs::OpenOptions::new().write(true).create(true).open(&path)?;
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        file.sync_all()?;
        let len = file.metadata()?.len();
        self.db.lock().expect("db").mark_complete(ino, &path, len)?;
        self.db.lock().expect("db").mark_dirty(ino)?;
        Ok(data.len() as u32)
    }

    pub fn truncate(&self, ino: i64, size: u64) -> Result<()> {
        let node = self.db.lock().expect("db").node(ino)?;
        if node.kind != NodeKind::File {
            return Err(EngineError::IsDirectory);
        }
        let path = if let Some(path) = node.blob.clone() {
            PathBuf::from(path)
        } else {
            let path = self.blobs.path_for(ino);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, [])?;
            path
        };
        let file = std::fs::OpenOptions::new().write(true).create(true).open(&path)?;
        file.set_len(size)?;
        file.sync_all()?;
        self.db.lock().expect("db").mark_complete(ino, &path, size)?;
        self.db.lock().expect("db").mark_dirty(ino)?;
        Ok(())
    }

    pub fn unlink(&self, parent: i64, name: &str) -> Result<()> {
        if !self.allow_deletion() {
            return Err(EngineError::PermissionDenied);
        }
        let node = self
            .db
            .lock()
            .expect("db")
            .child(parent, name)?
            .ok_or(EngineError::NotFound)?;
        if node.kind != NodeKind::File {
            return Err(EngineError::IsDirectory);
        }
        self.db.lock().expect("db").remove_node(node.ino)?;
        Ok(())
    }

    pub fn rmdir(&self, parent: i64, name: &str) -> Result<()> {
        if !self.allow_deletion() {
            return Err(EngineError::PermissionDenied);
        }
        let node = self
            .db
            .lock()
            .expect("db")
            .child(parent, name)?
            .ok_or(EngineError::NotFound)?;
        if node.kind != NodeKind::Directory {
            return Err(EngineError::NotADirectory);
        }
        if !self.db.lock().expect("db").children(node.ino)?.is_empty() {
            return Err(EngineError::NotEmpty);
        }
        self.db.lock().expect("db").remove_node(node.ino)?;
        Ok(())
    }

    pub fn rename(&self, parent: i64, name: &str, new_parent: i64, new_name: &str) -> Result<()> {
        let new_name = clean_name(new_name)?;
        let src = self
            .db
            .lock()
            .expect("db")
            .child(parent, name)?
            .ok_or(EngineError::NotFound)?;
        if let Some(dst) = self.db.lock().expect("db").child(new_parent, &new_name)? {
            if src.kind != NodeKind::File || dst.kind != NodeKind::File {
                return Err(EngineError::PermissionDenied);
            }
            if !src.complete {
                return Err(EngineError::Message("the new file is not fully written".into()));
            }
            let Some(src_blob) = src.blob.clone() else {
                return Err(EngineError::Message("the new file has no contents".into()));
            };
            let new_blob = self.blobs.path_for(dst.ino);
            std::fs::copy(&src_blob, &new_blob)?;
            std::fs::File::open(&new_blob)?.sync_all()?;
            match self.db.lock().expect("db").commit_replace(src.ino, dst.ino, &new_blob, src.size) {
                Ok(old) => {
                    if let Some(old) = old {
                        if old != src_blob && old != new_blob.to_string_lossy() {
                            let _ = std::fs::remove_file(old);
                        }
                    }
                    if src_blob != new_blob.to_string_lossy() {
                        let _ = std::fs::remove_file(src_blob);
                    }
                }
                Err(err) => {
                    let _ = std::fs::remove_file(&new_blob);
                    return Err(err);
                }
            }
            return Ok(());
        }
        self.db.lock().expect("db").move_node(src.ino, new_parent, &new_name)
    }

    pub fn chmod_ok(&self, ino: i64) -> Result<()> {
        let _ = self.db.lock().expect("db").node(ino)?;
        Ok(())
    }

    pub fn resolve_attention(&self, id: i64, action: &str) -> Result<()> {
        let item = self.db.lock().expect("db").attention(id)?;
        match action {
            "dismiss" => self.db.lock().expect("db").dismiss(id),
            "keep_mine" => {
                if let Some(ino) = item.ino {
                    self.db.lock().expect("db").mark_dirty(ino)?;
                }
                self.db.lock().expect("db").dismiss(id)
            }
            "keep_theirs" => {
                if let Some(ino) = item.ino {
                    self.db.lock().expect("db").forget_local_bytes(ino)?;
                }
                self.db.lock().expect("db").dismiss(id)
            }
            "keep_both" => {
                if let Some(ino) = item.ino {
                    let node = self.db.lock().expect("db").node(ino)?;
                    let copy = self.db.lock().expect("db").fork_conflict_copy(ino)?;
                    if let Some(blob) = node.blob.clone() {
                        let dest = self.blobs.path_for(copy);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::copy(&blob, &dest)?;
                        self.db.lock().expect("db").mark_complete(copy, &dest, node.size)?;
                        self.db.lock().expect("db").mark_dirty(copy)?;
                    }
                    self.db.lock().expect("db").forget_local_bytes(ino)?;
                }
                self.db.lock().expect("db").dismiss(id)
            }
            "discard" => {
                if let Some(ino) = item.ino {
                    let _ = std::fs::remove_file(self.blobs.temp_for(ino));
                }
                self.db.lock().expect("db").dismiss(id)
            }
            "retry" | "refresh" => {
                self.db.lock().expect("db").dismiss(id)?;
                self.wake.notify_waiters();
                Ok(())
            }
            _ => Err(EngineError::Message(format!("unknown action {action}"))),
        }
    }

    async fn hydrate_pins(&self) -> Result<()> {
        let pinned = self.db.lock().expect("db").incomplete_pins()?;
        for node in pinned {
            if self.shutdown.is_cancelled() {
                break;
            }
            let _ = self.ensure_content(node.ino, FetchReason::Pin).await;
        }
        Ok(())
    }

    async fn upload_dirty(&self) -> Result<()> {
        self.publish_directories().await?;
        let dirty = self.db.lock().expect("db").dirty_files()?;
        if dirty.is_empty() {
            return Ok(());
        }
        self.set_status(Phase::Uploading, "Uploading", "", 0, None, 0, Some(dirty.len() as u64));
        for node in dirty {
            let path = self.db.lock().expect("db").path_of(node.ino).unwrap_or(node.name.clone());
            if let Err(err) = self.place_remote(&node).await {
                self.db.lock().expect("db").raise(
                    AttentionKind::StalledUpload,
                    Some(node.ino),
                    &path,
                    &err.to_string(),
                )?;
                continue;
            }
            let Some(blob) = node.blob.clone() else { continue };
            let parent = self.db.lock().expect("db").node(node.parent)?;
            let Some(folder) = parent.docwsid.clone().filter(|id| !id.is_empty() && id != "root") else {
                continue;
            };
            self.set_status(Phase::Uploading, &format!("Uploading {path}"), &path, 0, Some(node.size), 0, None);
            let cancel = self.cancel.lock().expect("cancel").child_token();
            if cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let existing = node.docwsid.clone().filter(|id| !id.is_empty());
            let result = {
                let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
                self.client
                    .lock()
                    .await
                    .upload_new(&folder, &node.name, Path::new(&blob), existing.as_deref())
                    .await
            };
            match result {
                Ok(()) => {
                    self.db.lock().expect("db").clear_dirty(node.ino)?;
                    self.db.lock().expect("db").dismiss_path(AttentionKind::StalledUpload, &path)?;
                }
                Err(err) => {
                    self.db.lock().expect("db").raise(
                        AttentionKind::StalledUpload,
                        Some(node.ino),
                        &path,
                        &err.to_string(),
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn publish_directories(&self) -> Result<()> {
        let mut dirs = self.db.lock().expect("db").dirty_directories()?;
        dirs.sort_by_key(|node| {
            self.db
                .lock()
                .expect("db")
                .path_of(node.ino)
                .map(|path| path.chars().filter(|c| *c == '/').count())
                .unwrap_or(0)
        });
        for node in dirs {
            let path = self.db.lock().expect("db").path_of(node.ino).unwrap_or(node.name.clone());
            if node.drivewsid.is_some() {
                if let Err(err) = self.place_remote(&node).await {
                    self.db.lock().expect("db").raise(
                        AttentionKind::StalledUpload,
                        Some(node.ino),
                        &path,
                        &err.to_string(),
                    )?;
                } else {
                    self.db.lock().expect("db").clear_dirty(node.ino)?;
                }
                continue;
            }
            let parent = self.db.lock().expect("db").node(node.parent)?;
            let Some(dest) = parent.drivewsid.clone() else { continue };
            let created = {
                let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
                self.client.lock().await.create_folder(&dest, &node.name).await
            };
            match created {
                Ok(folder) => {
                    if let Some(drivewsid) = folder.drivewsid {
                        self.db.lock().expect("db").bind_remote(
                            node.ino,
                            &drivewsid,
                            folder.docwsid.as_deref().unwrap_or(""),
                            &folder.etag,
                            &dest,
                        )?;
                    } else {
                        self.db.lock().expect("db").clear_dirty(node.ino)?;
                    }
                }
                Err(err) => {
                    self.db.lock().expect("db").raise(
                        AttentionKind::StalledUpload,
                        Some(node.ino),
                        &path,
                        &err.to_string(),
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn place_remote(&self, node: &Node) -> Result<()> {
        let Some(drivewsid) = node.drivewsid.clone() else {
            return Ok(());
        };
        let parent = self.db.lock().expect("db").node(node.parent)?;
        let parent_id = parent.drivewsid.clone().unwrap_or_default();
        if !node.name_apple.is_empty() && node.name != node.name_apple {
            let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
            self.client
                .lock()
                .await
                .rename_item(&drivewsid, &node.etag, &node.name)
                .await?;
            self.db.lock().expect("db").set_name_apple(node.ino, &node.name)?;
        }
        if let Some(old_parent) = node.remote_parent.clone() {
            if !parent_id.is_empty() && old_parent != parent_id {
                let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
                self.client
                    .lock()
                    .await
                    .move_item(&drivewsid, &node.etag, &parent_id)
                    .await?;
                self.db.lock().expect("db").set_remote_parent(node.ino, &parent_id)?;
            }
        }
        Ok(())
    }

    async fn flush_trash(&self) -> Result<()> {
        if !self.allow_deletion() {
            return Ok(());
        }
        let queued = self.db.lock().expect("db").take_trash()?;
        for (drivewsid, etag) in queued {
            let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
            if let Err(err) = self.client.lock().await.trash_item(&drivewsid, &etag).await {
                self.db.lock().expect("db").requeue_trash(&drivewsid, &etag)?;
                self.db.lock().expect("db").raise(
                    AttentionKind::StalledUpload,
                    None,
                    &drivewsid,
                    &format!("delete failed: {err}"),
                )?;
            }
        }
        Ok(())
    }

    fn note_auth(&self, step: &AuthStep) {
        let db = self.db.lock().expect("db");
        match step {
            AuthStep::SignedIn => {
                let _ = db.dismiss_kind(AttentionKind::NeedsAuth);
                let _ = db.dismiss_kind(AttentionKind::NeedsTerms);
                let _ = db.dismiss_kind(AttentionKind::NeedsAdp);
                self.wake.notify_waiters();
            }
            AuthStep::NeedsCode => {
                let _ = db.raise(AttentionKind::NeedsAuth, None, "/", "Enter the Apple verification code");
            }
            AuthStep::NeedsTerms => {
                let _ = db.raise(AttentionKind::NeedsTerms, None, "/", "Apple updated the iCloud terms");
            }
            AuthStep::NeedsDeviceApproval => {
                let _ = db.raise(
                    AttentionKind::NeedsAdp,
                    None,
                    "/",
                    "Approve iCloud Drive on your iPhone",
                );
            }
            AuthStep::Failed(message) => {
                let _ = db.raise(AttentionKind::NeedsAuth, None, "/", message);
            }
        }
    }

    fn report_index(&self, path: &str, total_files: Option<u64>) {
        let files = self.db.lock().expect("db").indexed_file_count().unwrap_or(0);
        self.set_status(Phase::Indexing, "Indexing iCloud Drive", path, 0, None, files, total_files);
    }

    fn note_directory(&self, ino: i64) {
        let listener = self.dir_changed.lock().expect("dir").clone();
        if let Some(listener) = listener {
            listener(ino);
        }
    }

    fn set_status(
        &self,
        phase: Phase,
        label: &str,
        path: &str,
        bytes_done: u64,
        bytes_total: Option<u64>,
        items_done: u64,
        items_total: Option<u64>,
    ) {
        *self.status.lock().expect("status") = SyncStatus {
            phase,
            label: label.to_string(),
            current_path: path.to_string(),
            bytes_done,
            bytes_total,
            items_done,
            items_total,
            attention: 0,
        };
    }
}

fn clean_name(name: &str) -> Result<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(EngineError::InvalidName);
    }
    Ok(name.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
