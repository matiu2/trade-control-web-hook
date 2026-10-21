use clap::Args;
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use std::{
    io::{self, BufRead, IsTerminal},
    path::PathBuf,
};

/// Shared options for MT5 registration and credential rotation. Passwords are
/// deliberately excluded from argv, environment variables, and Debug output.
#[derive(Args, Default)]
pub struct Options {
    /// MT5 account number; defaults to the existing named profile's login.
    #[arg(long)]
    pub login: Option<u64>,
    /// Exact MT5 server name. Required for a new MT5 profile.
    #[arg(long)]
    pub server: Option<String>,
    /// Explicit MT5 server clock, e.g. +03:00. Required for a new profile.
    #[arg(long, allow_hyphen_values = true)]
    pub server_time: Option<String>,
    /// Absolute CandleBridge mailbox directory. Required for a new profile.
    #[arg(long)]
    pub mailbox: Option<PathBuf>,
    /// Absolute private credential file; reuses the existing profile's path,
    /// or defaults to mt5-credentials/<name>.toml beside mt5-accounts.toml.
    #[arg(long)]
    pub credentials_file: Option<PathBuf>,
    /// Read one password line from stdin instead of prompting without echo.
    #[arg(long)]
    pub password_stdin: bool,
}

impl Options {
    pub fn is_present(&self) -> bool {
        self.login.is_some()
            || self.server.is_some()
            || self.server_time.is_some()
            || self.mailbox.is_some()
            || self.credentials_file.is_some()
            || self.password_stdin
    }

    pub fn password(&self) -> Result<String> {
        if self.password_stdin {
            return read_password(&mut io::stdin().lock());
        }
        ensure!(
            io::stdin().is_terminal(),
            "MT5 password needs an interactive terminal; use --password-stdin for piped input"
        );
        rpassword::prompt_password("MT5 trading password: ").wrap_err("reading MT5 password")
    }
}

fn read_password(input: &mut impl BufRead) -> Result<String> {
    let mut password = String::new();
    input
        .read_line(&mut password)
        .wrap_err("reading MT5 password from stdin")?;
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    ensure!(!password.is_empty(), "MT5 password must be nonempty");
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_preserves_spaces_and_reads_only_one_line() {
        crate::tests::init();
        assert_eq!(
            read_password(&mut &b" secret \r\nignored"[..]).unwrap(),
            " secret "
        );
        assert_eq!(read_password(&mut &b"secret"[..]).unwrap(), "secret");
        assert!(read_password(&mut &b"\n"[..]).is_err());
        assert!(read_password(&mut &b""[..]).is_err());
    }
}
