//! Creating anvil's own files and folders so only their owner can open
//! them.
//!
//! On Unix every folder is created with mode 0700 and every file with
//! 0600, whatever the umask; a file that already exists is narrowed to
//! 0600 when it is written. On Windows a new file inherits the access list
//! of its folder, which for a folder under the user's profile is already
//! limited to the user, SYSTEM and Administrators; `anvil doctor` checks
//! the access lists of `~/.anvil` and the project's `.anvil` instead.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Creates a folder and its missing parents, owner-only on Unix.
///
/// # Arguments
///
/// * `dir` - The folder.
///
/// # Errors
///
/// When a folder cannot be created.
pub fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Options that create a file owner-only on Unix.
fn options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.mode(0o600);
        options
    }
    #[cfg(not(unix))]
    OpenOptions::new()
}

/// Narrows an existing file to its owner on Unix; nothing elsewhere.
fn restrict(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Writes a whole file, creating its folder, owner-only on Unix.
///
/// # Arguments
///
/// * `path` - The file.
/// * `bytes` - Its new contents.
///
/// # Errors
///
/// When the folder or file cannot be written.
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    let mut file = options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    restrict(&file)?;
    file.write_all(bytes)
}

/// Opens a file for appending, creating it and its folder, owner-only on
/// Unix.
///
/// # Arguments
///
/// * `path` - The file.
///
/// # Returns
///
/// The file, positioned at the end.
///
/// # Errors
///
/// When the folder or file cannot be created.
pub fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    let file = options().create(true).append(true).open(path)?;
    restrict(&file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_and_folders_are_created_with_their_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a").join("b").join("f.txt");
        write(&file, b"one").expect("write");
        write(&file, b"two").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "two");
        let mut log = open_append(&dir.path().join("c").join("log")).expect("open");
        log.write_all(b"x").expect("append");
        drop(log);
        let mut log = open_append(&dir.path().join("c").join("log")).expect("reopen");
        log.write_all(b"y").expect("append");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c").join("log")).expect("read"),
            "xy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_are_0600_and_folders_0700_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("new").join("f");
        std::fs::create_dir_all(dir.path().join("old")).expect("mkdir");
        let loose = dir.path().join("old").join("g");
        std::fs::write(&loose, "x").expect("write");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write(&file, b"x").expect("write");
        write(&loose, b"y").expect("write");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&loose), 0o600, "an existing file is narrowed");
        assert_eq!(mode(&dir.path().join("new")), 0o700);
    }
}
