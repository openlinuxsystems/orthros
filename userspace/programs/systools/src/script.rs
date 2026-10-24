// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! script(1): runs a shell, or a command (`-c`), on a new pseudo-terminal
//! and saves all it writes to a file (`typescript` by default), as
//! util-linux's script does, while passing it on to the terminal.

use std::ffi::{CString, OsString};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail};

/// Set when the terminal's size changes.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

/// script(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut append = false;
    let mut quiet = false;
    let mut command: Option<String> = None;
    let mut return_code = false;
    let mut flush = false;
    let mut file: Option<OsString> = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('a') => append = true,
            Arg::Long(l) if l == "append" => append = true,
            Arg::Short('q') => quiet = true,
            Arg::Long(l) if l == "quiet" => quiet = true,
            Arg::Short('c') => command = Some(args.text()?),
            Arg::Long(l) if l == "command" => command = Some(args.text()?),
            Arg::Short('e') => return_code = true,
            Arg::Long(l) if l == "return" => return_code = true,
            Arg::Short('f') => flush = true,
            Arg::Long(l) if l == "flush" => flush = true,
            Arg::Short('E') => {
                let _ = args.text()?;
            }
            Arg::Long(l) if l == "echo" => {
                let _ = args.text()?;
            }
            Arg::Operand(name) if file.is_none() => file = Some(name),
            other => return Err(args.unknown(&other)),
        }
    }
    let path = file.unwrap_or_else(|| "typescript".into());
    let mut log = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&path)
        .map_err(|e| util::failed(&std::path::Path::new(&path).display(), e))?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let now = (util::now_ns() / 1_000_000_000) as i64;
    let tty = util::is_terminal(0);
    let (columns, lines) = util::terminal_size(0).unwrap_or((80, 24));
    let term = std::env::var("TERM").unwrap_or_else(|_| "dumb".into());
    let mut header =
        format!("Script started on {} [", util::local_time(now, "%Y-%m-%d %H:%M:%S%z"));
    if let Some(command) = &command {
        header.push_str(&format!("COMMAND=\"{command}\" "));
    }
    header.push_str(&format!("TERM=\"{term}\" COLUMNS=\"{columns}\" LINES=\"{lines}\"]\n"));
    log.write_all(header.as_bytes())?;
    if !quiet {
        println!("Script started, output log file is '{}'.", std::path::Path::new(&path).display());
    }

    // The child's terminal: the size of ours.
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: all zeros is a valid winsize: numbers.
    let mut size: libc::winsize = unsafe { util::zeroed() };
    // SAFETY: TIOCGWINSZ fills `size`; failing leaves it zero.
    unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) };
    // SAFETY: openpty writes the two descriptors; the name may be null.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            if tty { &size } else { std::ptr::null() },
        )
    };
    if opened != 0 {
        return Err(fail(format!(
            "openpty failed: {}",
            util::strip_os_error(&io::Error::last_os_error())
        )));
    }
    let argv_strings: Vec<CString> = match &command {
        Some(command) => {
            vec![c_string(&shell), CString::new("-c").unwrap_or_default(), c_string(command)]
        }
        None => vec![c_string(&shell), CString::new("-i").unwrap_or_default()],
    };
    let mut argv: Vec<*const libc::c_char> = argv_strings.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: the program is single-threaded; the child only makes the
    // terminal its own and execs.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        // SAFETY: async-signal-safe calls on our own descriptors and the
        // argv built before the fork.
        unsafe {
            libc::close(master);
            libc::login_tty(slave);
            libc::execv(argv[0], argv.as_ptr());
            libc::_exit(127);
        }
    }
    // SAFETY: closing our copy of the child's end.
    unsafe { libc::close(slave) };
    // A closed output is an error to stop at with the terminal put back
    // (util-linux's script dies of the SIGPIPE and leaves it raw). Only
    // here, after the fork, so the child keeps the default.
    // SAFETY: ignoring a signal.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let log_name = std::path::Path::new(&path).display();
    let mut saved = Restore(None);
    if tty {
        // SAFETY: all zeros is a valid termios: numbers.
        let mut t: libc::termios = unsafe { util::zeroed() };
        // SAFETY: tcgetattr fills `t`.
        if unsafe { libc::tcgetattr(0, &mut t) } == 0 {
            saved.0 = Some(t);
            let mut raw = t;
            // SAFETY: cfmakeraw changes the termios it is given.
            unsafe { libc::cfmakeraw(&mut raw) };
            // SAFETY: `raw` is a valid termios.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        }
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a function address.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = on_winch as *const () as usize;
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) };
    }
    let mut stdin_open = true;
    let mut buffer = [0u8; 4096];
    let mut stdout = io::stdout().lock();
    loop {
        if RESIZED.swap(false, Ordering::Relaxed) {
            // SAFETY: copies the terminal's size to the child's.
            unsafe {
                libc::ioctl(0, libc::TIOCGWINSZ, &mut size);
                libc::ioctl(master, libc::TIOCSWINSZ, &size);
            }
        }
        let mut fds = [
            libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if stdin_open { 0 } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            // SAFETY: reads into our buffer.
            let n = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n <= 0 {
                stdin_open = false;
                // The end of what was piped in is the end of the
                // terminal's input: an end-of-file character (^D).
                if !tty {
                    write_all_fd(master, &[4]);
                }
            } else {
                write_all_fd(master, &buffer[..n as usize]);
            }
        }
        if fds[0].revents != 0 {
            // SAFETY: reads into our buffer.
            let n = unsafe { libc::read(master, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n <= 0 {
                // The child's side has closed: it has ended.
                break;
            }
            let data = &buffer[..n as usize];
            stdout
                .write_all(data)
                .and_then(|()| stdout.flush())
                .map_err(|e| util::failed(&"write error", e))?;
            log.write_all(data)
                .and_then(|()| if flush { log.flush() } else { Ok(()) })
                .map_err(|e| util::failed(&log_name, e))?;
        }
    }
    drop(saved);
    let mut status = 0;
    // SAFETY: waits for our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    // SAFETY: closing our descriptor.
    unsafe { libc::close(master) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    let now = (util::now_ns() / 1_000_000_000) as i64;
    log.write_all(
        format!(
            "\nScript done on {} [COMMAND_EXIT_CODE=\"{code}\"]\n",
            util::local_time(now, "%Y-%m-%d %H:%M:%S%z")
        )
        .as_bytes(),
    )?;
    if !quiet {
        println!("Script done.");
    }
    Ok(if return_code { code } else { 0 })
}

/// The terminal's settings from before it was made raw, put back when it
/// is dropped: also when a write to the log or the output fails.
struct Restore(Option<libc::termios>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: `t` is the termios read at the start.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}

fn c_string(text: &str) -> CString {
    CString::new(text.as_bytes()).unwrap_or_default()
}

/// Writes all of `data` to `fd`, as far as it takes it.
fn write_all_fd(fd: libc::c_int, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: writes from our buffer.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n <= 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        data = &data[n as usize..];
    }
}
