//! Who can change the policy file: only an administrator may.
//!
//! On Unix the file and its folder must be owned by root and writable by
//! no group or other user. On Windows the owner of each must be the
//! Administrators group, SYSTEM or TrustedInstaller, and no access-control
//! entry may give anyone else a right that changes or deletes them.
//!
//! Windows access lists are read as SDDL from `Get-Acl`, because SDDL names
//! principals by security identifier while `icacls` prints names that
//! change with the system language. PowerShell is started by its full
//! path with a cleared environment and no profile, so a user's `PATH`,
//! module path or profile cannot stand in a different answer.

use std::path::Path;

/// Checks the policy file and the folder that holds it.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// `Ok(())` when only an administrator can change either.
///
/// # Errors
///
/// A sentence saying which check failed and how to fix it.
pub fn check(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", path.display()))?;
    check_platform(path, dir)
}

/// The Unix checks.
#[cfg(unix)]
fn check_platform(path: &Path, dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    for target in [path, dir] {
        let meta = std::fs::metadata(target)
            .map_err(|error| format!("{} cannot be inspected: {error}", target.display()))?;
        if let Some(problem) = unix_problem(meta.uid(), meta.mode()) {
            return Err(format!(
                "{} {problem}; run: sudo chown root {} && sudo chmod go-w {}",
                target.display(),
                target.display(),
                target.display()
            ));
        }
    }
    Ok(())
}

/// The Windows checks.
#[cfg(windows)]
fn check_platform(path: &Path, dir: &Path) -> Result<(), String> {
    for target in [path, dir] {
        let sddl = read_sddl(target)?;
        let problems = sddl_problems(&sddl);
        if !problems.is_empty() {
            return Err(format!(
                "{} {}; only Administrators, SYSTEM and TrustedInstaller may own or change it (see docs/ADMIN_GUIDE.md)",
                target.display(),
                problems.join("; ")
            ));
        }
    }
    Ok(())
}

/// Other systems have no check anvil knows how to make, so it refuses.
#[cfg(not(any(unix, windows)))]
fn check_platform(path: &Path, _dir: &Path) -> Result<(), String> {
    Err(format!(
        "{}: anvil cannot check file ownership on this system",
        path.display()
    ))
}

/// Why a Unix owner and mode let someone other than root change a file.
///
/// # Arguments
///
/// * `uid` - The owner.
/// * `mode` - The mode bits.
///
/// # Returns
///
/// `None` when root owns it and neither group nor others may write it.
pub fn unix_problem(uid: u32, mode: u32) -> Option<String> {
    if uid != 0 {
        return Some(format!("is owned by uid {uid}, not root"));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "is writable by its group or by others (mode {:o})",
            mode & 0o777
        ));
    }
    None
}

/// The security identifiers allowed to own or change the policy:
/// Administrators, SYSTEM and TrustedInstaller, in SDDL short and long form.
const TRUSTED_SIDS: &[&str] = &[
    "BA",
    "S-1-5-32-544",
    "SY",
    "S-1-5-18",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// SDDL right codes that change, delete or take over a file or folder:
/// generic and file-wide all and write, delete, write DAC, write owner,
/// write data or add file, append data or add folder, write extended
/// attributes, delete child, write attributes.
const WRITE_CODES: &[&str] = &[
    "GA", "GW", "FA", "FW", "SD", "WD", "WO", "DC", "LC", "RP", "DT", "CR", "KA", "KW",
];

/// The same rights as access-mask bits: write data, append data, write
/// extended attributes, delete child, write attributes, delete, write DAC,
/// write owner, maximum allowed, generic all, generic write.
const WRITE_BITS: u32 = 0x2
    | 0x4
    | 0x10
    | 0x40
    | 0x100
    | 0x1_0000
    | 0x4_0000
    | 0x8_0000
    | 0x200_0000
    | 0x1000_0000
    | 0x4000_0000;

/// What in an SDDL string lets someone other than an administrator change
/// the object.
///
/// # Arguments
///
/// * `sddl` - The security descriptor, such as `Get-Acl` prints in `.Sddl`.
///
/// # Returns
///
/// One sentence per problem; empty when only trusted principals own or
/// can change it. A string anvil cannot read is itself a problem.
pub fn sddl_problems(sddl: &str) -> Vec<String> {
    let mut problems = Vec::new();
    match section(sddl, 'O') {
        Some(owner) if is_trusted(owner) => {}
        Some(owner) => problems.push(format!("is owned by {owner}")),
        None => problems.push("has no owner anvil can read".to_owned()),
    }
    let Some(dacl) = section(sddl, 'D') else {
        problems.push("has no access list, so everyone may change it".to_owned());
        return problems;
    };
    for ace in dacl.split('(').skip(1) {
        let ace = ace.trim_end_matches(')');
        let fields: Vec<&str> = ace.split(';').collect();
        if fields.len() < 6 {
            problems.push(format!("has an entry anvil cannot read: ({ace})"));
            continue;
        }
        let (kind, flags, rights, sid) = (fields[0], fields[1], fields[2], fields[5]);
        let allows = matches!(kind, "A" | "OA" | "XA" | "ZA");
        let inherit_only = flags.as_bytes().chunks(2).any(|flag| flag == b"IO");
        if allows && !inherit_only && grants_write(rights) && !is_trusted(sid) && sid != "CO" {
            problems.push(format!("grants {sid} the right to change it ({rights})"));
        }
    }
    problems
}

/// Whether a SID names an administrator principal.
fn is_trusted(sid: &str) -> bool {
    TRUSTED_SIDS
        .iter()
        .any(|trusted| trusted.eq_ignore_ascii_case(sid))
}

/// Whether an SDDL rights field includes a right that changes the object.
fn grants_write(rights: &str) -> bool {
    if let Some(hex) = rights
        .strip_prefix("0x")
        .or_else(|| rights.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).map_or(true, |mask| mask & WRITE_BITS != 0);
    }
    rights
        .as_bytes()
        .chunks(2)
        .any(|code| WRITE_CODES.iter().any(|write| write.as_bytes() == code))
}

/// The text of one SDDL section (`O:`, `G:`, `D:` or `S:`), up to the next.
fn section(sddl: &str, tag: char) -> Option<&str> {
    let start = sddl.find(&format!("{tag}:"))? + 2;
    let rest = &sddl[start..];
    let mut depth = 0usize;
    for (index, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            'O' | 'G' | 'D' | 'S' if depth == 0 && rest[index..].get(1..2) == Some(":") => {
                return Some(&rest[..index]);
            }
            _ => {}
        }
    }
    Some(rest)
}

/// Reads a file's or folder's security descriptor as SDDL.
#[cfg(windows)]
fn read_sddl(target: &Path) -> Result<String, String> {
    const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    let literal = target.display().to_string().replace('\'', "''");
    let output = std::process::Command::new(POWERSHELL)
        .env_clear()
        .env("SystemRoot", r"C:\Windows")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Acl -LiteralPath '{literal}').Sddl"),
        ])
        .output()
        .map_err(|error| format!("{}: cannot run {POWERSHELL}: {error}", target.display()))?;
    let sddl = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || sddl.is_empty() {
        return Err(format!(
            "{}: cannot read its access list: {}",
            target.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(sddl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_needs_root_and_no_group_or_other_write() {
        assert_eq!(unix_problem(0, 0o100644), None);
        assert_eq!(unix_problem(0, 0o40755), None);
        assert!(
            unix_problem(1000, 0o100644)
                .expect("problem")
                .contains("uid 1000")
        );
        assert!(
            unix_problem(0, 0o100664)
                .expect("problem")
                .contains("writable")
        );
        assert!(
            unix_problem(0, 0o100646)
                .expect("problem")
                .contains("writable")
        );
    }

    #[test]
    fn an_administrator_only_descriptor_passes() {
        let sddl =
            "O:BAG:SYD:PAI(A;OICIIO;GA;;;CO)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
        assert_eq!(sddl_problems(sddl), Vec::<String>::new());
        let hosts =
            "O:SYG:SYD:(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;0x1200a9;;;BU)(A;ID;0x1200a9;;;AC)";
        assert_eq!(sddl_problems(hosts), Vec::<String>::new());
    }

    #[test]
    fn a_user_owned_or_user_writable_descriptor_fails() {
        let user = "S-1-5-21-4158986379-1985627759-715112919-1001";
        let owned = format!("O:{user}G:{user}D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FA;;;{user})");
        let problems = sddl_problems(&owned);
        assert!(
            problems.iter().any(|p| p.contains("owned by")),
            "{problems:?}"
        );
        assert!(problems.iter().any(|p| p.contains(user)), "{problems:?}");
        // C:\ProgramData lets Users create files in every folder it holds.
        let program_data = "O:SYG:SYD:PAI(A;OICIIO;GA;;;CO)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)(A;CI;DCLCRPCR;;;BU)";
        let problems = sddl_problems(program_data);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("BU"));
        for rights in ["0x1301bf", "FW", "GA", "SD", "WD", "0x10000"] {
            let sddl = format!("O:BAD:(A;;{rights};;;AU)");
            assert_eq!(sddl_problems(&sddl).len(), 1, "{rights}");
        }
    }

    #[test]
    fn deny_and_inherit_only_entries_do_not_count() {
        let sddl = "O:BAD:(D;;FA;;;WD)(A;OICIIO;FA;;;BU)(A;;FA;;;BA)";
        assert_eq!(sddl_problems(sddl), Vec::<String>::new());
    }

    #[test]
    fn unreadable_descriptors_fail_closed() {
        assert!(!sddl_problems("").is_empty());
        assert!(!sddl_problems("O:BA").is_empty(), "no DACL");
        assert!(!sddl_problems("O:BAD:(A;;FA)").is_empty());
        assert!(!sddl_problems("O:BAD:(A;;0xZZ;;;BU)").is_empty());
    }

    #[test]
    fn a_file_the_current_user_created_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(super::super::POLICY_FILE);
        std::fs::write(&path, "version = 1\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(&path).expect("meta").uid() == 0 {
                return;
            }
        }
        let error = check(&path).expect_err("a user-owned file is not safe");
        assert!(!error.is_empty());
    }
}
