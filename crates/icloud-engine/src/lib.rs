//! Local index, pin rules, and the sync decisions that must not lose data.

mod blobs;
mod db;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use icloud_proto::{AuthStep, IcloudClient, ProtoError, RemoteKind, RemoteNode, SavedSession, ROOT_DRIVEWSID};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

pub use db::{ActivityRecord, AttentionKind, Node, NodeKind};

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
    /// Real folder in the home directory. Pinned and local files live here, so
    /// they are still there when the service is not running.
    library: PathBuf,
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
        library: &Path,
        client: IcloudClient,
        allow_deletion: bool,
        poll_seconds: u32,
    ) -> Result<Self> {
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(blobs)?;
        std::fs::create_dir_all(library)?;
        let db = db::Db::open(database)?;
        Ok(Self {
            db: Mutex::new(db),
            blobs: blobs::BlobStore::new(blobs),
            library: library.to_path_buf(),
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
        self.adopt_library()?;
        self.clear_empty_stubs()?;
        let mut queue = VecDeque::from([ROOT_DRIVEWSID.to_string()]);
        let mut seen = std::collections::HashSet::from([ROOT_DRIVEWSID.to_string()]);
        let mut failed_downloads = std::collections::HashSet::new();
        let mut total_files: Option<u64> = None;
        loop {
            if self.shutdown.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let skipped: Vec<i64> = failed_downloads.iter().copied().collect();
            let pending = self.db.lock().expect("db").next_incomplete_pin(&skipped)?;
            if pending.is_none() && queue.is_empty() {
                break;
            }
            if let Some(node) = pending {
                if self.ensure_content(node.ino, FetchReason::Pin).await.is_err() {
                    failed_downloads.insert(node.ino);
                }
            }
            let Some(drivewsid) = queue.pop_front() else {
                continue;
            };
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
                    if let Some(doc) = listing.docwsid.filter(|doc| !doc.is_empty() && doc != "root") {
                        self.db.lock().expect("db").set_docwsid(&drivewsid, &doc)?;
                    }
                    let (_folders, changed) = self.ingest_listing(parent, started, &listing.children)?;
                    self.materialize_synced(parent, &listing.children)?;
                    let roots = self.db.lock().expect("db").sync_folders()?;
                    let whole_drive = roots.iter().any(|root| normalize_drive_path(root) == "/");
                    if whole_drive {
                        if drivewsid == ROOT_DRIVEWSID {
                            total_files = listing.file_count.or(total_files);
                        }
                    } else {
                        total_files = None;
                    }
                    // Only the top-level names are listed until a folder is chosen.
                    // A chosen folder, and everything inside it, is listed after that.
                    for child in &listing.children {
                        if child.kind != RemoteKind::Folder {
                            continue;
                        }
                        let child_path = join_drive_path(&path, &child.name_local);
                        if !path_is_synced(&child_path, &roots) {
                            continue;
                        }
                        if seen.insert(child.drivewsid.clone()) {
                            queue.push_back(child.drivewsid.clone());
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
        self.adopt_library()?;
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
        if local_copy_is_ready(&node) {
            return Ok(PathBuf::from(node.blob.clone().unwrap_or_default()));
        }
        let Some(docwsid) = node.docwsid.clone().filter(|id| !id.is_empty()) else {
            if node.dirty {
                return self.write_empty_kept_file(ino);
            }
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
            Ok(written) if node.size == 0 || written == node.size => {
                let final_path = self.publish_kept_file(ino, &temp, written)?;
                self.db.lock().expect("db").dismiss_path(AttentionKind::StalledDownload, &path)?;
                self.note_activity("downloaded", &path, &format_bytes(written));
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
                self.note_activity("download_failed", &path, &format!("stopped at {}", format_bytes(written)));
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
                self.note_activity("download_failed", &path, &err.to_string());
                self.set_status(Phase::Error, &err.to_string(), &path, 0, Some(node.size), 0, None);
                Err(err.into())
            }
        }
    }

    pub fn sync_folders(&self) -> Result<Vec<String>> {
        self.db.lock().expect("db").sync_folders()
    }

    /// Download every file under `path` into the home folder, or stop doing that.
    /// `path` of `/` is the whole drive. Files land on disk only after the download finishes.
    pub fn set_sync_folder(&self, path: &str, enabled: bool) -> Result<()> {
        let path = normalize_drive_path(path);
        if enabled {
            let ino = if path == "/" {
                std::fs::create_dir_all(&self.library)?;
                db::ROOT_INO
            } else {
                let node = self.resolve(&path)?;
                if node.kind != NodeKind::Directory {
                    return Err(EngineError::NotADirectory);
                }
                if let Ok(dir) = self.library_file(&path) {
                    std::fs::create_dir_all(dir)?;
                }
                node.ino
            };
            self.db.lock().expect("db").add_sync_folder(&path)?;
            self.db.lock().expect("db").set_pin_tree(ino, true)?;
        } else {
            self.db.lock().expect("db").remove_sync_folder(&path)?;
            let ino = if path == "/" {
                Some(db::ROOT_INO)
            } else {
                match self.db.lock().expect("db").resolve(&path) {
                    Ok(ino) => Some(ino),
                    Err(EngineError::NotFound) => None,
                    Err(err) => return Err(err),
                }
            };
            let Some(ino) = ino else {
                self.wake.notify_waiters();
                return Ok(());
            };
            self.db.lock().expect("db").set_pin_tree(ino, false)?;
            for root in self.db.lock().expect("db").sync_folders()? {
                let root_ino = if root == "/" {
                    db::ROOT_INO
                } else {
                    match self.db.lock().expect("db").resolve(&root) {
                        Ok(ino) => ino,
                        Err(EngineError::NotFound) => continue,
                        Err(err) => return Err(err),
                    }
                };
                self.db.lock().expect("db").set_pin_tree(root_ino, true)?;
            }
            self.evict_unpinned_subtree(ino)?;
            if path == "/" {
                prune_empty_dirs(&self.library, &self.library);
            } else if let Ok(dir) = self.library_file(&path) {
                prune_empty_dirs(&dir, &self.library);
            }
        }
        let label = if path == "/" { "Entire iCloud Drive" } else { path.as_str() };
        self.note_activity(if enabled { "sync_on" } else { "sync_off" }, label, "");
        self.wake.notify_waiters();
        Ok(())
    }

    pub fn activity(&self) -> Result<Vec<ActivityRecord>> {
        self.db.lock().expect("db").recent_activity(200)
    }

    fn note_activity(&self, kind: &str, path: &str, detail: &str) {
        let _ = self.db.lock().expect("db").record_activity(kind, path, detail);
    }

    pub fn set_pin(&self, path: &str, pin: bool) -> Result<()> {
        let path = normalize_drive_path(path);
        let node = self.resolve(&path)?;
        if pin && node.kind == NodeKind::Directory {
            return self.set_sync_folder(&path, true);
        }
        if !pin && node.kind == NodeKind::Directory {
            return self.set_sync_folder(&path, false);
        }
        self.db.lock().expect("db").set_pin_tree(node.ino, pin)?;
        if pin {
            self.wake.notify_waiters();
        } else {
            self.evict_unpinned_subtree(node.ino)?;
        }
        Ok(())
    }

    /// Drop local copies under `ino` that are no longer pinned and have no edits.
    /// Names stay in the search index.
    fn evict_unpinned_subtree(&self, ino: i64) -> Result<()> {
        let nodes = self.db.lock().expect("db").unpinned_local_files(ino)?;
        for node in nodes {
            if let Some(blob) = node.blob.as_deref() {
                self.remove_kept_file(Path::new(blob));
            }
            if let Ok(drive_path) = self.db.lock().expect("db").path_of(node.ino) {
                if let Ok(kept) = self.library_file(&drive_path) {
                    self.remove_kept_file(&kept);
                }
            }
            let _ = std::fs::remove_file(self.blobs.path_for(node.ino));
            let _ = std::fs::remove_file(self.blobs.temp_for(node.ino));
            self.db.lock().expect("db").forget_local_bytes(node.ino)?;
        }
        Ok(())
    }

    /// Remove pinned zero-byte stand-ins so a photo viewer cannot open them, and download the real file.
    fn clear_empty_stubs(&self) -> Result<()> {
        let stubs = self.db.lock().expect("db").empty_stubs()?;
        for node in stubs {
            let empty = node.blob.as_deref().and_then(|path| std::fs::metadata(path).ok()).is_some_and(|meta| meta.is_file() && meta.len() == 0);
            if empty {
                if let Some(blob) = node.blob.as_deref() {
                    self.remove_kept_file(Path::new(blob));
                }
                self.db.lock().expect("db").forget_local_bytes(node.ino)?;
            }
        }
        Ok(())
    }

    /// Pin files, and create directories, for children that sit inside a chosen sync folder.
    fn materialize_synced(&self, parent: i64, children: &[RemoteNode]) -> Result<()> {
        let roots = self.db.lock().expect("db").sync_folders()?;
        if roots.is_empty() {
            return Ok(());
        }
        let parent_path = self.db.lock().expect("db").path_of(parent).unwrap_or_else(|_| "/".into());
        for child in children {
            let path = join_drive_path(&parent_path, &child.name_local);
            if !path_is_synced(&path, &roots) {
                continue;
            }
            if child.kind == RemoteKind::Folder {
                if let Ok(dir) = self.library_file(&path) {
                    let _ = std::fs::create_dir_all(dir);
                }
                continue;
            }
            let Some(node) = self.db.lock().expect("db").child(parent, &child.name_local)? else {
                continue;
            };
            if !node.pin {
                self.db.lock().expect("db").set_pinned_flag(node.ino, true)?;
            }
        }
        Ok(())
    }

    /// Path of a kept file inside the home folder. The drive path is `/Photos/a.jpg`.
    fn library_file(&self, drive_path: &str) -> Result<PathBuf> {
        let mut out = self.library.clone();
        let trimmed = drive_path.trim_matches('/');
        if trimmed.is_empty() {
            return Err(EngineError::InvalidName);
        }
        for part in trimmed.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return Err(EngineError::InvalidName);
            }
            out.push(part);
        }
        Ok(out)
    }

    fn write_empty_kept_file(&self, ino: i64) -> Result<PathBuf> {
        let drive_path = self.db.lock().expect("db").path_of(ino)?;
        let path = self.library_file(&drive_path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, [])?;
        self.db.lock().expect("db").mark_complete(ino, &path, 0)?;
        Ok(path)
    }

    /// Move a downloaded or cached file into the home folder and remember that path.
    fn publish_kept_file(&self, ino: i64, source: &Path, size: u64) -> Result<PathBuf> {
        let drive_path = self.db.lock().expect("db").path_of(ino)?;
        let dest = self.library_file(&drive_path)?;
        if source != dest.as_path() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::rename(source, &dest).is_err() {
                std::fs::copy(source, &dest)?;
                let _ = std::fs::remove_file(source);
            }
        }
        self.db.lock().expect("db").mark_complete(ino, &dest, size)?;
        Ok(dest)
    }

    fn remove_kept_file(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
        if !path.starts_with(&self.library) {
            return;
        }
        let mut dir = path.parent();
        while let Some(current) = dir {
            if current == self.library {
                break;
            }
            if std::fs::remove_dir(current).is_err() {
                break;
            }
            dir = current.parent();
        }
    }

    /// Notice files the user added or edited in the home folder while the service was away.
    fn adopt_library(&self) -> Result<()> {
        if directory_is_mounted(&self.library) {
            return Ok(());
        }
        let mut dirs = vec![self.library.clone()];
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(&self.library) else {
                    continue;
                };
                let drive = format!("/{}", rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
                let len = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                let resolved = self.db.lock().expect("db").resolve(&drive);
                match resolved {
                    Ok(ino) => {
                        let node = self.db.lock().expect("db").node(ino)?;
                        if node.kind != NodeKind::File {
                            continue;
                        }
                        let path_str = path.to_string_lossy();
                        let same_place = node.blob.as_deref() == Some(path_str.as_ref());
                        if same_place && node.size == len && node.complete {
                            continue;
                        }
                        if len == 0 && !node.dirty && node.size == 0 && node.docwsid.as_ref().is_some_and(|id| !id.is_empty()) {
                            continue;
                        }
                        self.db.lock().expect("db").mark_complete(ino, &path, len)?;
                        if node.size != len || !node.complete {
                            self.db.lock().expect("db").mark_dirty(ino)?;
                            self.note_activity("local", &drive, "Edited on this computer");
                        }
                    }
                    Err(EngineError::NotFound) => {
                        let parent_path = match drive.rfind('/') {
                            Some(0) | None => "/",
                            Some(index) => &drive[..index],
                        };
                        let parent = self.ensure_dir_chain(parent_path)?;
                        let ino = self.db.lock().expect("db").create(parent, name, NodeKind::File)?;
                        self.db.lock().expect("db").mark_complete(ino, &path, len)?;
                        self.db.lock().expect("db").mark_dirty(ino)?;
                        self.note_activity("local", &drive, "Added on this computer");
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }

    fn ensure_dir_chain(&self, drive_path: &str) -> Result<i64> {
        let trimmed = drive_path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(db::ROOT_INO);
        }
        let mut parent = db::ROOT_INO;
        for part in trimmed.split('/') {
            let part = clean_name(part)?;
            if let Some(node) = self.db.lock().expect("db").child(parent, &part)? {
                if node.kind != NodeKind::Directory {
                    return Err(EngineError::NotADirectory);
                }
                parent = node.ino;
                continue;
            }
            parent = self.db.lock().expect("db").create(parent, &part, NodeKind::Directory)?;
        }
        Ok(parent)
    }

    pub fn create_file(&self, parent: i64, name: &str) -> Result<Node> {
        let name = clean_name(name)?;
        let ino = self.db.lock().expect("db").create(parent, &name, NodeKind::File)?;
        let path = self.write_empty_kept_file(ino)?;
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
            self.write_empty_kept_file(ino)?
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
            self.write_empty_kept_file(ino)?
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
            let new_blob = self.library_file(&self.db.lock().expect("db").path_of(dst.ino)?)?;
            if let Some(parent) = new_blob.parent() {
                std::fs::create_dir_all(parent)?;
            }
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
                        let dest = self.library_file(&self.db.lock().expect("db").path_of(copy)?)?;
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

    async fn upload_dirty(&self) -> Result<()> {
        self.publish_directories().await?;
        let dirty = self.db.lock().expect("db").dirty_files()?;
        if dirty.is_empty() {
            return Ok(());
        }
        self.set_status(Phase::Uploading, "Uploading", "", 0, None, 0, Some(dirty.len() as u64));
        for node in dirty {
            let path = self.db.lock().expect("db").path_of(node.ino).unwrap_or(node.name.clone());
            if self.db.lock().expect("db").has_open(AttentionKind::Conflict, node.ino)? {
                continue;
            }
            if let Err(err) = self.place_remote(&node).await {
                self.db.lock().expect("db").raise(
                    AttentionKind::StalledUpload,
                    Some(node.ino),
                    &path,
                    &err.to_string(),
                )?;
                self.note_activity("upload_failed", &path, &err.to_string());
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
                    self.db.lock().expect("db").dismiss_path(AttentionKind::Conflict, &path)?;
                    self.note_activity("uploaded", &path, &format_bytes(node.size));
                }
                Err(err) => {
                    self.db.lock().expect("db").raise(
                        AttentionKind::StalledUpload,
                        Some(node.ino),
                        &path,
                        &err.to_string(),
                    )?;
                    self.note_activity("upload_failed", &path, &err.to_string());
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
                    self.note_activity("uploaded", &path, "Created folder");
                }
                Err(err) => {
                    self.db.lock().expect("db").raise(
                        AttentionKind::StalledUpload,
                        Some(node.ino),
                        &path,
                        &err.to_string(),
                    )?;
                    self.note_activity("upload_failed", &path, &err.to_string());
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
        if !node.name_apple.is_empty() && !remote_name_matches(&node.name_apple, &node.extension, &node.name) {
            let _permit = self.transfers.acquire().await.map_err(|_| EngineError::Cancelled)?;
            self.client
                .lock()
                .await
                .rename_item(&drivewsid, &node.etag, &node.name)
                .await?;
            self.db.lock().expect("db").set_name_apple(node.ino, &node.name)?;
            let path = self.db.lock().expect("db").path_of(node.ino).unwrap_or_else(|_| node.name.clone());
            self.note_activity("renamed", &path, &format!("Renamed from {}", node.name_apple));
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
                let path = self.db.lock().expect("db").path_of(node.ino).unwrap_or_else(|_| node.name.clone());
                self.note_activity("moved", &path, "Moved on iCloud");
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

    /// Write one folder's names without holding the database lock for the whole folder.
    /// A large folder must not freeze the mount.
    fn ingest_listing(&self, parent: i64, started: i64, children: &[RemoteNode]) -> Result<(Vec<String>, bool)> {
        let parent_drive = self.db.lock().expect("db").node(parent)?.drivewsid.unwrap_or_default();
        let mut folders = Vec::new();
        let mut changed = false;
        for child in children {
            if self.db.lock().expect("db").upsert_remote(parent, &parent_drive, child)? {
                changed = true;
            }
            if child.kind == RemoteKind::Folder {
                folders.push(child.drivewsid.clone());
            }
        }
        let seen: std::collections::HashSet<&str> = children.iter().map(|child| child.drivewsid.as_str()).collect();
        let local = self.db.lock().expect("db").children(parent)?;
        for node in local {
            let Some(drivewsid) = node.drivewsid.clone() else {
                continue;
            };
            if seen.contains(drivewsid.as_str()) || node.last_synced_ms >= started {
                continue;
            }
            let mut db = self.db.lock().expect("db");
            if node.dirty {
                let path = db.path_of(node.ino).unwrap_or_else(|_| node.name.clone());
                db.raise(
                    AttentionKind::Conflict,
                    Some(node.ino),
                    &path,
                    "This file changed here and on iCloud",
                )?;
            } else {
                db.remove_node(node.ino)?;
                changed = true;
            }
        }
        Ok((folders, changed))
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

pub fn normalize_drive_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        format!("/{trimmed}")
    }
}

pub fn path_is_synced(path: &str, roots: &[String]) -> bool {
    let path = normalize_drive_path(path);
    roots.iter().any(|root| {
        let root = normalize_drive_path(root);
        root == "/" || path == root || path.starts_with(&format!("{root}/"))
    })
}

fn join_drive_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// A file already on disk that is safe to open. An empty stand-in for a remote file is not.
fn local_copy_is_ready(node: &Node) -> bool {
    if node.kind != NodeKind::File {
        return false;
    }
    let Some(path) = node.blob.as_deref() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if node.dirty {
        return true;
    }
    if meta.len() == 0 && !node.bytes_fetched {
        return false;
    }
    if node.size == 0 {
        return node.bytes_fetched;
    }
    meta.len() == node.size
}

fn prune_empty_dirs(dir: &Path, library: &Path) {
    if !dir.starts_with(library) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_empty_dirs(&entry.path(), library);
        }
    }
    if dir != library {
        let _ = std::fs::remove_dir(dir);
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn directory_is_mounted(folder: &Path) -> bool {
    let listed = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let display = folder.display().to_string();
    let escaped = display.replace(' ', "\\040");
    listed.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|target| target == display || target == escaped)
    })
}

/// Apple keeps the extension in its own field, so `Case` plus `sc` is already `Case.sc`.
fn remote_name_matches(name_apple: &str, extension: &str, name_local: &str) -> bool {
    if name_apple == name_local {
        return true;
    }
    !extension.is_empty() && name_local == format!("{name_apple}.{extension}")
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

#[cfg(test)]
mod tests {
    use super::{local_copy_is_ready, path_is_synced, Node, NodeKind};

    #[test]
    fn apple_extension_is_not_a_local_rename() {
        assert!(super::remote_name_matches("Cyberpunk_PC_Case", "sc", "Cyberpunk_PC_Case.sc"));
        assert!(super::remote_name_matches("Notes.txt", "", "Notes.txt"));
        assert!(!super::remote_name_matches("Cyberpunk_PC_Case", "sc", "Other.sc"));
    }

    fn sample(bytes_fetched: bool, size: u64, blob: Option<&str>, dirty: bool) -> Node {
        Node {
            ino: 2,
            parent: 1,
            name: "a.jpg".into(),
            name_apple: "a.jpg".into(),
            extension: String::new(),
            remote_parent: None,
            kind: NodeKind::File,
            size,
            mtime_ms: 0,
            complete: true,
            pin: true,
            dirty,
            blob: blob.map(str::to_string),
            docwsid: Some("doc".into()),
            drivewsid: None,
            etag: String::new(),
            revision: 1,
            last_synced_ms: 0,
            bytes_fetched,
        }
    }

    #[test]
    fn sync_roots_cover_children_and_the_whole_drive() {
        let photos = vec!["/Photos".to_string()];
        assert!(path_is_synced("/Photos", &photos));
        assert!(path_is_synced("/Photos/a.jpg", &photos));
        assert!(!path_is_synced("/Notes/a.txt", &photos));
        assert!(path_is_synced("/Notes/a.txt", &["/".to_string()]));
    }

    #[test]
    fn an_empty_listing_is_not_ready_to_open() {
        let dir = std::env::temp_dir().join(format!("icloud-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jpg");
        std::fs::write(&path, []).unwrap();
        let path = path.to_string_lossy().to_string();
        assert!(!local_copy_is_ready(&sample(false, 0, Some(&path), false)));
        assert!(local_copy_is_ready(&sample(true, 0, Some(&path), false)));
        std::fs::write(&path, b"jpeg").unwrap();
        assert!(local_copy_is_ready(&sample(true, 4, Some(&path), false)));
        assert!(!local_copy_is_ready(&sample(true, 9, Some(&path), false)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
