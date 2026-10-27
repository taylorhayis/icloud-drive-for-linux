use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use icloud_engine::{Engine, EngineError, FetchReason, Node, NodeKind};

const TTL: Duration = Duration::from_secs(1);
const TRACKER_INO: u64 = u64::MAX - 1;

pub fn mount_blocking(engine: Arc<Engine>, mount_point: &std::path::Path, rt: tokio::runtime::Handle) -> std::io::Result<()> {
    std::fs::create_dir_all(mount_point)?;
    let options = vec![
        MountOption::FSName("icloud-drive".into()),
        MountOption::Subtype("icloud".into()),
        MountOption::DefaultPermissions,
    ];
    fuser::mount2(
        IcloudFs { engine, rt },
        mount_point,
        &options,
    )
    .map_err(|err| std::io::Error::other(err))
}

struct IcloudFs {
    engine: Arc<Engine>,
    rt: tokio::runtime::Handle,
}

impl IcloudFs {
    fn node(&self, ino: u64) -> icloud_engine::Result<Node> {
        self.engine.getattr(ino as i64)
    }

    fn attr_of(&self, node: &Node, req: &Request<'_>) -> FileAttr {
        file_attr(node.ino as u64, node.size, node.kind, node.mtime_ms, req)
    }
}

impl Filesystem for IcloudFs {
    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if parent == 1 && name == ".trackerignore" {
            reply.entry(&TTL, &synthetic_attr(req), 0);
            return;
        }
        match self.engine.lookup(parent as i64, name) {
            Ok(Some(node)) => reply.entry(&TTL, &self.attr_of(&node, req), 0),
            Ok(None) => reply.error(libc::ENOENT),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == TRACKER_INO {
            reply.attr(&TTL, &synthetic_attr(req));
            return;
        }
        match self.node(ino) {
            Ok(node) => reply.attr(&TTL, &self.attr_of(&node, req)),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if ino == TRACKER_INO {
            reply.attr(&TTL, &synthetic_attr(req));
            return;
        }
        if self.engine.chmod_ok(ino as i64).is_err() {
            reply.error(libc::ENOENT);
            return;
        }
        if let Some(size) = size {
            if let Err(err) = self.engine.truncate(ino as i64, size) {
                reply.error(errno(&err));
                return;
            }
        }
        match self.node(ino) {
            Ok(node) => reply.attr(&TTL, &self.attr_of(&node, req)),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn mkdir(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.engine.mkdir(parent as i64, name) {
            Ok(node) => reply.entry(&TTL, &self.attr_of(&node, req), 0),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if name == ".trackerignore" {
            reply.error(libc::EPERM);
            return;
        }
        match self.engine.unlink(parent as i64, name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.engine.rmdir(parent as i64, name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.engine.rename(parent as i64, name, newparent as i64, newname) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if ino == TRACKER_INO {
            reply.opened(0, 0);
            return;
        }
        let engine = Arc::clone(&self.engine);
        let opened = self.rt.block_on(engine.ensure_content(ino as i64, FetchReason::Open));
        match opened {
            Ok(_) => reply.opened(0, 0),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        if ino == TRACKER_INO {
            reply.data(&[]);
            return;
        }
        let node = match self.node(ino) {
            Ok(node) => node,
            Err(err) => {
                reply.error(errno(&err));
                return;
            }
        };
        let Some(path) = node.blob else {
            reply.error(libc::EIO);
            return;
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) => {
                reply.error(err.raw_os_error().unwrap_or(libc::EIO));
                return;
            }
        };
        use std::io::{Read, Seek, SeekFrom};
        let mut file = file;
        if file.seek(SeekFrom::Start(offset.max(0) as u64)).is_err() {
            reply.error(libc::EIO);
            return;
        }
        let mut buf = vec![0u8; size as usize];
        match file.read(&mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(err) => reply.error(err.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn write(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
        match self.engine.write_at(ino as i64, offset.max(0) as u64, data) {
            Ok(n) => reply.written(n),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn create(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.engine.create_file(parent as i64, name) {
            Ok(node) => reply.created(&TTL, &self.attr_of(&node, req), 0, 0, 0),
            Err(err) => reply.error(errno(&err)),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let dotdot = if ino == 1 {
            1
        } else {
            self.engine.getattr(ino as i64).map(|node| node.parent.max(1) as u64).unwrap_or(1)
        };
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (dotdot, FileType::Directory, "..".to_string()),
        ];
        match self.engine.readdir(ino as i64) {
            Ok(children) => {
                for child in children {
                    let kind = if child.kind == NodeKind::Directory {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    entries.push((child.ino as u64, kind, child.name));
                }
            }
            Err(err) => {
                reply.error(errno(&err));
                return;
            }
        }
        if ino == 1 && entries.iter().all(|(_, _, name)| name != ".trackerignore") {
            entries.push((TRACKER_INO, FileType::RegularFile, ".trackerignore".into()));
        }
        for (index, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (index as i64) + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

fn file_attr(ino: u64, size: u64, kind: NodeKind, mtime_ms: i64, req: &Request<'_>) -> FileAttr {
    let kind = if kind == NodeKind::Directory {
        FileType::Directory
    } else {
        FileType::RegularFile
    };
    let time = UNIX_EPOCH + Duration::from_millis(mtime_ms.max(0) as u64);
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind,
        perm: if kind == FileType::Directory { 0o755 } else { 0o644 },
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid: req.uid(),
        gid: req.gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn synthetic_attr(req: &Request<'_>) -> FileAttr {
    file_attr(TRACKER_INO, 0, NodeKind::File, 0, req)
}

fn errno(err: &EngineError) -> i32 {
    match err {
        EngineError::NotFound => libc::ENOENT,
        EngineError::PermissionDenied => libc::EPERM,
        EngineError::AlreadyExists => libc::EEXIST,
        EngineError::NotEmpty => libc::ENOTEMPTY,
        EngineError::IsDirectory => libc::EISDIR,
        EngineError::NotADirectory => libc::ENOTDIR,
        EngineError::InvalidName => libc::EINVAL,
        EngineError::Cancelled => libc::EINTR,
        _ => libc::EIO,
    }
}
