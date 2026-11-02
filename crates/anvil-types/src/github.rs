//! What both binaries pass to git so it can reach GitHub with a token.
//!
//! The factory and `anvil doctor` push over HTTPS from machines that have no
//! stored credential. Instead of writing the token into a config file, every
//! remote command carries it on its own command line, which is the only place
//! it ever exists.

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;

/// The `-c` arguments that make one git command authenticate with a token.
///
/// The first pair empties `credential.helper`, which disables every helper
/// configured on the machine (the Windows credential manager included) so git
/// neither prompts nor stores anything. The second sets the header GitHub
/// accepts for a token, `AUTHORIZATION: basic <base64 of x-access-token:TOKEN>`.
/// Put the result before the subcommand; never write it to a config file or a
/// log.
///
/// # Arguments
///
/// * `token` - A GitHub token.
///
/// # Returns
///
/// Four arguments: `-c`, `credential.helper=`, `-c` and the header setting.
pub fn git_auth_args(token: &str) -> [String; 4] {
    let encoded = BASE64_STANDARD.encode(format!("x-access-token:{token}"));
    [
        "-c".to_owned(),
        "credential.helper=".to_owned(),
        "-c".to_owned(),
        format!("http.extraheader=AUTHORIZATION: basic {encoded}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_args_disable_helpers_and_carry_the_basic_header() {
        // `printf 'x-access-token:fake-token' | base64`
        let args = git_auth_args("fake-token");
        assert_eq!(
            args,
            [
                "-c",
                "credential.helper=",
                "-c",
                "http.extraheader=AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46ZmFrZS10b2tlbg==",
            ]
        );
        assert!(args.iter().all(|arg| !arg.contains('\n')));
        assert!(args.iter().all(|arg| !arg.contains("fake-token")));
    }
}
