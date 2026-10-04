//! Bazel interactions with `CARGO_MANIFEST_DIR`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

pub type RlocationPath = String;

/// Create a symlink file on unix systems
#[cfg(target_family = "unix")]
pub fn symlink(src: &Path, dest: &Path) -> Result<(), std::io::Error> {
    std::os::unix::fs::symlink(src, dest)
}

/// Create a symlink file on windows systems
#[cfg(target_family = "windows")]
pub fn symlink(src: &Path, dest: &Path) -> Result<(), std::io::Error> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dest)
    } else {
        std::os::windows::fs::symlink_file(src, dest)
    }
}

/// Create a symlink file on unix systems
#[cfg(target_family = "unix")]
pub fn remove_symlink(path: &Path) -> Result<(), std::io::Error> {
    std::fs::remove_file(path)
}

/// Create a symlink file on windows systems
#[cfg(target_family = "windows")]
pub fn remove_symlink(path: &Path) -> Result<(), std::io::Error> {
    if path.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Check if the system supports symlinks by attempting to create one.
#[cfg(target_family = "windows")]
fn system_supports_symlinks(test_dir: &Path) -> Result<bool, String> {
    let test_file = test_dir.join("cbsr.txt");
    std::fs::write(&test_file, "").map_err(|e| {
        format!(
            "Failed to write test file for checking symlink support '{}' with {:?}",
            test_file.display(),
            e
        )
    })?;
    let test_link = test_dir.join("cbsr.link.txt");
    match symlink(&test_file, &test_link) {
        Err(_) => {
            std::fs::remove_file(test_file).map_err(|e| {
                format!("Failed to delete file {} with {:?}", test_link.display(), e)
            })?;
            Ok(false)
        }
        Ok(_) => {
            remove_symlink(&test_link).map_err(|e| {
                format!(
                    "Failed to remove symlink {} with {:?}",
                    test_link.display(),
                    e
                )
            })?;
            std::fs::remove_file(test_file).map_err(|e| {
                format!("Failed to delete file {} with {:?}", test_link.display(), e)
            })?;
            Ok(true)
        }
    }
}

fn is_dir_empty(path: &Path) -> Result<bool, String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|e| format!("Failed to read directory: {} with {:?}", path.display(), e))?;

    Ok(entries.next().is_none())
}

/// Recursively checks whether a directory tree contains any regular files.
///
/// Returns `false` if the directory only contains empty subdirectories,
/// which is important because remote execution tree artifacts only track
/// files, not directories.
fn dir_contains_files(path: &Path) -> bool {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            if dir_contains_files(&entry.path()) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

#[cfg(any(target_family = "windows", test))]
fn copy_file_if_absent(src: &Path, dest: &Path) -> std::io::Result<()> {
    struct IncompleteCopy<'a> {
        /// Some owns the incomplete destination; success or cleanup clears it.
        path: Option<&'a Path>,
    }

    // ===== impl IncompleteCopy =====

    impl IncompleteCopy<'_> {
        #[inline]
        fn disarm(mut self) {
            self.path = None;
        }

        fn cleanup(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    impl Drop for IncompleteCopy<'_> {
        #[inline]
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    // Acquire our own file first: fs::copy alone overwrites existing destinations,
    // including following symlinks. Keep fs::copy's source-attribute handling.
    // https://doc.rust-lang.org/std/fs/fn.copy.html
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    let incomplete = IncompleteCopy { path: Some(dest) };
    drop(file);
    std::fs::copy(src, dest)?;
    incomplete.disarm();
    Ok(())
}

/// A struct for generating runfiles directories to use when running Cargo build scripts.
pub struct RunfilesMaker {
    /// The output where a runfiles-like directory should be written.
    output_dir: PathBuf,

    /// A list of file suffixes to retain when pruning runfiles.
    filename_suffixes_to_retain: BTreeSet<String>,

    /// Runfiles to include in `output_dir`.
    runfiles: BTreeMap<PathBuf, RlocationPath>,
}

// ===== impl RunfilesMaker =====

impl RunfilesMaker {
    pub fn from_param_file(arg: &str) -> RunfilesMaker {
        assert!(
            arg.starts_with('@'),
            "Expected arg to be a params file. Got {}",
            arg
        );

        let content = std::fs::read_to_string(
            arg.strip_prefix('@')
                .expect("Param files should start with @"),
        )
        .unwrap();
        let mut args = content.lines();

        let output_dir = PathBuf::from(
            args.next()
                .unwrap_or_else(|| panic!("Not enough arguments provided.")),
        );
        let filename_suffixes_to_retain = args
            .next()
            .unwrap_or_else(|| panic!("Not enough arguments provided."))
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect::<BTreeSet<String>>();
        let runfiles = args
            .map(|s| {
                let s = if s.starts_with('\'') && s.ends_with('\'') {
                    s.trim_matches('\'')
                } else {
                    s
                };
                let (src, dest) = s
                    .split_once('=')
                    .unwrap_or_else(|| panic!("Unexpected runfiles argument: {}", s));
                (PathBuf::from(src), RlocationPath::from(dest))
            })
            .collect::<BTreeMap<_, _>>();

        assert!(!runfiles.is_empty(), "No runfiles found");

        RunfilesMaker {
            output_dir,
            filename_suffixes_to_retain,
            runfiles,
        }
    }

    fn is_mergeable_metadata(rlocation_path: &str) -> bool {
        rlocation_path.ends_with("/_repo_mapping")
            || rlocation_path == "_repo_mapping"
            || rlocation_path.ends_with("/MANIFEST")
            || rlocation_path == "MANIFEST"
    }

    fn merge_metadata_file(existing: &Path, new_source: &Path) -> Result<(), String> {
        let existing_content = if existing.is_symlink() {
            let target = std::fs::read_link(existing).map_err(|e| {
                format!(
                    "Failed to read symlink '{}' with {:?}",
                    existing.display(),
                    e
                )
            })?;
            std::fs::read(&target).map_err(|e| {
                format!(
                    "Failed to read symlink target '{}' with {:?}",
                    target.display(),
                    e
                )
            })?
        } else {
            std::fs::read(existing)
                .map_err(|e| format!("Failed to read file '{}' with {:?}", existing.display(), e))?
        };

        let new_content = std::fs::read(new_source).map_err(|e| {
            format!(
                "Failed to read file '{}' with {:?}",
                new_source.display(),
                e
            )
        })?;

        if existing_content == new_content {
            return Ok(());
        }

        let existing_str = String::from_utf8(existing_content)
            .map_err(|e| format!("Failed to parse '{}' as UTF-8: {:?}", existing.display(), e))?;
        let new_str = String::from_utf8(new_content).map_err(|e| {
            format!(
                "Failed to parse '{}' as UTF-8: {:?}",
                new_source.display(),
                e
            )
        })?;

        let mut merged_lines: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for line in existing_str.lines().chain(new_str.lines()) {
            if seen.insert(line.to_string()) {
                merged_lines.push(line.to_string());
            }
        }

        if existing.is_symlink() {
            remove_symlink(existing).map_err(|e| {
                format!(
                    "Failed to remove symlink '{}' with {:?}",
                    existing.display(),
                    e
                )
            })?;
        } else {
            std::fs::remove_file(existing).map_err(|e| {
                format!(
                    "Failed to remove file '{}' with {:?}",
                    existing.display(),
                    e
                )
            })?;
        }

        std::fs::write(existing, merged_lines.join("\n")).map_err(|e| {
            format!(
                "Failed to write merged metadata to '{}' with {:?}",
                existing.display(),
                e
            )
        })?;

        Ok(())
    }

    /// Create a runfiles directory.
    #[cfg(target_family = "unix")]
    pub fn create_runfiles_dir(&self) -> Result<(), String> {
        for (src, dest) in &self.runfiles {
            let abs_dest = self.output_dir.join(dest);

            if let Some(parent) = abs_dest.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!(
                            "Failed to create parent directory '{}' for '{}' with {:?}",
                            parent.display(),
                            abs_dest.display(),
                            e
                        )
                    })?;
                }
            }

            let abs_src = std::env::current_dir().unwrap().join(src);

            match symlink(&abs_src, &abs_dest) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_mergeable_metadata(dest) {
                        Self::merge_metadata_file(&abs_dest, &abs_src)?;
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to link `{} -> {}` with {:?}",
                        abs_src.display(),
                        abs_dest.display(),
                        e
                    ));
                }
            }
        }

        Ok(())
    }

    /// Create a runfiles directory.
    #[cfg(target_family = "windows")]
    pub fn create_runfiles_dir(&self) -> Result<(), String> {
        if !self.output_dir.exists() {
            std::fs::create_dir_all(&self.output_dir).map_err(|e| {
                format!(
                    "Failed to create output directory '{}' with {:?}",
                    self.output_dir.display(),
                    e
                )
            })?;
        }

        let supports_symlinks = system_supports_symlinks(&self.output_dir)?;
        let cwd = std::env::current_dir()
            .map_err(|e| format!("Failed to resolve current directory with {:?}", e))?;

        for (src, dest) in &self.runfiles {
            let abs_dest = self.output_dir.join(dest);
            if let Some(parent) = abs_dest.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!(
                            "Failed to create parent directory '{}' for '{}' with {:?}",
                            parent.display(),
                            abs_dest.display(),
                            e
                        )
                    })?;
                }
            }

            let abs_src = cwd.join(src);
            if supports_symlinks {
                Self::finish_link_windows(&abs_src, &abs_dest, dest, symlink(&abs_src, &abs_dest))?;
            } else {
                Self::copy_runfile_windows(&abs_src, &abs_dest, dest)?;
            }
        }
        Ok(())
    }

    #[cfg(any(target_family = "windows", test))]
    fn finish_link_windows(
        src: &Path,
        dest: &Path,
        rlocation_path: &str,
        link_result: std::io::Result<()>,
    ) -> Result<(), String> {
        // A short capability probe can succeed while a long individual input
        // fails with ERROR_PRIVILEGE_NOT_HELD. Other link failures must surface.
        // https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--1300-1699-
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

        match link_result {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => {
                Self::copy_runfile_windows(src, dest, rlocation_path)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::merge_runfile_collision(src, dest, rlocation_path)
            }
            Err(e) => Err(format!(
                "Failed to link `{} -> {}` with {:?}",
                src.display(),
                dest.display(),
                e
            )),
        }
    }

    #[cfg(any(target_family = "windows", test))]
    fn copy_runfile_windows(src: &Path, dest: &Path, rlocation_path: &str) -> Result<(), String> {
        match copy_file_if_absent(src, dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::merge_runfile_collision(src, dest, rlocation_path)
            }
            Err(e) => Err(format!(
                "Failed to copy `{} -> {}` with {:?}",
                src.display(),
                dest.display(),
                e
            )),
        }
    }

    #[cfg(any(target_family = "windows", test))]
    fn merge_runfile_collision(
        src: &Path,
        dest: &Path,
        rlocation_path: &str,
    ) -> Result<(), String> {
        if Self::is_mergeable_metadata(rlocation_path) {
            Self::merge_metadata_file(dest, src)?;
        }
        Ok(())
    }

    fn prune_empty_parents(&self, path: &Path) -> Result<(), String> {
        let mut dir = path.parent().map(Path::to_path_buf);
        while let Some(parent) = dir {
            if parent == self.output_dir {
                break;
            }
            if is_dir_empty(&parent)
                .map_err(|e| format!("Failed to determine if directory was empty with: {:?}", e))?
            {
                std::fs::remove_dir(&parent).map_err(|e| {
                    format!(
                        "Failed to delete directory {} with {:?}",
                        parent.display(),
                        e
                    )
                })?;
                dir = parent.parent().map(Path::to_path_buf);
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Tear down the runfiles directory, materializing retained entries as real files.
    ///
    /// Removes every entry created by [`Self::create_runfiles_dir`] (symlinks, plus the
    /// real files produced by merged metadata). For entries whose destination matches a
    /// user-defined suffix, the source is then copied into place so the file survives
    /// after the runfiles tree is gone. Skips entries whose destination was already
    /// processed (from runfiles collisions).
    #[cfg(target_family = "unix")]
    fn drain_runfiles_dir_unix(&self) -> Result<(), String> {
        let mut processed: HashSet<String> = HashSet::new();

        for (src, dest) in &self.runfiles {
            if !processed.insert(dest.clone()) {
                continue;
            }

            let abs_dest = self.output_dir.join(dest);

            if !abs_dest.exists() && !abs_dest.is_symlink() {
                continue;
            }

            if abs_dest.is_symlink() {
                remove_symlink(&abs_dest).map_err(|e| {
                    format!(
                        "Failed to delete symlink '{}' with {:?}",
                        abs_dest.display(),
                        e
                    )
                })?;
            } else {
                std::fs::remove_file(&abs_dest).map_err(|e| {
                    format!(
                        "Failed to delete file '{}' with {:?}",
                        abs_dest.display(),
                        e
                    )
                })?;
            }

            if !self
                .filename_suffixes_to_retain
                .iter()
                .any(|suffix| dest.ends_with(suffix))
            {
                self.prune_empty_parents(&abs_dest)?;
                continue;
            }

            std::fs::copy(src, &abs_dest).map_err(|e| {
                format!(
                    "Failed to copy `{} -> {}` with {:?}",
                    src.display(),
                    abs_dest.display(),
                    e
                )
            })?;
        }

        Ok(())
    }

    /// Tear down the runfiles directory, leaving retained entries in place.
    ///
    /// Windows creation can mix symlinks and copied files. Materialize retained links,
    /// preserve retained real files (including build-script edits or merged metadata),
    /// and remove non-retained entries regardless of how they were created.
    #[cfg(any(target_family = "windows", test))]
    fn drain_runfiles_dir_windows(&self) -> Result<(), String> {
        let mut processed: HashSet<String> = HashSet::new();

        for (src, dest) in &self.runfiles {
            if !processed.insert(dest.clone()) {
                continue;
            }

            let retain = self
                .filename_suffixes_to_retain
                .iter()
                .any(|suffix| dest.ends_with(suffix));
            let abs_dest = self.output_dir.join(dest);
            let metadata = match std::fs::symlink_metadata(&abs_dest) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(format!(
                        "Failed to inspect file '{}' with {:?}",
                        abs_dest.display(),
                        e
                    ));
                }
            };

            if retain && !metadata.file_type().is_symlink() {
                continue;
            }

            if metadata.file_type().is_symlink() {
                remove_symlink(&abs_dest).map_err(|e| {
                    format!(
                        "Failed to delete symlink '{}' with {:?}",
                        abs_dest.display(),
                        e
                    )
                })?;
            } else {
                std::fs::remove_file(&abs_dest).map_err(|e| {
                    format!(
                        "Failed to delete file '{}' with {:?}",
                        abs_dest.display(),
                        e
                    )
                })?;
            }

            if retain {
                copy_file_if_absent(src, &abs_dest).map_err(|e| {
                    format!(
                        "Failed to copy `{} -> {}` with {:?}",
                        src.display(),
                        abs_dest.display(),
                        e
                    )
                })?;
            } else {
                self.prune_empty_parents(&abs_dest)?;
            }
        }
        Ok(())
    }

    /// Tear down the runfiles directory, keeping only entries whose destination matches
    /// a user-defined suffix. Retained entries are left as real files in `out_dir`.
    pub fn drain_runfiles_dir(&self, out_dir: &Path) -> Result<(), String> {
        #[cfg(target_family = "windows")]
        self.drain_runfiles_dir_windows()?;
        #[cfg(target_family = "unix")]
        self.drain_runfiles_dir_unix()?;

        // If the runfiles dir contains no files, add an empty file to avoid
        // an upstream Bazel bug where tree artifacts with only empty
        // subdirectories are considered "not created" in remote execution.
        // https://github.com/bazelbuild/bazel/issues/28286
        if !dir_contains_files(&self.output_dir) {
            std::fs::write(self.output_dir.join(".empty"), "").unwrap_or_else(|e| {
                panic!(
                    "Failed to write empty file to runfiles dir `{}`\n{:?}",
                    self.output_dir.display(),
                    e
                )
            })
        }

        // Due to the symlinks in `CARGO_MANIFEST_DIR`, some build scripts
        // may have placed symlinks over real files in `OUT_DIR`. To counter
        // this, all non-relative symlinks are resolved.
        replace_symlinks_in_out_dir(out_dir)
    }
}

/// Iterates over the given directory recursively and resolves any symlinks
///
/// Symlinks shouldn't present in `out_dir` as those amy contain paths to sandboxes which doesn't exists anymore.
/// Therefore, bazel will fail because of dangling symlinks.
fn replace_symlinks_in_out_dir(out_dir: &Path) -> Result<(), String> {
    if out_dir.is_dir() {
        let out_dir_paths = std::fs::read_dir(out_dir).map_err(|e| {
            format!(
                "Failed to read directory `{}` with {:?}",
                out_dir.display(),
                e
            )
        })?;
        for entry in out_dir_paths {
            let entry =
                entry.map_err(|e| format!("Failed to read directory entry with  {:?}", e,))?;
            let path = entry.path();

            if path.is_symlink() {
                let target_path = std::fs::read_link(&path).map_err(|e| {
                    format!("Failed to read symlink `{}` with {:?}", path.display(), e,)
                })?;
                // we don't want to replace relative symlinks
                if target_path.is_relative() {
                    continue;
                }
                std::fs::remove_file(&path)
                    .map_err(|e| format!("Failed remove file `{}` with {:?}", path.display(), e))?;
                std::fs::copy(&target_path, &path).map_err(|e| {
                    format!(
                        "Failed to copy `{} -> {}` with {:?}",
                        target_path.display(),
                        path.display(),
                        e
                    )
                })?;
            } else if path.is_dir() {
                replace_symlinks_in_out_dir(&path).map_err(|e| {
                    format!(
                        "Failed to normalize nested directory `{}` with {}",
                        path.display(),
                        e,
                    )
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    macro_rules! assert_ok {
        ($result:expr) => {
            match $result {
                ::std::result::Result::Ok(value) => value,
                ::std::result::Result::Err(error) => ::std::panic!("expected Ok, got {:?}", error),
            }
        };
    }

    macro_rules! assert_err {
        ($result:expr) => {
            match $result {
                ::std::result::Result::Err(error) => error,
                ::std::result::Result::Ok(value) => ::std::panic!("expected Err, got {:?}", value),
            }
        };
    }

    struct TestDir {
        /// Some owns this fixture tree; explicit cleanup or Drop takes it once.
        path: Option<PathBuf>,
    }

    // ===== impl TestDir =====

    impl Default for TestDir {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    impl TestDir {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let base = PathBuf::from(assert_ok!(std::env::var("TEST_TMPDIR")));
            let path = base.join(format!(
                "windows_runfiles_{}_{}",
                std::process::id(),
                // Only allocates unique names; it publishes no fixture state.
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            assert_ok!(fs::create_dir(&path));
            TestDir { path: Some(path) }
        }

        #[inline]
        fn path(&self) -> &Path {
            self.path
                .as_deref()
                .expect("test directory remains owned until cleanup")
        }

        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.path().join(relative);
            assert_ok!(fs::create_dir_all(
                path.parent()
                    .expect("fixture file is below its owned directory")
            ));
            assert_ok!(fs::write(&path, contents));
            path
        }

        fn maker(&self, inputs: &[(&Path, &str)], retain: &[&str]) -> RunfilesMaker {
            let output_dir = self.path().join("runfiles");
            assert_ok!(fs::create_dir(&output_dir));
            for (_, dest) in inputs {
                assert_ok!(fs::create_dir_all(
                    output_dir
                        .join(dest)
                        .parent()
                        .expect("runfile is below its owned directory")
                ));
            }
            RunfilesMaker {
                output_dir,
                filename_suffixes_to_retain: retain.iter().map(|s| (*s).to_owned()).collect(),
                runfiles: inputs
                    .iter()
                    .map(|(src, dest)| (src.to_path_buf(), (*dest).to_owned()))
                    .collect(),
            }
        }

        fn cleanup(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }

    impl Drop for TestDir {
        #[inline]
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    #[inline]
    fn privilege_error() -> std::io::Error {
        // Observed native Windows error, independently of the implementation's
        // fallback discriminator: ERROR_PRIVILEGE_NOT_HELD.
        // https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--1300-1699-
        const WINDOWS_PRIVILEGE_ERROR: i32 = 1314;
        std::io::Error::from_raw_os_error(WINDOWS_PRIVILEGE_ERROR)
    }

    /// The short Windows symlink probe passed but this long declared input failed
    /// with raw1314 (Uncategorized). Inject only that OS result; copying and cleanup
    /// use real files. Native CI still owns Windows acceptance.
    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn privilege_failure_copies_long_declared_file() {
        // Exceed Windows' legacy MAX_PATH with components below its name limit.
        // https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation
        const LEGACY_MAX_PATH: usize = 260;
        const LONG_PATH_COMPONENTS: usize = 5;
        let dir = TestDir::new();
        let long_dir =
            ["long_directory_component_long_directory_component"; LONG_PATH_COMPONENTS].join("/");
        let src = dir.write(&format!("source/{}/input.cs", long_dir), b"declared input");
        let rlocation = format!("!/{}/input.cs", long_dir);
        let maker = dir.maker(&[(&src, &rlocation)], &[]);
        let dest = maker.output_dir.join(&rlocation);
        assert!(
            src.to_string_lossy().encode_utf16().count() > LEGACY_MAX_PATH,
            "source={}",
            src.display()
        );
        assert!(
            dest.to_string_lossy().encode_utf16().count() > LEGACY_MAX_PATH,
            "destination={}",
            dest.display()
        );
        let error = privilege_error();
        assert_ne!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_ok!(RunfilesMaker::finish_link_windows(
            &src,
            &dest,
            &rlocation,
            Err(error),
        ));
        assert!(!dest.is_symlink(), "destination={}", dest.display());
        assert_eq!(assert_ok!(fs::read(&dest)), b"declared input");
        assert_eq!(assert_ok!(fs::read(&src)), b"declared input");
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn unrelated_link_failure_does_not_copy() {
        let dir = TestDir::new();
        let src = dir.write("source/input", b"declared input");
        let maker = dir.maker(&[(&src, "input")], &[]);
        let dest = maker.output_dir.join("input");
        let error = assert_err!(RunfilesMaker::finish_link_windows(
            &src,
            &dest,
            "input",
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unrelated link failure",
            )),
        ));
        assert!(error.contains("Failed to link"), "{}", error);
        assert!(error.contains("unrelated link failure"), "{}", error);
        assert!(!dest.exists(), "destination={}", dest.display());
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn failed_copy_removes_only_its_placeholder() {
        let dir = TestDir::new();
        let source_dir = dir.path().join("source_dir");
        assert_ok!(fs::create_dir(&source_dir));
        let missing = dir.path().join("missing");
        for src in [&missing, &source_dir] {
            let dest = dir.path().join("destination");
            let error = assert_err!(copy_file_if_absent(src, &dest));
            if src == &missing {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            }
            assert!(!dest.exists(), "destination={}", dest.display());
            assert!(!dest.is_symlink(), "destination={}", dest.display());
            assert!(source_dir.is_dir(), "source={}", source_dir.display());
        }
        let error = assert_err!(RunfilesMaker::finish_link_windows(
            &missing,
            &dir.path().join("destination"),
            "destination",
            Err(privilege_error()),
        ));
        assert!(error.contains("Failed to copy"), "{}", error);
        assert!(
            !dir.path().join("destination").exists(),
            "fixture={}",
            dir.path().display()
        );
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn copy_collision_preserves_ordinary_and_dangling_destinations() {
        let dir = TestDir::new();
        let src = dir.write("source/input", b"new source");
        let dest = dir.write("ordinary", b"existing destination");
        assert_ok!(RunfilesMaker::copy_runfile_windows(&src, &dest, "ordinary"));
        assert_ok!(RunfilesMaker::finish_link_windows(
            &src,
            &dest,
            "ordinary",
            symlink(&src, &dest),
        ));
        assert_eq!(assert_ok!(fs::read(&dest)), b"existing destination");

        let absent = dir.path().join("absent_target");
        let dangling = dir.path().join("dangling");
        assert_ok!(symlink(&absent, &dangling));
        assert_ok!(RunfilesMaker::finish_link_windows(
            &src,
            &dangling,
            "dangling",
            Err(privilege_error()),
        ));
        assert!(dangling.is_symlink(), "destination={}", dangling.display());
        assert_eq!(assert_ok!(fs::read_link(&dangling)), absent);
        assert!(!absent.exists(), "target={}", absent.display());
        assert_eq!(assert_ok!(fs::read(&src)), b"new source");
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn metadata_collisions_preserve_merged_retained_contents() {
        for entry_type in ["copied", "linked"] {
            let dir = TestDir::new();
            let first = dir.write("source/a", b"first\nshared\n");
            let second = dir.write("source/z", b"shared\nsecond\n");
            let maker = dir.maker(
                &[(&first, "MANIFEST"), (&second, "MANIFEST")],
                &["MANIFEST"],
            );
            let dest = maker.output_dir.join("MANIFEST");
            match entry_type {
                "copied" => {
                    assert_ok!(RunfilesMaker::copy_runfile_windows(
                        &first, &dest, "MANIFEST"
                    ));
                }
                "linked" => {
                    assert_ok!(RunfilesMaker::finish_link_windows(
                        &first,
                        &dest,
                        "MANIFEST",
                        symlink(&first, &dest),
                    ));
                }
                _ => unreachable!(),
            }
            match entry_type {
                "copied" => {
                    assert_ok!(RunfilesMaker::copy_runfile_windows(
                        &second, &dest, "MANIFEST"
                    ));
                }
                "linked" => {
                    assert_ok!(RunfilesMaker::finish_link_windows(
                        &second,
                        &dest,
                        "MANIFEST",
                        symlink(&second, &dest),
                    ));
                }
                _ => unreachable!(),
            }
            assert_eq!(assert_ok!(fs::read(&dest)), b"first\nshared\nsecond");
            assert!(!dest.is_symlink(), "destination={}", dest.display());
            assert_ok!(maker.drain_runfiles_dir_windows());
            assert_eq!(assert_ok!(fs::read(&dest)), b"first\nshared\nsecond");
            assert_eq!(assert_ok!(fs::read(&first)), b"first\nshared\n");
            assert_eq!(assert_ok!(fs::read(&second)), b"shared\nsecond\n");
        }
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn mixed_cleanup_materializes_links_and_preserves_edited_copies() {
        let dir = TestDir::new();
        let linked = dir.write("source/a", b"retained link");
        let collision = dir.write("source/z", b"collision source");
        let copied = dir.write("source/copy", b"original copy");
        let discarded_link = dir.write("source/link", b"discarded link");
        let discarded_copy = dir.write("source/plain", b"discarded copy");
        let absent = dir.path().join("source/absent");
        let maker = dir.maker(
            &[
                (&linked, "retained/link.keep"),
                (&collision, "retained/link.keep"),
                (&copied, "retained/copy.keep"),
                (&discarded_link, "discarded/link"),
                (&discarded_copy, "discarded/copy"),
                (&absent, "dangling/link"),
            ],
            &[".keep"],
        );
        let retained_link = maker.output_dir.join("retained/link.keep");
        let retained_copy = maker.output_dir.join("retained/copy.keep");
        assert_ok!(symlink(&linked, &retained_link));
        assert_ok!(copy_file_if_absent(&copied, &retained_copy));
        assert_ok!(fs::write(&retained_copy, b"build-script edit"));
        assert_ok!(symlink(
            &discarded_link,
            &maker.output_dir.join("discarded/link")
        ));
        assert_ok!(copy_file_if_absent(
            &discarded_copy,
            &maker.output_dir.join("discarded/copy")
        ));
        assert_ok!(symlink(&absent, &maker.output_dir.join("dangling/link")));

        assert_ok!(maker.drain_runfiles_dir_windows());
        assert!(
            !retained_link.is_symlink(),
            "destination={}",
            retained_link.display()
        );
        assert_eq!(assert_ok!(fs::read(&retained_link)), b"retained link");
        assert_eq!(assert_ok!(fs::read(&retained_copy)), b"build-script edit");
        assert!(
            !maker.output_dir.join("discarded").exists(),
            "runfiles={}",
            maker.output_dir.display()
        );
        assert!(
            !maker.output_dir.join("dangling").exists(),
            "runfiles={}",
            maker.output_dir.display()
        );
        assert_eq!(assert_ok!(fs::read(&copied)), b"original copy");
        assert_eq!(assert_ok!(fs::read(&collision)), b"collision source");
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn copied_read_only_files_keep_attributes_and_follow_retention() {
        let dir = TestDir::new();
        let retained = dir.write("source/retained", b"retained bytes");
        let discarded = dir.write("source/discarded", b"discarded bytes");
        for src in [&retained, &discarded] {
            let mut permissions = assert_ok!(fs::metadata(src)).permissions();
            permissions.set_readonly(true);
            assert_ok!(fs::set_permissions(src, permissions));
        }
        let maker = dir.maker(
            &[
                (&retained, "retained.keep"),
                (&discarded, "nested/discarded"),
            ],
            &[".keep"],
        );
        let keep = maker.output_dir.join("retained.keep");
        let remove = maker.output_dir.join("nested/discarded");
        assert_ok!(RunfilesMaker::copy_runfile_windows(
            &retained,
            &keep,
            "retained.keep"
        ));
        assert_ok!(RunfilesMaker::copy_runfile_windows(
            &discarded,
            &remove,
            "nested/discarded"
        ));
        assert!(
            assert_ok!(fs::metadata(&keep)).permissions().readonly(),
            "retained={}",
            keep.display()
        );
        assert!(
            assert_ok!(fs::metadata(&remove)).permissions().readonly(),
            "discarded={}",
            remove.display()
        );
        assert_ok!(maker.drain_runfiles_dir_windows());
        assert_eq!(assert_ok!(fs::read(&keep)), b"retained bytes");
        assert!(
            assert_ok!(fs::metadata(&keep)).permissions().readonly(),
            "retained={}",
            keep.display()
        );
        assert!(
            !maker.output_dir.join("nested").exists(),
            "runfiles={}",
            maker.output_dir.display()
        );
        for src in [&retained, &discarded] {
            assert!(
                assert_ok!(fs::metadata(src)).permissions().readonly(),
                "source={}",
                src.display()
            );
        }
        assert_eq!(assert_ok!(fs::read(&retained)), b"retained bytes");
        assert_eq!(assert_ok!(fs::read(&discarded)), b"discarded bytes");
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn retained_dangling_link_surfaces_copy_failure_without_placeholder() {
        let dir = TestDir::new();
        let src = dir.path().join("missing_source");
        let maker = dir.maker(&[(&src, "retained.keep")], &[".keep"]);
        let dest = maker.output_dir.join("retained.keep");
        assert_ok!(symlink(&src, &dest));
        let error = assert_err!(maker.drain_runfiles_dir_windows());
        assert!(error.contains("Failed to copy"), "{}", error);
        assert!(!dest.exists(), "destination={}", dest.display());
        assert!(!dest.is_symlink(), "destination={}", dest.display());
        assert!(!src.exists(), "source={}", src.display());
    }

    // https://github.com/sloper-ai/sloper/actions/runs/37217094145/job/111485839839
    #[test]
    fn fully_drained_tree_keeps_empty_marker() {
        let dir = TestDir::new();
        let src = dir.write("source/input", b"input");
        let maker = dir.maker(&[(&src, "nested/input")], &[]);
        assert_ok!(RunfilesMaker::copy_runfile_windows(
            &src,
            &maker.output_dir.join("nested/input"),
            "nested/input",
        ));
        assert_ok!(maker.drain_runfiles_dir_windows());
        let out_dir = dir.path().join("out_dir");
        assert_ok!(fs::create_dir(&out_dir));
        assert_ok!(maker.drain_runfiles_dir(&out_dir));
        assert!(
            !maker.output_dir.join("nested").exists(),
            "runfiles={}",
            maker.output_dir.display()
        );
        assert_eq!(assert_ok!(fs::read(maker.output_dir.join(".empty"))), b"");
    }

    fn prepare_output_dir_with_symlinks() -> PathBuf {
        let test_tmp = PathBuf::from(std::env::var("TEST_TMPDIR").unwrap());
        let out_dir = test_tmp.join("out_dir");
        fs::create_dir(&out_dir).unwrap();
        let nested_dir = out_dir.join("nested");
        fs::create_dir(nested_dir).unwrap();

        let temp_dir_file = test_tmp.join("outside.txt");
        let mut file = fs::File::create(&temp_dir_file).unwrap();
        file.write_all(b"outside world").unwrap();
        // symlink abs path outside of the out_dir
        symlink(&temp_dir_file, &out_dir.join("outside.txt")).unwrap();

        let inside_dir_file = out_dir.join("inside.txt");
        let mut file = fs::File::create(inside_dir_file).unwrap();
        file.write_all(b"inside world").unwrap();
        // symlink relative next to the file in the out_dir
        symlink(
            &PathBuf::from("inside.txt"),
            &out_dir.join("inside_link.txt"),
        )
        .unwrap();
        // symlink relative within a subdir in the out_dir
        symlink(
            &PathBuf::from("..").join("inside.txt"),
            &out_dir.join("nested").join("inside_link.txt"),
        )
        .unwrap();

        out_dir
    }

    #[cfg(any(target_family = "windows", target_family = "unix"))]
    #[test]
    fn replace_symlinks_in_out_dir() {
        let out_dir = prepare_output_dir_with_symlinks();
        super::replace_symlinks_in_out_dir(&out_dir).unwrap();

        // this should be replaced because it is an absolute symlink
        let file_path = out_dir.join("outside.txt");
        assert!(!file_path.is_symlink());
        let contents = fs::read_to_string(file_path).unwrap();
        assert_eq!(contents, "outside world");

        // this is the file created inside the out_dir
        let file_path = out_dir.join("inside.txt");
        assert!(!file_path.is_symlink());
        let contents = fs::read_to_string(file_path).unwrap();
        assert_eq!(contents, "inside world");

        // this is the symlink in the out_dir
        let file_path = out_dir.join("inside_link.txt");
        assert!(file_path.is_symlink());
        let contents = fs::read_to_string(file_path).unwrap();
        assert_eq!(contents, "inside world");

        // this is the symlink in the out_dir under another directory which refers to ../inside.txt
        let file_path = out_dir.join("nested").join("inside_link.txt");
        assert!(file_path.is_symlink());
        let contents = fs::read_to_string(file_path).unwrap();
        assert_eq!(contents, "inside world");
    }
}
