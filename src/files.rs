//! Jailed file directory: every read and write is confined to `SLACK_FILES_DIR`.

use std::path::{Component, Path, PathBuf};

/// Failures resolving a path inside the jail.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("path `{0}` resolves outside SLACK_FILES_DIR")]
    Escape(String),
    #[error("no such file: {0}")]
    NotFound(String),
    #[error("not a regular file: {0}")]
    NotAFile(String),
    #[error("destination `{0}` already exists; pass overwrite=true to replace it")]
    Exists(String),
    #[error("invalid path: {0}")]
    Invalid(String),
    #[error("io error on `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> FileError {
    FileError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// A canonicalized directory that scopes all file reads and writes.
#[derive(Clone, Debug)]
pub struct FileJail {
    root: PathBuf,
}

/// Resolves `.` and `..` without touching the filesystem.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl FileJail {
    /// The directory must exist; it is canonicalized once here.
    pub fn new(dir: &Path) -> Result<Self, FileError> {
        let root = std::fs::canonicalize(dir).map_err(|e| io_err(dir, e))?;
        if !root.is_dir() {
            return Err(FileError::Invalid(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn join(&self, input: &str) -> Result<PathBuf, FileError> {
        if input.trim().is_empty() || input.contains('\0') {
            return Err(FileError::Invalid("path is empty or contains NUL".into()));
        }
        let p = Path::new(input);
        Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        })
    }

    /// Existing regular file that canonicalizes inside the root (symlinks followed, then checked).
    pub fn resolve_read(&self, input: &str) -> Result<PathBuf, FileError> {
        let joined = self.join(input)?;
        let canon = std::fs::canonicalize(&joined).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FileError::NotFound(input.to_string())
            } else {
                io_err(&joined, e)
            }
        })?;
        if !canon.starts_with(&self.root) {
            return Err(FileError::Escape(input.to_string()));
        }
        if !canon.is_file() {
            return Err(FileError::NotAFile(input.to_string()));
        }
        Ok(canon)
    }

    /// Destination inside the root that must not exist yet. Missing parent directories are
    /// created one level at a time, re-checking containment; dot-prefixed components are refused.
    pub fn resolve_write(&self, input: &str) -> Result<PathBuf, FileError> {
        self.resolve_dest(input, false)
    }

    /// Like `resolve_write`, but an existing regular file at the target is allowed.
    pub fn resolve_write_overwrite(&self, input: &str) -> Result<PathBuf, FileError> {
        self.resolve_dest(input, true)
    }

    fn resolve_dest(&self, input: &str, overwrite: bool) -> Result<PathBuf, FileError> {
        let lexical = lexical_normalize(&self.join(input)?);
        if !lexical.starts_with(&self.root) {
            return Err(FileError::Escape(input.to_string()));
        }
        let rel_all = lexical.strip_prefix(&self.root).unwrap_or(&lexical);
        if rel_all
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            return Err(FileError::Invalid(
                "destination components must not start with `.`".into(),
            ));
        }
        let name = lexical
            .file_name()
            .ok_or_else(|| FileError::Invalid("destination has no file name".into()))?
            .to_owned();
        let parent = lexical.parent().unwrap_or(&self.root);
        let rel = parent
            .strip_prefix(&self.root)
            .map_err(|_| FileError::Escape(input.to_string()))?;
        let mut dir = self.root.clone();
        for comp in rel.components() {
            let next = dir.join(comp.as_os_str());
            if std::fs::symlink_metadata(&next).is_err() {
                std::fs::create_dir(&next).map_err(|e| io_err(&next, e))?;
            }
            dir = std::fs::canonicalize(&next).map_err(|e| io_err(&next, e))?;
            if !dir.starts_with(&self.root) {
                return Err(FileError::Escape(input.to_string()));
            }
        }
        let target = dir.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(FileError::Escape(input.to_string()));
            }
            if meta.is_dir() {
                return Err(FileError::NotAFile(input.to_string()));
            }
            if !overwrite {
                return Err(FileError::Exists(input.to_string()));
            }
        }
        Ok(target)
    }
}

/// Reduces a remote file name to `[A-Za-z0-9._-]`, at most 100 chars, never dot-leading.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    let trimmed = cleaned.trim_start_matches('.');
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Filename sent to Slack: keeps Unicode, strips path separators and control characters.
pub fn clean_remote_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(255)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "slack-mcp-files-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).expect("mkdir");
        std::fs::canonicalize(p).expect("canon")
    }

    #[test]
    fn files_dir_rejects_path_escape() {
        let base = scratch();
        let root = base.join("jail");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(base.join("x"), b"secret").expect("write");
        std::fs::write(outside.join("f"), b"secret").expect("write");
        std::fs::write(root.join("ok.txt"), b"fine").expect("write");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("f"), root.join("link_file")).expect("ln");
            std::os::unix::fs::symlink(&outside, root.join("link_dir")).expect("ln");
        }
        let jail = FileJail::new(&root).expect("jail");

        assert!(jail.resolve_read("ok.txt").is_ok());
        assert!(jail.resolve_write("new/dir/file.bin").is_ok());

        let abs_outside = outside.join("f").display().to_string();
        assert!(matches!(
            jail.resolve_read("../x"),
            Err(FileError::Escape(_))
        ));
        assert!(matches!(
            jail.resolve_read(&abs_outside),
            Err(FileError::Escape(_))
        ));
        assert!(matches!(
            jail.resolve_write("../x"),
            Err(FileError::Escape(_))
        ));
        assert!(matches!(
            jail.resolve_write(&abs_outside),
            Err(FileError::Escape(_))
        ));
        assert!(matches!(
            jail.resolve_write("a/../../x"),
            Err(FileError::Escape(_))
        ));

        #[cfg(unix)]
        {
            assert!(matches!(
                jail.resolve_read("link_file"),
                Err(FileError::Escape(_))
            ));
            assert!(matches!(
                jail.resolve_read("link_dir/f"),
                Err(FileError::Escape(_))
            ));
            assert!(matches!(
                jail.resolve_write("link_file"),
                Err(FileError::Escape(_))
            ));
            assert!(matches!(
                jail.resolve_write("link_dir/new"),
                Err(FileError::Escape(_))
            ));
        }
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn write_refuses_existing_and_dotfiles() {
        let root = scratch();
        std::fs::write(root.join("have.txt"), b"x").expect("write");
        let jail = FileJail::new(&root).expect("jail");
        assert!(matches!(
            jail.resolve_write("have.txt"),
            Err(FileError::Exists(_))
        ));
        assert!(jail.resolve_write_overwrite("have.txt").is_ok());
        assert!(matches!(
            jail.resolve_write(".hidden"),
            Err(FileError::Invalid(_))
        ));
        assert!(matches!(
            jail.resolve_write("a/.b/c"),
            Err(FileError::Invalid(_))
        ));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn remote_names_keep_unicode() {
        assert_eq!(
            clean_remote_filename(" rapport été.pdf "),
            "rapport été.pdf"
        );
        assert_eq!(clean_remote_filename("a/b\\c\0d.txt"), "abcd.txt");
        assert_eq!(clean_remote_filename("  "), "file");
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename("report 1.pdf"), "report_1.pdf");
    }
}
