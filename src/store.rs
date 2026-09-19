use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::io;
use crate::{Error, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SENTINEL: &str = "PRIVATE FORGE CACHE\nThis directory may contain private data. Do not copy it into a repository, commit it, or publish it.\n";

pub struct Store {
    root: PathBuf,
    permissions: CachePermissions,
}

#[derive(Clone, Copy)]
struct CachePermissions {
    directory: u32,
    file: u32,
}

const PRIVATE_PERMISSIONS: CachePermissions = CachePermissions {
    directory: 0o700,
    file: 0o600,
};

pub struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        validate_absolute_no_symlinks(&root)?;
        let permissions = if root.exists() {
            if !root.is_dir() {
                return Err(Error::Inconsistent(
                    "output path is not a directory".to_owned(),
                ));
            }
            cache_permissions(&root)?
        } else {
            create_dir_all_with_mode(&root, PRIVATE_PERMISSIONS.directory)?;
            let permissions = cache_permissions(&root)?;
            if permissions.directory != PRIVATE_PERMISSIONS.directory {
                return Err(unsupported_cache_permissions(&root, permissions.directory));
            }
            PRIVATE_PERMISSIONS
        };
        let store = Self { root, permissions };
        store.atomic_write_bytes(Path::new("README-PRIVATE.txt"), SENTINEL.as_bytes())?;
        Ok(store)
    }

    pub fn open_existing(root: PathBuf) -> Result<Self> {
        validate_absolute_no_symlinks(&root)?;
        if !root.is_dir() {
            return Err(Error::Inconsistent(
                "output directory does not exist".to_owned(),
            ));
        }
        let permissions = cache_permissions(&root)?;
        Ok(Self { root, permissions })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock(&self) -> Result<Lock> {
        let path = self.root.join(".forge-sync.lock");
        reject_symlink(&path)?;
        let file = open_with_mode(&path, self.permissions.file)?;
        file.try_lock_exclusive().map_err(|_| Error::Locked)?;
        Ok(Lock { file })
    }

    pub fn create_dir(&self, relative: impl AsRef<Path>) -> Result<()> {
        let path = self.safe_path(relative.as_ref())?;
        create_dir_all_with_mode(&path, self.permissions.directory)
    }

    pub fn exists(&self, relative: impl AsRef<Path>) -> bool {
        self.safe_path(relative.as_ref())
            .is_ok_and(|path| path.is_file())
    }

    pub fn read_json<T: DeserializeOwned>(&self, relative: impl AsRef<Path>) -> Result<Option<T>> {
        let path = self.safe_path(relative.as_ref())?;
        if !path.exists() {
            return Ok(None);
        }
        reject_symlink(&path)?;
        let mut data = Vec::new();
        File::open(&path)
            .map_err(io(format!("opening {}", path.display())))?
            .read_to_end(&mut data)
            .map_err(io(format!("reading {}", path.display())))?;
        serde_json::from_slice(&data)
            .map(Some)
            .map_err(|_| Error::Json {
                context: path.display().to_string(),
            })
    }

    pub fn atomic_write_json<T: Serialize + ?Sized>(
        &self,
        relative: impl AsRef<Path>,
        value: &T,
    ) -> Result<()> {
        let mut data = serde_json::to_vec_pretty(value).map_err(|_| Error::Json {
            context: "serializing local data".to_owned(),
        })?;
        data.push(b'\n');
        self.atomic_write_bytes(relative.as_ref(), &data)
    }

    fn atomic_write_bytes(&self, relative: &Path, data: &[u8]) -> Result<()> {
        let path = self.safe_path(relative)?;
        let parent = path.parent().ok_or_else(|| Error::UnsafePath {
            path: path.clone(),
            reason: "missing parent".to_owned(),
        })?;
        create_dir_all_with_mode(parent, self.permissions.directory)?;
        validate_absolute_no_symlinks(parent)?;
        reject_symlink(&path)?;
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".forge-sync-{}-{id}.tmp", std::process::id()));
        let result = (|| {
            let mut file = create_new_with_mode(&temp, self.permissions.file)?;
            file.write_all(data)
                .map_err(io(format!("writing {}", temp.display())))?;
            file.sync_all()
                .map_err(io(format!("syncing {}", temp.display())))?;
            fs::rename(&temp, &path).map_err(io(format!("replacing {}", path.display())))?;
            File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(io(format!("syncing {}", parent.display())))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn safe_path(&self, relative: &Path) -> Result<PathBuf> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::UnsafePath {
                path: relative.to_owned(),
                reason: "path is not a safe relative path".to_owned(),
            });
        }
        Ok(self.root.join(relative))
    }
}

fn validate_absolute_no_symlinks(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::UnsafePath {
            path: path.to_owned(),
            reason: "path is not absolute".to_owned(),
        });
    }
    let mut current = PathBuf::from("/");
    for component in path.components() {
        if matches!(component, Component::RootDir) {
            continue;
        }
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::UnsafePath {
                path: path.to_owned(),
                reason: "non-normal component".to_owned(),
            });
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::UnsafePath {
                    path: current,
                    reason: "symlink component".to_owned(),
                })
            }
            Ok(metadata) if current != path && !metadata.is_dir() => {
                return Err(Error::UnsafePath {
                    path: current,
                    reason: "non-directory component".to_owned(),
                })
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(Error::Io {
                    context: format!("inspecting {}", current.display()),
                    source,
                })
            }
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::UnsafePath {
            path: path.to_owned(),
            reason: "symlink".to_owned(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            context: format!("inspecting {}", path.display()),
            source,
        }),
    }
}

fn create_dir_all_with_mode(path: &Path, mode: u32) -> Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor.to_owned());
        cursor = cursor.parent().ok_or_else(|| Error::UnsafePath {
            path: path.to_owned(),
            reason: "no existing ancestor".to_owned(),
        })?;
    }
    validate_absolute_no_symlinks(cursor)?;
    for dir in missing.iter().rev() {
        fs::create_dir(dir).map_err(io(format!("creating {}", dir.display())))?;
        set_mode(dir, mode)?;
    }
    validate_absolute_no_symlinks(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io(format!("setting permissions on {}", path.display())))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &File, path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(io(format!("setting permissions on {}", path.display())))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File, _path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn cache_permissions(path: &Path) -> Result<CachePermissions> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(io(format!("inspecting {}", path.display())))?;
    let directory = metadata.permissions().mode() & 0o7777;
    match directory {
        0o700 | 0o750 | 0o770 => {}
        _ => return Err(unsupported_cache_permissions(path, directory)),
    }
    let file = directory & 0o660;
    Ok(CachePermissions { directory, file })
}

#[cfg(not(unix))]
fn cache_permissions(_path: &Path) -> Result<CachePermissions> {
    Ok(PRIVATE_PERMISSIONS)
}

fn unsupported_cache_permissions(path: &Path, mode: u32) -> Error {
    Error::Inconsistent(format!(
        "cache root {} has unsupported permissions {mode:04o}; expected 0700, 0750, or 0770",
        path.display()
    ))
}

fn open_with_mode(path: &Path, mode: u32) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let file = options
        .open(path)
        .map_err(io(format!("opening {}", path.display())))?;
    set_file_mode(&file, path, mode)?;
    Ok(file)
}

fn create_new_with_mode(path: &Path, mode: u32) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let file = options
        .open(path)
        .map_err(io(format!("creating {}", path.display())))?;
    set_file_mode(&file, path, mode)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: impl AsRef<Path>) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_test_mode(path: impl AsRef<Path>, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn private_and_atomic_and_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let store = Store::open(root.clone()).unwrap();
        store
            .atomic_write_json("nested/value.json", &serde_json::json!({"x": 1}))
            .unwrap();
        let _lock = store.lock().unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(root.join("nested")), 0o700);
        assert_eq!(mode(root.join("nested/value.json")), 0o600);
        assert_eq!(mode(root.join("README-PRIVATE.txt")), 0o600);
        assert_eq!(mode(root.join(".forge-sync.lock")), 0o600);
        symlink(temp.path(), root.join("bad")).unwrap();
        assert!(store.atomic_write_json("bad/oops.json", &1).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn derives_group_readable_permissions_from_existing_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir(&root).unwrap();
        set_test_mode(&root, 0o750);

        let store = Store::open(root.clone()).unwrap();
        store.atomic_write_json("one/two/value.json", &1).unwrap();
        let _lock = store.lock().unwrap();

        assert_eq!(mode(&root), 0o750);
        assert_eq!(mode(root.join("one")), 0o750);
        assert_eq!(mode(root.join("one/two")), 0o750);
        assert_eq!(mode(root.join("one/two/value.json")), 0o640);
        assert_eq!(mode(root.join("README-PRIVATE.txt")), 0o640);
        assert_eq!(mode(root.join(".forge-sync.lock")), 0o640);
    }

    #[test]
    #[cfg(unix)]
    fn derives_group_writable_permissions_and_atomic_replacement_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir(&root).unwrap();
        set_test_mode(&root, 0o770);

        let store = Store::open(root.clone()).unwrap();
        store.atomic_write_json("nested/value.json", &1).unwrap();
        set_test_mode(root.join("nested/value.json"), 0o600);
        store.atomic_write_json("nested/value.json", &2).unwrap();

        assert_eq!(mode(root.join("nested")), 0o770);
        assert_eq!(mode(root.join("nested/value.json")), 0o660);
        assert_eq!(mode(root.join("README-PRIVATE.txt")), 0o660);
    }

    #[test]
    #[cfg(unix)]
    fn rejects_world_accessible_and_unsupported_root_modes() {
        for unsupported in [0o701, 0o710, 0o740, 0o755] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("cache");
            fs::create_dir(&root).unwrap();
            set_test_mode(&root, unsupported);
            assert!(matches!(Store::open(root), Err(Error::Inconsistent(_))));
        }
    }

    #[test]
    #[cfg(unix)]
    fn does_not_change_existing_nested_directory_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let existing = root.join("existing");
        fs::create_dir(&root).unwrap();
        set_test_mode(&root, 0o750);
        fs::create_dir(&existing).unwrap();
        set_test_mode(&existing, 0o700);

        let store = Store::open(root).unwrap();
        store.atomic_write_json("existing/value.json", &1).unwrap();

        assert_eq!(mode(&existing), 0o700);
        assert_eq!(mode(existing.join("value.json")), 0o640);
    }

    #[test]
    fn locking_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("cache")).unwrap();
        let _lock = store.lock().unwrap();
        assert!(matches!(store.lock(), Err(Error::Locked)));
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_output_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("cache");
        symlink(&target, &link).unwrap();
        assert!(matches!(Store::open(link), Err(Error::UnsafePath { .. })));
    }
}
