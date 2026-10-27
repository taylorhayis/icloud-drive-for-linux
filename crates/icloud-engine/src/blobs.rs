use std::path::{Path, PathBuf};

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for(&self, ino: i64) -> PathBuf {
        self.root.join(format!("{ino}.blob"))
    }

    pub fn temp_for(&self, ino: i64) -> PathBuf {
        self.root.join(format!("{ino}.partial"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
