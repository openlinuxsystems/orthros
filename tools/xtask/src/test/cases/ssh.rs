// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! OpenSSH's client against a real server (ADR 0289). QEMU's user
//! network runs the host's sshd for each connection to 10.0.2.100 port
//! 22, in inetd mode (`sshd -i`), with a host key, settings and a
//! repository the test makes in out/test/ssh, and tells the host's user
//! name, whom it signs in as, on port 79. _/system/tests/posix/sshtest_
//! makes an Ed25519 key with ssh-keygen, which the test puts in the
//! server's authorized keys once the guest shows it; signs a file and a
//! git commit with the key and checks them; keeps the key in ssh-agent;
//! signs in, taking the host key the first time and finding it the
//! next, and refuses a host whose key changed; reads the host key with
//! ssh-keyscan; copies files both ways with scp and sftp; and clones the
//! host's repository over ssh, commits and pushes, and the host finds the
//! commit pushed in its repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::*;

/// Where the server's files are, from the top of the tree, which QEMU
/// runs in.
const DIR: &str = "out/test/ssh";

/// The test's network: QEMU's user network, with the host's sshd at
/// 10.0.2.100 port 22 and the host user's name at port 79.
const NETDEV: &str = "user,id=n0,guestfwd=tcp:10.0.2.100:22-cmd:out/test/ssh/serve,\
                      guestfwd=tcp:10.0.2.100:79-cmd:out/test/ssh/whoami";

pub const CASE: Case = Case {
    name: "ssh",
    about: "OpenSSH's client signs in to the host's sshd through QEMU's network: keys made, signed with and kept in ssh-agent, the host's key taken and a changed one refused, files copied with scp and sftp, and a repository cloned and pushed to over ssh",
    firmware: Firmware::Uefi,
    media: Media::Usb,
    cmdline: "lockdep ip=10.0.2.15/24 gateway=10.0.2.2 -- --posix --network /system/tests/posix/sshtest",
    cpus: 2,
    memory: "1G",
    reboot: false,
    image: TestImage::Plain,
    cpu_model: None,
    qemu: &["-netdev", NETDEV, "-device", "virtio-net-pci,netdev=n0"],
    script: ssh,
    disks: &[],
    second: None,
};

fn write(path: &Path, text: &str, executable: bool) -> Result<(), String> {
    std::fs::write(path, text).map_err(|err| format!("writing {}: {err}", path.display()))?;
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("making {} a program: {err}", path.display()))?;
    }
    Ok(())
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Host")
        .env("GIT_AUTHOR_EMAIL", "host@dukovo")
        .env("GIT_COMMITTER_NAME", "Host")
        .env("GIT_COMMITTER_EMAIL", "host@dukovo")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|err| format!("running {program}: {err}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The server's files: its host key, settings, the command it runs for
/// each session, the programs QEMU runs, a file to fetch, and a bare
/// repository with one commit.
fn serve(dir: &Path) -> Result<(), String> {
    let sshd = Path::new("/usr/sbin/sshd");
    if !sshd.exists() {
        return Err("the ssh test needs the host's sshd: install openssh-server".into());
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|err| format!("removing {}: {err}", dir.display()))?;
    }
    std::fs::create_dir_all(dir.join("work"))
        .map_err(|err| format!("making {}: {err}", dir.display()))?;
    let at = dir.display();
    run(dir, "ssh-keygen", &["-q", "-t", "ed25519", "-N", "", "-f", "host_key"])?;
    write(&dir.join("authorized_keys"), "", false)?;
    write(
        &dir.join("sshd_config"),
        &format!(
            "HostKey {at}/host_key\nAuthorizedKeysFile {at}/authorized_keys\nStrictModes no\n\
             UsePAM no\nPermitRootLogin yes\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nForceCommand {at}/command\n\
             Subsystem sftp internal-sftp\nPidFile none\nLogLevel VERBOSE\n"
        ),
        false,
    )?;
    // Each session runs in the repository's folder: a command as asked,
    // sftp's server for sftp and scp, and a greeting for a shell.
    write(
        &dir.join("command"),
        &format!(
            "#!/bin/sh\ncd {at}/work || exit 1\ncase $SSH_ORIGINAL_COMMAND in\n\
             \"\") echo \"hello from the test's sshd\" ;;\n\
             *sftp*) for s in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server \
             /usr/lib/ssh/sftp-server; do [ -x $s ] && exec $s; done; exit 1 ;;\n\
             *) exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" ;;\nesac\n"
        ),
        true,
    )?;
    // As root, sshd wants its privilege separation folder.
    write(
        &dir.join("serve"),
        &format!(
            "#!/bin/sh\n[ \"$(id -u)\" = 0 ] && mkdir -p /run/sshd\n\
             exec /usr/sbin/sshd -i -f {at}/sshd_config -E {at}/sshd.log\n"
        ),
        true,
    )?;
    write(&dir.join("whoami"), "#!/bin/sh\nexec id -un\n", true)?;
    let work = dir.join("work");
    write(&work.join("greeting"), "from the host\n", false)?;
    let seed = dir.join("seed");
    run(dir, "git", &["init", "-q", "-b", "main", "seed"])?;
    write(&seed.join("file"), "from the host\n", false)?;
    run(&seed, "git", &["add", "file"])?;
    run(&seed, "git", &["commit", "-q", "-m", "First"])?;
    run(dir, "git", &["clone", "-q", "--bare", "seed", "work/repo.git"])?;
    Ok(())
}

fn ssh(vm: &mut Vm, case: &Case) -> Result<(), String> {
    let root: PathBuf = std::env::current_dir().map_err(|err| err.to_string())?;
    let dir = root.join(DIR);
    serve(&dir)?;
    boot_report(vm, case)?;
    vm.expect("Boot complete.")?;
    // The guest's key, which the server then lets in.
    let key = program_line(vm, "sshtest: key ")?;
    if !key.starts_with("ssh-ed25519 ") {
        return Err(format!("the guest showed \"{key}\" as its key"));
    }
    write(&dir.join("authorized_keys"), &format!("{key}\n"), false)?;
    let pushed = program_line(vm, "sshtest: pushed ")?;
    vm.expect("sshtest: every check passed")?;
    // The host's repository has the commit the guest pushed.
    let work = dir.join("work");
    let head = run(&work, "git", &["--git-dir", "repo.git", "rev-parse", "main"])?;
    if head != pushed {
        return Err(format!("the host's repository is at {head}, not the pushed {pushed}"));
    }
    let subject = run(&work, "git", &["--git-dir", "repo.git", "log", "-1", "--format=%s"])?;
    if subject != "From Dukovo" {
        return Err(format!("the host's last commit is \"{subject}\""));
    }
    let sent = std::fs::read_to_string(work.join("sent")).unwrap_or_default();
    if sent != "from dukovo\n" {
        return Err(format!("scp put \"{sent}\" on the host"));
    }
    let code = program_line(vm, "(/system/tests/posix/sshtest) exited with code ")?;
    if code != "0x0" {
        return Err(format!("sshtest exited with code {code}"));
    }
    Ok(())
}
