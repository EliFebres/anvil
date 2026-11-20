//! Creating anvil's own files and folders so only their owner can open
//! them.
//!
//! On Unix every folder is created with mode 0700 and every file with
//! 0600, whatever the umask; a file or folder that already exists is
//! narrowed when anvil reuses it, so one left wider by an earlier version
//! or by hand is fixed on first use. On Windows a new file inherits the access list
//! of its folder, which for a folder under the user's profile is already
//! limited to the user, SYSTEM and Administrators; `anvil doctor` checks
//! the access lists of `~/.anvil` and the project's `.anvil` instead.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Creates a folder and its missing parents, owner-only on Unix. When the
/// folder already exists it is narrowed to 0700; existing parents are left
/// alone, since they may be the user's own folders.
///
/// # Arguments
///
/// * `dir` - The folder.
///
/// # Errors
///
/// When a folder cannot be created or narrowed.
pub fn create_dir_all(dir: impl AsRef<Path>) -> std::io::Result<()> {
    let dir = dir.as_ref();
    if dir.is_dir() {
        return narrow_dir(dir);
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

/// Open options that create a file owner-only (0600) on Unix, for callers
/// that need flags [`write`] and [`open_append`] do not set.
///
/// # Returns
///
/// The options, with no access mode chosen yet.
pub fn owner_only() -> OpenOptions {
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

/// Narrows an open file to its owner (0600) on Unix; nothing elsewhere.
/// Callers that open through [`owner_only`] call it, because the mode
/// given there applies only to a file the open creates.
///
/// # Arguments
///
/// * `file` - The open file.
///
/// # Errors
///
/// When the mode cannot be changed.
pub fn narrow(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// Narrows a file another program wrote, such as git's index file, to its
/// owner (0600) on Unix; nothing elsewhere.
///
/// # Arguments
///
/// * `path` - The file; a missing one is not an error.
///
/// # Errors
///
/// When the mode of an existing file cannot be changed.
pub fn narrow_path(path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    if !path.is_file() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Narrows an existing folder to 0700 on Unix when others may open it.
fn narrow_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Copies a file and makes the copy owner-only on Unix: 0600, or 0700
/// when the source is executable by its owner, so a copied script still
/// runs.
///
/// # Arguments
///
/// * `from` - The source file.
/// * `to` - The copy; its folder must exist.
///
/// # Errors
///
/// When the file cannot be copied or its mode set.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    std::fs::copy(from, to)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(from)?.permissions().mode() & 0o100 != 0;
        let mode = if executable { 0o700 } else { 0o600 };
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode))?;
    }
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
pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    let mut file = owner_only()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    narrow(&file)?;
    file.write_all(bytes.as_ref())
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
pub fn open_append(path: impl AsRef<Path>) -> std::io::Result<File> {
    let path = path.as_ref();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all(parent)?;
    }
    let file = owner_only().create(true).append(true).open(path)?;
    narrow(&file)?;
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
        let mut log = open_append(dir.path().join("c").join("log")).expect("open");
        log.write_all(b"x").expect("append");
        drop(log);
        let mut log = open_append(dir.path().join("c").join("log")).expect("reopen");
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

    #[cfg(unix)]
    #[test]
    fn an_existing_folder_is_narrowed_and_a_copy_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        let wide = dir.path().join("wide");
        std::fs::create_dir(&wide).expect("mkdir");
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        create_dir_all(&wide).expect("reuse");
        assert_eq!(mode(&wide), 0o700, "an existing folder is narrowed");

        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "x").expect("write");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        copy(&plain, wide.join("plain.txt")).expect("copy");
        copy(&script, wide.join("run.sh")).expect("copy");
        assert_eq!(mode(&wide.join("plain.txt")), 0o600);
        assert_eq!(mode(&wide.join("run.sh")), 0o700);

        let written = dir.path().join("index");
        std::fs::write(&written, "x").expect("write");
        std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        narrow_path(&written).expect("narrow");
        assert_eq!(mode(&written), 0o600);
        narrow_path(dir.path().join("missing")).expect("a missing file is fine");
    }
}
