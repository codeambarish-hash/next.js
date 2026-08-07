use std::{
    env,
    ffi::{OsStr, OsString},
    fs::FileTimes,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use fs_err::{DirEntry, OpenOptions, read_dir, remove_dir_all, rename};

/// Information gathered by `vergen_gitcl` in the top-level binary crate and passed down. This
/// information must be computed in the top-level crate for cargo incremental compilation to work
/// correctly.
///
/// See `crates/next-napi-bindings/build.rs` for details.
pub struct GitVersionInfo<'a> {
    /// Output of `git describe --match 'v[0-9]' --dirty`.
    pub describe: &'a str,
    /// Is the git repository dirty? Always forced to `false` when the `CI` environment variable is
    /// set and non-empty.
    pub dirty: bool,
}

/// How many databases with a version other than the current one are retained.
///
/// The point of retaining any at all is to make version changes cheap to undo: upgrading Next.js on
/// a branch needs a fresh cache, but switching back to the old branch should still find the old
/// cache intact. One spare version covers that round trip, and each additional one costs real disk
/// (caches are routinely multiple GB). On CI it never keeps any other versions.
const MAX_OTHER_DB_VERSIONS: usize = 1;

/// How long a database with a version other than the current one is retained since it was last
/// used. Beyond this it's unlikely to be switched back to, and the disk space is better reclaimed.
///
/// Overridable via the `TURBO_ENGINE_VERSION_TTL` environment variable (in seconds).
const DEFAULT_OTHER_DB_VERSION_TTL: Duration = Duration::from_secs(3 * 24 * 60 * 60); // 3 days

/// Directories are prefixed with this before being deleted, so that if we fail to fully delete the
/// directory, we can pick up where we left off last time.
const DELETION_PREFIX: &str = "__stale_";

/// The file whose mtime records when a version directory was last used.
///
/// This is the persistence layer's own `CURRENT` file, reused as the last-used stamp. It's written
/// on every commit anyway, and we additionally touch it whenever the version is selected, so that
/// read-only sessions (and sessions that never persist anything) still count as "used".
///
/// Deliberately not the directory's own atime/mtime: atime is fragile (recursive scanning tools
/// like ripgrep bump it, and `noatime` mounts pin it to mtime), and the directory's mtime only
/// changes when entries are added or removed, not when the cache is read.
const LAST_USED_FILE: &str = "CURRENT";

/// Given a base path, creates a version directory for the given `version_info`. Automatically
/// cleans up old/stale databases.
///
/// A database whose version isn't the current one is retained only if it is among the
/// [`MAX_OTHER_DB_VERSIONS`] most recently used *and* was used within
/// [`DEFAULT_OTHER_DB_VERSION_TTL`]. The current version's database is always retained, no matter
/// how long ago it was last used.
///
/// **Environment Variables**
/// - `TURBO_ENGINE_VERSION`: Forces use of a specific database version.
/// - `TURBO_ENGINE_IGNORE_DIRTY`: Enable filesystem cache in a dirty git repository. Otherwise a
///   temporary directory is created.
/// - `TURBO_ENGINE_DISABLE_VERSIONING`: Ignores versioning and always uses the same "unversioned"
///   database when set.
/// - `TURBO_ENGINE_VERSION_TTL`: How long, in seconds, to retain a database whose version isn't the
///   current one. Overrides [`DEFAULT_OTHER_DB_VERSION_TTL`].
pub fn handle_db_versioning(
    base_path: &Path,
    version_info: &GitVersionInfo,
    is_ci: bool,
) -> Result<PathBuf> {
    if let Ok(version) = env::var("TURBO_ENGINE_VERSION") {
        return Ok(base_path.join(version));
    }
    let ignore_dirty = env::var("TURBO_ENGINE_IGNORE_DIRTY").ok().is_some();
    let disabled_versioning = env::var("TURBO_ENGINE_DISABLE_VERSIONING").ok().is_some();
    let version = if disabled_versioning {
        println!(
            "WARNING: File System Cache versioning is disabled. Manual removal of the filesystem \
             caching database might be required."
        );
        Some("unversioned")
    } else if !version_info.dirty {
        Some(version_info.describe)
    } else if ignore_dirty {
        println!(
            "WARNING: The git repository is dirty, but File System Cache is still enabled. Manual \
             removal of the filesystem cache database might be required."
        );
        Some(version_info.describe)
    } else {
        println!(
            "WARNING: The git repository is dirty: File System Cache is disabled. Use \
             TURBO_ENGINE_IGNORE_DIRTY=1 to ignore dirtiness of the repository."
        );
        None
    };
    let path;
    if let Some(version) = version {
        path = base_path.join(version);

        let (max_other_db_versions, ttl) = if is_ci {
            (0, Duration::ZERO)
        } else {
            (MAX_OTHER_DB_VERSIONS, other_db_version_ttl())
        };

        if let Ok(read_dir) = read_dir(base_path) {
            let mut old_dbs = Vec::new();
            for entry in read_dir {
                let Ok(entry) = entry else { continue };

                // skip our target version (if it exists)
                let name = entry.file_name();
                if name == version {
                    continue;
                }

                // skip non-directories
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }

                // Find and try to finish removing any partially deleted directories
                if name
                    .as_encoded_bytes()
                    .starts_with(AsRef::<OsStr>::as_ref(DELETION_PREFIX).as_encoded_bytes())
                {
                    // failures during cleanup of a cache directory are not fatal
                    let _ = remove_dir_all(entry.path());
                    continue;
                }

                old_dbs.push(entry);
            }

            // Most recently used first, so the ones we keep are a prefix of this list. Entries
            // whose age can't be determined sort last and are therefore evicted first.
            old_dbs
                .sort_by_cached_key(|entry| time_since_last_used(entry).unwrap_or(Duration::MAX));

            let retained = old_dbs
                .iter()
                .take(max_other_db_versions)
                .take_while(|entry| time_since_last_used(entry).is_some_and(|age| age <= ttl))
                .count();

            for entry in old_dbs.into_iter().skip(retained) {
                let mut new_name = OsString::from(DELETION_PREFIX);
                new_name.push(entry.file_name());
                let new_path = base_path.join(new_name);
                // rename first, it's an atomic operation
                let rename_result = rename(entry.path(), &new_path);
                // Only try to delete the files if the rename succeeded, it's not safe to delete
                // contents if we didn't manage to first poison the directory by renaming it.
                if rename_result.is_ok() {
                    // It's okay if this fails, as we've already poisoned the directory.
                    let _ = remove_dir_all(&new_path);
                }
            }
        }

        // Record that this version is in use, so that a future run retains it for the TTL. Done
        // after cleanup so that a failure to clean up doesn't cost us the stamp, and best-effort
        // because an unstamped directory only risks being evicted early.
        let _ = mark_used(&path);
    } else {
        path = base_path.join("temp");
        if path.exists() {
            // propagate errors: if this fails we may have stale files left over in the temp
            // directory
            remove_dir_all(&path)?;
        }
    }

    Ok(path)
}

/// How long to retain a database whose version isn't the current one, honoring the
/// `TURBO_ENGINE_VERSION_TTL` override (in seconds). Falls back to
/// [`DEFAULT_OTHER_DB_VERSION_TTL`] if the variable is unset or unparsable.
fn other_db_version_ttl() -> Duration {
    let Ok(raw) = env::var("TURBO_ENGINE_VERSION_TTL") else {
        return DEFAULT_OTHER_DB_VERSION_TTL;
    };
    match raw.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            println!(
                "WARNING: Ignoring TURBO_ENGINE_VERSION_TTL={raw:?}, expected a whole number of \
                 seconds."
            );
            DEFAULT_OTHER_DB_VERSION_TTL
        }
    }
}

/// How long ago the version directory `entry` was last used.
///
/// Read from the mtime of [`LAST_USED_FILE`], falling back to the newest mtime of any file directly
/// inside the directory. The fallback covers directories written by a version of the persistence
/// layer that named its files differently, or where `CURRENT` can't be read: such a directory may
/// hold a perfectly usable cache, and treating "I can't tell how old this is" as "delete it now"
/// would throw away the cache on the very upgrade round trip this retention exists to support.
///
/// Returns `None` only when the directory has no readable timestamp at all, which means it doesn't
/// look like a database — callers treat that as maximally stale.
///
/// A clock that jumped backwards can leave a timestamp in the future; that reads as age zero rather
/// than as an error, so a version is never evicted for looking too new.
fn time_since_last_used(entry: &DirEntry) -> Option<Duration> {
    let dir = entry.path();
    let age_of = |mtime: SystemTime| SystemTime::now().duration_since(mtime).unwrap_or_default();

    if let Ok(mtime) = dir
        .join(LAST_USED_FILE)
        .metadata()
        .and_then(|m| m.modified())
    {
        return Some(age_of(mtime));
    }

    // Fall back to the most recently modified file in the directory. Not recursive: the
    // persistence layer keeps its SST/meta/blob files at the top level, so this is enough to tell
    // a live cache from an abandoned one without walking a multi-GB tree on every startup.
    read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok()?.metadata().ok()?.modified().ok())
        .max()
        .map(age_of)
}

/// Stamps the version directory at `path` as used now, by setting the mtime of its
/// [`LAST_USED_FILE`] to the current time.
///
/// This is a no-op when the file doesn't exist. Creating it here would be actively harmful: the
/// persistence layer reads `CURRENT` as a big-endian `u32` sequence number, so an empty file we
/// fabricated would fail that read and make the directory un-openable. A directory in that state is
/// either brand new (the persistence layer writes `CURRENT` as it initializes, which stamps it for
/// us moments later) or not a database, and [`time_since_last_used`] has its own fallback for
/// judging the age of one that lacks the file.
fn mark_used(path: &Path) -> Result<()> {
    // `write(true)` without `truncate`/`append`: we need a writable handle to set times, but must
    // not disturb the contents, which the persistence layer reads as its sequence number.
    let file = match OpenOptions::new()
        .write(true)
        .open(path.join(LAST_USED_FILE))
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    file.file()
        .set_times(FileTimes::new().set_modified(SystemTime::now()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rstest::rstest;
    use tempfile::TempDir;

    use super::*;

    const CURRENT_VERSION: &str = "mock-version";

    fn version_info() -> GitVersionInfo<'static> {
        GitVersionInfo {
            describe: CURRENT_VERSION,
            dirty: false,
        }
    }

    /// Sets a file's mtime to `ago` in the past.
    fn backdate(file: &Path, ago: Duration) {
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now() - ago))
            .unwrap();
    }

    /// Creates a version directory that looks like a real database (i.e. has a `CURRENT` file),
    /// last used `used_ago` in the past.
    fn create_version_dir(base_path: &Path, name: &str, used_ago: Duration) {
        let path = base_path.join(name);
        fs::create_dir(&path).unwrap();
        let current = path.join(LAST_USED_FILE);
        // 4 bytes, matching the big-endian u32 sequence number the persistence layer writes
        fs::write(&current, [0u8; 4]).unwrap();
        backdate(&current, used_ago);
    }

    fn entry_names(base_path: &Path) -> Vec<String> {
        let mut names = fs::read_dir(base_path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Only the most recently used other version survives, and the current version survives
    /// regardless of how stale it is. On CI no other version survives at all.
    #[rstest]
    #[case::not_ci(false, MAX_OTHER_DB_VERSIONS)]
    #[case::ci(true, 0)]
    fn test_max_versions(#[case] is_ci: bool, #[case] max_other_db_versions: usize) {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        // the current version is the least recently used, and should be preserved anyway
        create_version_dir(base_path, CURRENT_VERSION, Duration::from_secs(60 * 60));

        let num_other_dirs = max_other_db_versions + 3;
        for i in 0..num_other_dirs {
            // `other-dir-0` is the most recently used, so it's the one that should be retained
            create_version_dir(
                base_path,
                &format!("other-dir-{i}"),
                Duration::from_secs(i as u64 + 1),
            );
        }

        let versioned_path = handle_db_versioning(base_path, &version_info(), is_ci).unwrap();
        assert_eq!(versioned_path, base_path.join(CURRENT_VERSION));

        let mut expected = vec![CURRENT_VERSION.to_string()];
        expected.extend((0..max_other_db_versions).map(|i| format!("other-dir-{i}")));
        expected.sort();
        assert_eq!(entry_names(base_path), expected);
    }

    /// An other version that hasn't been used within the TTL is evicted even though it's within
    /// the count limit.
    #[test]
    fn test_ttl_evicts_unused_version() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        create_version_dir(base_path, CURRENT_VERSION, Duration::ZERO);
        create_version_dir(
            base_path,
            "stale-version",
            DEFAULT_OTHER_DB_VERSION_TTL + Duration::from_secs(60),
        );

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert_eq!(entry_names(base_path), vec![CURRENT_VERSION]);
    }

    /// An other version used within the TTL is retained.
    #[test]
    fn test_ttl_retains_recently_used_version() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        create_version_dir(base_path, CURRENT_VERSION, Duration::ZERO);
        create_version_dir(
            base_path,
            "recent-version",
            DEFAULT_OTHER_DB_VERSION_TTL - Duration::from_secs(60),
        );

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert_eq!(
            entry_names(base_path),
            vec!["mock-version", "recent-version"]
        );
    }

    /// A directory with no `CURRENT` file (e.g. written by a persistence layer that named its
    /// files differently) falls back to the newest mtime of its contents, so a recently used one
    /// is retained rather than thrown away for lacking the stamp we happen to look for.
    #[test]
    fn test_version_without_stamp_falls_back_to_contents() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        create_version_dir(base_path, CURRENT_VERSION, Duration::ZERO);

        let legacy = base_path.join("legacy-version");
        fs::create_dir(&legacy).unwrap();
        // no CURRENT, but a recently written data file
        fs::write(legacy.join("00000001.sst"), b"data").unwrap();

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert_eq!(
            entry_names(base_path),
            vec!["legacy-version", CURRENT_VERSION]
        );
    }

    /// The contents fallback still respects the TTL: a stampless directory whose files are all
    /// older than the TTL is evicted.
    #[test]
    fn test_version_without_stamp_still_expires() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        create_version_dir(base_path, CURRENT_VERSION, Duration::ZERO);

        let legacy = base_path.join("legacy-version");
        fs::create_dir(&legacy).unwrap();
        let sst = legacy.join("00000001.sst");
        fs::write(&sst, b"data").unwrap();
        backdate(&sst, DEFAULT_OTHER_DB_VERSION_TTL + Duration::from_secs(60));

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert_eq!(entry_names(base_path), vec![CURRENT_VERSION]);
    }

    /// A directory with no readable timestamp at all doesn't look like a database, and is evicted
    /// rather than occupying the single retention slot.
    #[test]
    fn test_empty_version_dir_is_evicted() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        create_version_dir(base_path, CURRENT_VERSION, Duration::ZERO);
        fs::create_dir(base_path.join("empty-dir")).unwrap();

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert_eq!(entry_names(base_path), vec![CURRENT_VERSION]);
    }

    /// A stampless directory must not have a `CURRENT` fabricated for it: the persistence layer
    /// reads that file as a `u32`, so an empty one would make the directory un-openable.
    #[test]
    fn test_mark_used_does_not_create_current() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.path().join("no-current");
        fs::create_dir(&path).unwrap();

        mark_used(&path).unwrap();

        assert!(!path.join(LAST_USED_FILE).exists());
    }

    /// Selecting a version refreshes its last-used stamp, so a version that would otherwise expire
    /// stays retained as long as it keeps being used.
    #[test]
    fn test_selecting_version_refreshes_stamp() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        let stale = DEFAULT_OTHER_DB_VERSION_TTL + Duration::from_secs(60);
        create_version_dir(base_path, CURRENT_VERSION, stale);

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        let age = fs::metadata(base_path.join(CURRENT_VERSION).join(LAST_USED_FILE))
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(age < Duration::from_secs(60), "stamp was not refreshed");

        // the contents must be left intact: the persistence layer reads them as a sequence number
        assert_eq!(
            fs::read(base_path.join(CURRENT_VERSION).join(LAST_USED_FILE)).unwrap(),
            [0u8; 4]
        );
    }

    #[test]
    fn test_cleanup_of_prefixed_items() {
        let tmp_dir = TempDir::new().unwrap();
        let base_path = tmp_dir.path();

        for i in 0..5 {
            fs::create_dir(base_path.join(format!("{DELETION_PREFIX}other-dir-{i}"))).unwrap();
        }

        handle_db_versioning(base_path, &version_info(), /* is_ci */ false).unwrap();

        assert!(entry_names(base_path).is_empty());
    }
}
