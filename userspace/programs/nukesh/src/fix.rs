// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Fixing the last command (ADR 0275), as thefuck does, but only ever as
//! a hint: after a command fails, the empty line shows it corrected,
//! faded; → takes it, and nothing runs until Enter.
//!
//! - A command not found that is a typo: the likely name (ADR 0259).
//! - A command not found that a bundle in the store's catalogue has:
//!   `bundle install ID` (`[commands] offer-install`).
//! - `cd` to a folder that is not there: the folder whose name is one
//!   typo or a case away.
//! - A command only the administrator may run: the same after `elevate`.

use crate::shell::Shell;

/// The catalogue's commands, a line each: `command TAB bundle TAB what
/// it is`. The store writes it when it fetches the catalogue; until the
/// store has one it is not there, and nothing is offered.
pub const CATALOGUE: &str = "/system/state/bundled/catalogue-commands";

/// Commands that only work for the administrator.
const ADMIN_ONLY: &[&str] = &[
    "mount", "umount", "fdisk", "sfdisk", "parted", "mkfs", "fsck", "wipefs", "modprobe", "reboot", "poweroff",
    "shutdown", "useradd", "userdel", "usermod", "groupadd", "chroot", "iptables", "nft", "sysctl",
];

/// The bundle that has `command` in `catalogue`, and what it is.
pub fn bundle_for(catalogue: &str, command: &str) -> Option<(String, String)> {
    catalogue.lines().find_map(|line| {
        let mut fields = line.split('\t');
        (fields.next()? == command).then(|| {
            let id = fields.next().unwrap_or_default().to_string();
            (id, fields.next().unwrap_or_default().to_string())
        })
    }).filter(|(id, _)| !id.is_empty())
}

/// The catalogue's bundle for `command`, read now.
pub fn offer(shell: &Shell, command: &str) -> Option<(String, String)> {
    if !shell.settings.offer_install || command.contains('/') {
        return None;
    }
    let path = shell.get("NUKESH_CATALOGUE").unwrap_or_else(|| CATALOGUE.into());
    let text = std::fs::read_to_string(path).ok()?;
    bundle_for(&text, command)
}

/// The words of `line`, split on blanks: enough for the simple commands
/// a fix is for.
fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

/// The last command corrected, if a rule knows how.
pub fn fix(shell: &Shell, last: &str, status: i32) -> Option<String> {
    let words = words(last);
    let first = *words.first()?;
    // Pipelines and lists are left alone: which part failed is not known.
    if last.contains(['|', ';', '&', '(', '`', '$', '\n']) {
        return None;
    }
    let rest = || words[1..].join(" ");
    let joined = |head: &str| if words.len() > 1 { format!("{head} {}", rest()) } else { head.to_string() };
    match status {
        127 => {
            if let Some(likely) = crate::suggest::did_you_mean(shell, first) {
                return Some(joined(&likely));
            }
            let (id, _) = offer(shell, first)?;
            Some(format!("bundle install {id}"))
        }
        1 if first == "cd" && words.len() == 2 => {
            let target = words[1];
            if std::fs::metadata(target).is_ok() {
                return None;
            }
            let (dir, base) = match target.trim_end_matches('/').rsplit_once('/') {
                Some((dir, base)) => (if dir.is_empty() { "/" } else { dir }, base),
                None => (".", target.trim_end_matches('/')),
            };
            let mut found: Vec<String> = std::fs::read_dir(dir)
                .ok()?
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| {
                    name.eq_ignore_ascii_case(base)
                        || (base.chars().count() >= 3 && crate::suggest::distance(&base.to_lowercase(), &name.to_lowercase()) <= 1)
                })
                .collect();
            found.sort();
            let name = found.first()?;
            let fixed = match target.trim_end_matches('/').rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{name}"),
                None => name.clone(),
            };
            Some(format!("cd {fixed}"))
        }
        s if s != 0 && s != 130 && ADMIN_ONLY.contains(&first.rsplit('/').next().unwrap_or(first)) => {
            (crate::sys::geteuid() != 0).then(|| format!("elevate {last}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_names_bundles() {
        let catalogue = "vim\torg.vim.Vim\tVi IMproved\nhtop\tdev.htop.Htop\t\n";
        assert_eq!(bundle_for(catalogue, "vim"), Some(("org.vim.Vim".into(), "Vi IMproved".into())));
        assert_eq!(bundle_for(catalogue, "htop").map(|b| b.0).as_deref(), Some("dev.htop.Htop"));
        assert_eq!(bundle_for(catalogue, "emacs"), None);
    }

    #[test]
    fn folders_one_typo_away_are_found() {
        let dir = std::env::temp_dir().join(format!("nukesh-fix-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Projects")).unwrap();
        let shell = Shell::new("test", false);
        let d = dir.to_str().unwrap();
        assert_eq!(fix(&shell, &format!("cd {d}/projects"), 1), Some(format!("cd {d}/Projects")));
        assert_eq!(fix(&shell, &format!("cd {d}/Porjects"), 1), Some(format!("cd {d}/Projects")));
        assert_eq!(fix(&shell, &format!("cd {d}/Elsewhere"), 1), None);
        assert_eq!(fix(&shell, "ls | wc", 127), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
