//! Read-only desktop frontend and service connection helpers.
pub mod backend;

use std::path::{Path, PathBuf};

pub fn find_service_executable(
    gui: &Path,
    override_path: Option<&Path>,
    search_path: &std::ffi::OsStr,
) -> Option<PathBuf> {
    if let Some(path) = override_path
        && executable(path)
    {
        return Some(path.to_path_buf());
    }
    if let Some(path) = gui.parent().map(|dir| dir.join("callboard"))
        && executable(&path)
    {
        return Some(path);
    }
    std::env::split_paths(search_path)
        .map(|dir| dir.join("callboard"))
        .find(|path| executable(path))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::find_service_executable;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn executable(path: &Path) {
        fs::write(path, "fixture").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn service_binary_lookup_prefers_override_then_sibling_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let gui = bin.join("callboard-gui");
        executable(&gui);
        let sibling = bin.join("callboard");
        let pathdir = dir.path().join("path");
        fs::create_dir(&pathdir).unwrap();
        let in_path = pathdir.join("callboard");
        executable(&in_path);
        let locate = |override_path: Option<&Path>, path: &std::ffi::OsStr| {
            find_service_executable(&gui, override_path, path)
        };
        assert_eq!(locate(None, pathdir.as_os_str()), Some(in_path.clone()));
        executable(&sibling);
        assert_eq!(locate(None, pathdir.as_os_str()), Some(sibling.clone()));
        let custom = dir.path().join("custom");
        executable(&custom);
        assert_eq!(locate(Some(&custom), pathdir.as_os_str()), Some(custom));
        assert_eq!(
            locate(None, std::ffi::OsStr::new("")),
            Some(sibling.clone())
        );
        fs::remove_file(&sibling).unwrap();
        fs::remove_file(&in_path).unwrap();
        assert_eq!(locate(None, std::ffi::OsStr::new("")), None);
    }
}
