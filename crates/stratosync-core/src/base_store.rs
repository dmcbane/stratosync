/// Content-addressed object store for base versions (3-way merge support).
///
/// Stores file content by SHA-256 hash under `.bases/objects/{hash[..2]}/{hash[2..]}`,
/// following git's object layout convention.
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Default text file extensions eligible for 3-way merge.
pub const DEFAULT_TEXT_EXTENSIONS: &[&str] = &[
    "md", "txt", "rs", "py", "toml", "yaml", "yml", "json", "xml", "html",
    "css", "js", "ts", "sh", "cfg", "ini", "conf", "csv", "tex",
    "c", "cpp", "h", "hpp", "go", "java", "rb", "pl", "lua", "sql",
    "dockerfile", "makefile", "cmake", "gitignore", "env", "properties",
];

pub struct BaseStore {
    base_dir: PathBuf,
}

impl BaseStore {
    /// Create a new BaseStore rooted at `base_dir`.
    /// Creates the `objects/` subdirectory if it doesn't exist.
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let objects_dir = base_dir.join("objects");
        std::fs::create_dir_all(&objects_dir)
            .with_context(|| format!("create base store at {objects_dir:?}"))?;
        Ok(Self { base_dir })
    }

    /// Hash the file at `content_path`, store it as a blob, and return the hex hash.
    ///
    /// If a blob with the same hash already exists, this is a no-op (deduplication).
    /// Uses write-to-temp-then-rename for atomicity.
    pub fn store_base(&self, content_path: &Path) -> Result<String> {
        let mut file = std::fs::File::open(content_path)
            .with_context(|| format!("open file for base snapshot: {content_path:?}"))?;

        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        let hash = format!("{:x}", hasher.finalize());

        let blob_path = self.object_path(&hash);
        if blob_path.exists() {
            return Ok(hash);
        }

        // Ensure the 2-char prefix directory exists
        if let Some(parent) = blob_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write to temp file then rename for atomicity
        let tmp_path = blob_path.with_extension("tmp");
        std::fs::copy(content_path, &tmp_path)
            .with_context(|| format!("copy base blob to {tmp_path:?}"))?;
        std::fs::rename(&tmp_path, &blob_path)
            .with_context(|| format!("rename base blob {tmp_path:?} -> {blob_path:?}"))?;

        Ok(hash)
    }

    /// Return the path where a blob with the given hash would be stored.
    /// Caller should check `.exists()` before using.
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = hash.split_at(2.min(hash.len()));
        self.base_dir.join("objects").join(prefix).join(rest)
    }

    /// Delete a blob by hash. Ignores ENOENT (already deleted).
    pub fn remove_object(&self, hash: &str) -> Result<()> {
        let path = self.object_path(hash);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove base object {path:?}")),
        }
    }

    /// Check whether a file is eligible for base-version storage and 3-way merge.
    ///
    /// Criteria:
    /// 1. File size <= max_size
    /// 2. File extension is in the text extension allowlist
    /// 3. First 512 bytes contain no NUL bytes (binary indicator)
    pub fn is_text_mergeable(
        path:     &Path,
        size:     u64,
        max_size: u64,
        text_exts: &[String],
    ) -> bool {
        if size > max_size {
            return false;
        }

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => {
                // Extensionless files: check filename itself (e.g. "Makefile", "Dockerfile")
                let name = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if text_exts.iter().any(|e| e.to_lowercase() == name) {
                    // Filename matches an entry in the list; continue to NUL check
                    name
                } else {
                    return false;
                }
            }
        };

        if !text_exts.iter().any(|e| e.to_lowercase() == ext) {
            return false;
        }

        // NUL-byte check on first 512 bytes
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut buf = vec![0u8; 512];
            if let Ok(n) = f.read(&mut buf) {
                if buf[..n].contains(&0) {
                    return false;
                }
            }
        }
        // If we can't read the file, let the caller handle it downstream

        true
    }

    /// Return the base directory path (for diagnostics/GC).
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_base_store() -> (tempfile::TempDir, BaseStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BaseStore::new(dir.path().join(".bases")).unwrap();
        (dir, store)
    }

    fn write_temp_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    #[test]
    fn store_and_retrieve_round_trip() {
        let (dir, store) = temp_base_store();
        let file = write_temp_file(dir.path(), "hello.txt", b"hello world\n");

        let hash = store.store_base(&file).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex

        let blob_path = store.object_path(&hash);
        assert!(blob_path.exists());

        let content = std::fs::read(&blob_path).unwrap();
        assert_eq!(content, b"hello world\n");
    }

    #[test]
    fn deduplication() {
        let (dir, store) = temp_base_store();
        let file1 = write_temp_file(dir.path(), "a.txt", b"same content");
        let file2 = write_temp_file(dir.path(), "b.txt", b"same content");

        let hash1 = store.store_base(&file1).unwrap();
        let hash2 = store.store_base(&file2).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn different_content_different_hash() {
        let (dir, store) = temp_base_store();
        let file1 = write_temp_file(dir.path(), "a.txt", b"content A");
        let file2 = write_temp_file(dir.path(), "b.txt", b"content B");

        let hash1 = store.store_base(&file1).unwrap();
        let hash2 = store.store_base(&file2).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn remove_object_existing() {
        let (dir, store) = temp_base_store();
        let file = write_temp_file(dir.path(), "rm.txt", b"remove me");

        let hash = store.store_base(&file).unwrap();
        assert!(store.object_path(&hash).exists());

        store.remove_object(&hash).unwrap();
        assert!(!store.object_path(&hash).exists());
    }

    #[test]
    fn remove_object_nonexistent() {
        let (_dir, store) = temp_base_store();
        // Should not error
        store.remove_object("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
    }

    #[test]
    fn is_text_mergeable_size_gate() {
        let (dir, _store) = temp_base_store();
        let file = write_temp_file(dir.path(), "big.txt", b"hello");
        let exts = vec!["txt".to_string()];

        assert!(BaseStore::is_text_mergeable(&file, 5, 100, &exts));
        assert!(!BaseStore::is_text_mergeable(&file, 5, 4, &exts)); // size > max_size
    }

    #[test]
    fn is_text_mergeable_extension_check() {
        let (dir, _store) = temp_base_store();
        let txt = write_temp_file(dir.path(), "file.txt", b"text");
        let bin = write_temp_file(dir.path(), "file.exe", b"binary");
        let exts = vec!["txt".to_string(), "md".to_string()];

        assert!(BaseStore::is_text_mergeable(&txt, 4, 1000, &exts));
        assert!(!BaseStore::is_text_mergeable(&bin, 6, 1000, &exts));
    }

    #[test]
    fn is_text_mergeable_binary_detection() {
        let (dir, _store) = temp_base_store();
        let mut content = b"looks like text but\x00has null".to_vec();
        let file = write_temp_file(dir.path(), "tricky.txt", &content);
        let exts = vec!["txt".to_string()];

        assert!(!BaseStore::is_text_mergeable(&file, content.len() as u64, 1000, &exts));

        // Genuinely text file should pass
        content = b"all text no nulls".to_vec();
        let file2 = write_temp_file(dir.path(), "clean.txt", &content);
        assert!(BaseStore::is_text_mergeable(&file2, content.len() as u64, 1000, &exts));
    }

    #[test]
    fn is_text_mergeable_case_insensitive_extension() {
        let (dir, _store) = temp_base_store();
        let file = write_temp_file(dir.path(), "README.TXT", b"hello");
        let exts = vec!["txt".to_string()];

        assert!(BaseStore::is_text_mergeable(&file, 5, 1000, &exts));
    }

    #[test]
    fn is_text_mergeable_extensionless_filename() {
        let (dir, _store) = temp_base_store();
        let file = write_temp_file(dir.path(), "Makefile", b"all: build\n");
        let exts = vec!["makefile".to_string(), "txt".to_string()];

        assert!(BaseStore::is_text_mergeable(&file, 11, 1000, &exts));
    }

    #[test]
    fn object_path_layout() {
        let (_dir, store) = temp_base_store();
        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let path = store.object_path(hash);

        // Should be .bases/objects/ab/cdef...
        assert!(path.to_str().unwrap().contains("objects/ab/cdef"));
    }
}
