use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::error::{Result, TruceError};

/// A git work tree discovered from a starting directory.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
}

impl GitRepo {
    /// Walk up from `start` using `git rev-parse --show-toplevel`.
    pub fn discover(start: &Path) -> Result<Self> {
        let output = git_in(start, &["rev-parse", "--show-toplevel"])?;
        let root = stdout_path(&output.stdout)?;
        if root.as_os_str().is_empty() {
            return Err(TruceError::BareRepository);
        }
        Ok(Self {
            root: canonicalize_dir(&root),
        })
    }

    /// True when a merge, rebase, cherry-pick, or revert is in progress.
    pub fn merge_in_progress(&self) -> Result<bool> {
        const NAMES: &[&str] = &[
            "MERGE_HEAD",
            "REBASE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
        ];
        for name in NAMES {
            let output = git_in(&self.root, &["rev-parse", "--git-path", name])?;
            let path = stdout_path(&output.stdout)?;
            let path = if path.is_absolute() {
                path
            } else {
                self.root.join(path)
            };
            if path.exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Unmerged index paths, relative to the repo root.
    ///
    /// Combines `git ls-files -u` (index stages) with
    /// `git diff --name-only --diff-filter=U`. Paths are de-duplicated and
    /// keep their original relative form (including spaces).
    pub fn unmerged_paths(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();

        let ls = git_in(&self.root, &["ls-files", "-z", "-u"])?;
        for record in split_nul(&ls.stdout) {
            if record.is_empty() {
                continue;
            }
            let path = parse_ls_files_unmerged(record)?;
            if !paths.contains(&path) {
                paths.push(path);
            }
        }

        let diff = git_in(
            &self.root,
            &["diff", "-z", "--name-only", "--diff-filter=U"],
        )?;
        for record in split_nul(&diff.stdout) {
            if record.is_empty() {
                continue;
            }
            let path = bytes_to_path(record);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }

        Ok(paths)
    }

    /// `git add --` the given repo-relative paths. No-op when `paths` is empty.
    pub fn stage(&self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new("git");
        cmd.arg("add").arg("--");
        for path in paths {
            cmd.arg(path);
        }
        let output = cmd
            .current_dir(&self.root)
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| {
                if err.kind() == ErrorKind::NotFound {
                    TruceError::GitNotFound
                } else {
                    TruceError::Io(err)
                }
            })?;
        if !output.status.success() {
            return Err(TruceError::from_git_output(output.status, &output.stderr));
        }
        Ok(())
    }
}

/// Run `git` with `cwd` set to `dir`. Never invokes a shell.
pub fn git_in(dir: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                TruceError::GitNotFound
            } else {
                TruceError::Io(err)
            }
        })?;

    if !output.status.success() {
        return Err(TruceError::from_git_output(output.status, &output.stderr));
    }
    Ok(output)
}

fn stdout_path(stdout: &[u8]) -> Result<PathBuf> {
    let mut bytes = stdout;
    while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        bytes = &bytes[..bytes.len() - 1];
    }
    Ok(bytes_to_path(bytes))
}

fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|b| *b == 0).filter(|part| !part.is_empty())
}

/// `ls-files -u -z` record: `<mode> SP <hash> SP <stage> TAB <path>`.
fn parse_ls_files_unmerged(record: &[u8]) -> Result<PathBuf> {
    let tab = record
        .iter()
        .position(|b| *b == b'\t')
        .ok_or_else(|| TruceError::Git {
            status: None,
            message: format!(
                "unexpected ls-files -u output: {:?}",
                String::from_utf8_lossy(record)
            ),
        })?;
    Ok(bytes_to_path(&record[tab + 1..]))
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return PathBuf::from(s);
    }
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

/// Restrict unmerged paths to the user-supplied filters (files or directories).
pub fn filter_paths(
    repo_root: &Path,
    cwd: &Path,
    unmerged: Vec<PathBuf>,
    filters: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    if filters.is_empty() {
        return Ok(unmerged);
    }

    let mut normalized = Vec::with_capacity(filters.len());
    for filter in filters {
        normalized.push(normalize_filter(repo_root, cwd, filter)?);
    }

    Ok(unmerged
        .into_iter()
        .filter(|path| {
            normalized
                .iter()
                .any(|filter| path == filter || path.starts_with(filter))
        })
        .collect())
}

fn normalize_filter(repo_root: &Path, cwd: &Path, filter: &Path) -> Result<PathBuf> {
    // Canonicalize so Windows 8.3 vs long paths, and git's `C:/...` vs
    // `std::env::current_dir()` `C:\...`, still strip_prefix.
    let root = canonicalize_dir(repo_root);
    let cwd = canonicalize_dir(cwd);

    let abs = if filter.is_absolute() {
        filter.to_path_buf()
    } else {
        cwd.join(filter)
    };

    // Do not require the filter to exist: a deleted-ours conflict may have no
    // working-tree file. Canonicalize when we can; otherwise walk `..`.
    let abs = if abs.exists() {
        fs::canonicalize(&abs).unwrap_or_else(|_| normalize_logical(&abs))
    } else {
        normalize_logical(&abs)
    };

    abs.strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| TruceError::PathOutsideRepo {
            path: filter.to_path_buf(),
        })
}

fn canonicalize_dir(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_logical(path))
}

fn normalize_logical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_files_record_with_spaces() {
        let record = b"100644 abcdef0123456789 2\tmy cool file.txt";
        let path = parse_ls_files_unmerged(record).unwrap();
        assert_eq!(path, PathBuf::from("my cool file.txt"));
    }

    #[test]
    fn split_nul_drops_empty_tails() {
        let parts: Vec<&[u8]> = split_nul(b"a\0b\0").collect();
        assert_eq!(parts, [b"a".as_slice(), b"b".as_slice()]);
    }

    #[test]
    fn filter_matches_prefix_and_exact() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/repo");
        let unmerged = vec![
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/b.rs"),
            PathBuf::from("README.md"),
        ];
        let filters = vec![PathBuf::from("src")];
        let got = filter_paths(&root, &cwd, unmerged, &filters).unwrap();
        assert_eq!(
            got,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
    }

    #[test]
    fn filter_exact_file_after_canonicalize() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/app.rs"), "x\n").unwrap();
        let unmerged = vec![PathBuf::from("src/app.rs"), PathBuf::from("src/other.rs")];
        let got = filter_paths(root, root, unmerged, &[PathBuf::from("src/app.rs")]).unwrap();
        assert_eq!(got, vec![PathBuf::from("src/app.rs")]);
    }
}
