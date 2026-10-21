// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running external programs and reporting progress.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};

use crate::{Ctx, Error, Result};

/// Prints a progress line for a step the user is waiting on.
pub fn step(message: &str) {
    eprintln!("==> {message}");
}

/// Renders a command the way a user would type it.
pub fn render(cmd: &Command) -> String {
    let mut out = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        let arg = arg.to_string_lossy();
        out.push(' ');
        if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || "'\"$;&|".contains(c)) {
            out.push('\'');
            out.push_str(&arg.replace('\'', r"'\''"));
            out.push('\'');
        } else {
            out.push_str(&arg);
        }
    }
    out
}

/// Runs `cmd` with inherited standard streams and returns its exit status.
pub fn status(ctx: &Ctx, cmd: &mut Command) -> Result<ExitStatus> {
    if ctx.verbose {
        eprintln!("+ {}", render(cmd));
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.status().map_err(|err| spawn_error(&program, err))
}

/// Runs `cmd` with inherited standard streams and fails on a non-zero exit.
pub fn run(ctx: &Ctx, cmd: &mut Command) -> Result<()> {
    let status = status(ctx, cmd)?;
    if !status.success() {
        let mut err = Error::new(format!("`{}` failed ({status})", render(cmd)));
        if !ctx.verbose {
            err = err.hint("the output above shows why; add --verbose to see every command");
        }
        return Err(err);
    }
    Ok(())
}

/// Runs `cmd`, captures its output and fails on a non-zero exit.
pub fn output(ctx: &Ctx, cmd: &mut Command) -> Result<Output> {
    if ctx.verbose {
        eprintln!("+ {}", render(cmd));
    }
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd.output().map_err(|err| spawn_error(&program, err))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(format!(
            "`{}` failed ({}):\n{}",
            render(cmd),
            output.status,
            stderr.trim_end()
        )));
    }
    Ok(output)
}

fn spawn_error(program: &str, err: std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        Error::new(format!("`{program}` is not installed"))
            .hint("run `./ob doctor` to see what is missing")
    } else {
        Error::new(format!("cannot run `{program}`: {err}"))
    }
}

/// Finds `name` on `PATH`, like `command -v`.
pub fn find_program(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Returns the first of `names` found on `PATH`.
pub fn find_any(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(find_program)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A `cargo` command that runs in the repository root and writes to
/// [`Ctx::target`].
pub fn cargo(ctx: &Ctx) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(&ctx.root).env("CARGO_TARGET_DIR", &ctx.target);
    // Flags meant for one build must not leak into builds for other targets.
    for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"] {
        cmd.env_remove(var);
    }
    cmd
}

/// Creates `path` and its parents.
pub fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|err| Error::new(format!("creating {}: {err}", path.display())))
}

/// Copies `from` to `to`, creating the destination directory.
pub fn copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        create_dir(parent)?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|err| Error::new(format!("copying {} to {}: {err}", from.display(), to.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_shell_quoting() {
        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.args(["-m", "1G", "-name", "OrthrOS test", "it's"]);
        assert_eq!(render(&cmd), r"qemu-system-x86_64 -m 1G -name 'OrthrOS test' 'it'\''s'");
    }
}
