// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test of Rust's standard library on Radix (the `std-test` test of
//! the posix boot test, radix-posix(7)): the environment, files,
//! threads and their synchronisation, time, processes, pipes and socket
//! pairs, panics and backtraces, and memory, through `std` as programs use it. Like
//! the C tests, it prints "std-test: <part>" before each part, a line for
//! each check that fails, and "std-test: every check passed"; it exits
//! with how many checks failed.
//!
//! It runs itself as the child its process tests need, with `child` and
//! what to do as its arguments.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::hint;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, LazyLock, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The test's name, which every line it prints starts with.
const NAME: &str = "std-test";

/// Where the posix boot test keeps the test programs.
const EXE: &str = "/system/tests/posix/std-test";

/// The directory the file tests work in.
const DIR: &str = "/tmp/std-test";

/// How many checks have failed.
static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Says that a check failed, and counts it.
fn failed(file: &str, line: u32, what: &str) {
    println!("{NAME}: FAILED: {file}:{line}: {what}");
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Checks that a condition holds.
macro_rules! check {
    ($condition:expr) => {
        if !$condition {
            failed(file!(), line!(), stringify!($condition));
        }
    };
}

/// What a part of the test returns: an error it could not go on after.
type Outcome = Result<(), Box<dyn std::error::Error>>;

/// Runs part `name` of the test, saying so first, so that a test that
/// hangs shows where.
fn run(name: &str, part: fn() -> Outcome) {
    println!("{NAME}: {name}");
    if let Err(error) = part() {
        println!("{NAME}: FAILED: {name} stopped: {error}");
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        child(&args[2..]);
    }
    run("environment", environment);
    run("files", files);
    run("threads", threads);
    run("synchronisation", synchronisation);
    run("time", time);
    run("processes", processes);
    run("pipes", pipes);
    run("sockets", sockets);
    run("panics", panics);
    run("backtraces", backtraces);
    run("memory", memory);
    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("{NAME}: every check passed");
    } else {
        println!("{NAME}: {failures} checks failed");
    }
    process::exit(failures.min(125) as i32);
}

/// What the test's children do.
fn child(args: &[String]) -> ! {
    match args.first().map(String::as_str) {
        Some("exit") => process::exit(args[1].parse().unwrap_or(99)),
        // Upper-cases its input onto its output; says so on its errors.
        Some("echo") => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input).unwrap_or_default();
            print!("{}", input.to_uppercase());
            eprint!("echoed {} bytes", input.len());
            process::exit(0)
        }
        // Prints a variable of its environment and its working directory.
        Some("env") => {
            let value = std::env::var("DUKOVO_TEST").unwrap_or_default();
            let others = std::env::vars().filter(|(name, _)| name != "DUKOVO_TEST").count();
            let dir = std::env::current_dir().unwrap_or_default();
            print!("{value} {others} {}", dir.display());
            process::exit(0)
        }
        // Prints whether it leads its process group.
        Some("group") => {
            // SAFETY: getpgrp has no preconditions.
            let group = unsafe { libc::getpgrp() };
            print!("{}", group == process::id() as i32);
            process::exit(0)
        }
        Some("sleep") => {
            thread::sleep(Duration::from_secs(60));
            process::exit(0)
        }
        Some("panic") => panic!("the child panics"),
        Some("abort") => process::abort(),
        _ => process::exit(98),
    }
}

/// Arguments, variables, the working directory, the program's path.
fn environment() -> Outcome {
    let args: Vec<String> = std::env::args().collect();
    check!(args.len() == 1 && args[0] == EXE);
    check!(std::env::current_exe()? == Path::new(EXE));
    check!(std::env::var("PATH").is_ok());
    // SAFETY: no other thread runs yet, so none reads the environment.
    unsafe { std::env::set_var("DUKOVO_TEST", "set") };
    check!(std::env::var("DUKOVO_TEST")? == "set");
    check!(std::env::vars().any(|(name, value)| name == "DUKOVO_TEST" && value == "set"));
    // SAFETY: as above.
    unsafe { std::env::remove_var("DUKOVO_TEST") };
    check!(std::env::var("DUKOVO_TEST") == Err(std::env::VarError::NotPresent));
    check!(std::env::current_dir()? == Path::new("/"));
    std::env::set_current_dir("/tmp")?;
    check!(std::env::current_dir()? == Path::new("/tmp"));
    std::env::set_current_dir("/")?;
    check!(std::env::temp_dir() == Path::new("/tmp"));
    check!(process::id() > 1 && std::os::unix::process::parent_id() > 0);
    // The posix boot test runs every test on a pseudo-terminal.
    check!(io::stdout().is_terminal() && io::stdin().is_terminal());
    Ok(())
}

/// Reading and writing files, their metadata, directories and errors.
fn files() -> Outcome {
    let dir = Path::new(DIR);
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir.join("a/b"))?;
    check!(dir.join("a/b").is_dir());
    // Whole files, written and read back.
    let file = dir.join("a/file");
    fs::write(&file, "hello, world\n")?;
    check!(fs::read_to_string(&file)? == "hello, world\n");
    check!(fs::metadata(&file)?.len() == 13 && file.is_file());
    // Seeking, and writing where it points.
    let mut handle = OpenOptions::new().read(true).write(true).open(&file)?;
    handle.seek(SeekFrom::Start(7))?;
    handle.write_all(b"Radix")?;
    handle.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    handle.read_to_string(&mut text)?;
    check!(text == "hello, Radix\n");
    check!(handle.stream_position()? == 13);
    handle.set_len(5)?;
    handle.sync_all()?;
    handle.sync_data()?;
    check!(fs::read(&file)? == b"hello");
    // Appending.
    let mut appending = OpenOptions::new().append(true).open(&file)?;
    appending.write_all(b" again")?;
    check!(fs::read_to_string(&file)? == "hello again");
    // A megabyte, a byte at a time different.
    let big: Vec<u8> = (0..1 << 20).map(|i: u32| (i % 251) as u8).collect();
    fs::write(dir.join("big"), &big)?;
    check!(fs::read(dir.join("big"))? == big);
    let mut part = [0u8; 4096];
    let mut reader = File::open(dir.join("big"))?;
    reader.seek(SeekFrom::Start(500_000))?;
    reader.read_exact(&mut part)?;
    check!(part.iter().enumerate().all(|(i, &b)| b == ((500_000 + i) % 251) as u8));
    // Lines through a buffer.
    fs::write(dir.join("lines"), "one\ntwo\nthree\n")?;
    let lines: Vec<String> =
        BufReader::new(File::open(dir.join("lines"))?).lines().collect::<Result<_, _>>()?;
    check!(lines == ["one", "two", "three"]);
    // Metadata: a number and times. Files have no permissions until users
    // come (Phase 13), so changing them succeeds and changes nothing.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    let metadata = fs::metadata(&file)?;
    check!(metadata.is_file() && !metadata.permissions().readonly());
    check!(metadata.ino() != 0 && metadata.nlink() >= 1 && metadata.uid() == 0);
    let modified = metadata.modified()?;
    check!(modified > UNIX_EPOCH);
    check!(SystemTime::now().duration_since(modified).is_ok_and(|d| d < Duration::from_secs(60)));
    // Locks: two opens of a file are two owners.
    let (first, second) = (File::open(&file)?, File::open(&file)?);
    first.lock()?;
    check!(matches!(second.try_lock_shared(), Err(fs::TryLockError::WouldBlock)));
    first.unlock()?;
    second.lock_shared()?;
    first.lock_shared()?;
    check!(matches!(first.try_lock(), Err(fs::TryLockError::WouldBlock)));
    drop(second);
    check!(first.try_lock().is_ok());
    // Copying and renaming.
    check!(fs::copy(&file, dir.join("copy"))? == 11);
    check!(fs::read_to_string(dir.join("copy"))? == "hello again");
    fs::rename(dir.join("copy"), dir.join("a/b/moved"))?;
    check!(!dir.join("copy").exists() && dir.join("a/b/moved").is_file());
    // A directory's entries, and their kinds.
    let mut names: Vec<(String, bool)> = fs::read_dir(dir.join("a"))?
        .map(|entry| {
            let entry = entry?;
            Ok((entry.file_name().into_string().unwrap_or_default(), entry.file_type()?.is_dir()))
        })
        .collect::<io::Result<_>>()?;
    names.sort();
    check!(names == [("b".to_string(), true), ("file".to_string(), false)]);
    check!(fs::canonicalize(dir.join("a/../a/b/./moved"))? == dir.join("a/b/moved"));
    // Errors, by kind.
    let kind = |result: io::Result<File>| result.err().map(|e| e.kind());
    check!(kind(File::open(dir.join("missing"))) == Some(io::ErrorKind::NotFound));
    check!(kind(File::create_new(&file)) == Some(io::ErrorKind::AlreadyExists));
    check!(kind(File::open(file.join("x"))) == Some(io::ErrorKind::NotADirectory));
    check!(fs::read(dir.join("a")).err().map(|e| e.kind()) == Some(io::ErrorKind::IsADirectory));
    check!(
        fs::remove_dir(dir.join("a")).err().map(|e| e.kind())
            == Some(io::ErrorKind::DirectoryNotEmpty)
    );
    // Removing.
    fs::remove_file(dir.join("a/b/moved"))?;
    fs::remove_dir(dir.join("a/b"))?;
    fs::remove_dir_all(dir)?;
    check!(!dir.exists());
    Ok(())
}

thread_local! {
    /// A value each thread has its own of, which counts the threads that
    /// ended with it.
    static MINE: Ending = const { Ending(Cell::new(0)) };
}

/// Threads that ended with their [`MINE`] dropped.
static ENDED: AtomicUsize = AtomicUsize::new(0);

/// A thread's own value, counted in [`ENDED`] when its thread ends.
struct Ending(Cell<u32>);

impl Drop for Ending {
    fn drop(&mut self) {
        if self.0.get() != 0 {
            ENDED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Uses `depth` MiB of stack; returns `depth`.
#[inline(never)]
fn deep(depth: usize) -> usize {
    let mut block = [0u8; 1 << 20];
    block[0] = 1;
    block[(1 << 20) - 1] = 1;
    std::hint::black_box(&mut block);
    let below = if depth > 1 { deep(depth - 1) } else { 0 };
    below + usize::from(block[0]) + usize::from(block[(1 << 20) - 1]) - 1
}

/// Threads: their results, names, stacks and thread-local values.
fn threads() -> Outcome {
    const THREADS: u64 = 8;
    let counter = Arc::new(Mutex::new(0u64));
    let atomic = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|n| {
            let (counter, atomic) = (counter.clone(), atomic.clone());
            thread::spawn(move || {
                for _ in 0..1000 {
                    *counter.lock().unwrap() += 1;
                    atomic.fetch_add(1, Ordering::Relaxed);
                }
                MINE.with(|mine| mine.0.set(1));
                n * 2
            })
        })
        .collect();
    let mut sum = 0;
    for handle in handles {
        sum += handle.join().map_err(|_| "a thread panicked")?;
    }
    check!(sum == THREADS * (THREADS - 1));
    check!(*counter.lock().unwrap() == THREADS * 1000 && atomic.load(Ordering::Relaxed) == 8000);
    // Every thread's own value was dropped when it ended.
    check!(ENDED.load(Ordering::Relaxed) == THREADS as usize);
    // Names, which the kernel keeps too.
    let named = thread::Builder::new().name("worker".into()).spawn(|| {
        let mut name = [0 as libc::c_char; 16];
        // SAFETY: the buffer is as long as it says.
        let e = unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr(), 16) };
        // SAFETY: pthread_getname_np ends the name with a NUL.
        let kernel = e == 0 && unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes() == b"worker";
        thread::current().name() == Some("worker") && kernel
    })?;
    check!(named.join().unwrap_or(false));
    check!(thread::current().name() == Some("main"));
    // A thread with a larger stack than the default 2 MiB.
    let big = thread::Builder::new().stack_size(16 << 20).spawn(|| deep(8))?;
    check!(big.join().unwrap_or(0) == 8);
    // Scoped threads borrow what the caller has.
    let words = ["one", "two", "three"];
    let lengths: Vec<usize> = thread::scope(|scope| {
        let handles: Vec<_> = words.iter().map(|w| scope.spawn(move || w.len())).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    check!(lengths == [3, 3, 5]);
    check!(thread::available_parallelism()?.get() >= 1);
    check!(thread::spawn(|| thread::current().id()).join().ok() != Some(thread::current().id()));
    // A thread that panics: join says so, with what it said.
    let result = thread::Builder::new()
        .name("panicker".into())
        .spawn(|| -> u32 {
            let _quiet = Quiet::new();
            panic!("thread boom")
        })?
        .join();
    let message = result.err().and_then(|p| p.downcast::<&str>().ok().map(|m| *m));
    check!(message == Some("thread boom"));
    Ok(())
}

/// Mutexes, condition variables, read-write locks, barriers, channels,
/// one-time values, parking and sleeping.
fn synchronisation() -> Outcome {
    // A condition variable passes turns between two threads.
    let state = Arc::new((Mutex::new(0u32), Condvar::new()));
    let other = state.clone();
    let pong = thread::spawn(move || {
        let (lock, changed) = &*other;
        for round in 0..100 {
            let mut turn =
                changed.wait_while(lock.lock().unwrap(), |t| *t != round * 2 + 1).unwrap();
            *turn += 1;
            changed.notify_all();
        }
    });
    {
        let (lock, changed) = &*state;
        for round in 0..100 {
            let mut turn = changed.wait_while(lock.lock().unwrap(), |t| *t != round * 2).unwrap();
            *turn += 1;
            changed.notify_all();
        }
        let done = changed.wait_while(lock.lock().unwrap(), |t| *t != 200).unwrap();
        check!(*done == 200);
    }
    pong.join().map_err(|_| "the pong thread panicked")?;
    // A wait that times out.
    let (lock, changed) = &*state;
    let (guard, timeout) =
        changed.wait_timeout(lock.lock().unwrap(), Duration::from_millis(20)).unwrap();
    drop(guard);
    check!(timeout.timed_out());
    // Readers share a read-write lock; a writer has it alone.
    let table = Arc::new(RwLock::new(vec![1, 2, 3]));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let table = table.clone();
            thread::spawn(move || table.read().unwrap().iter().sum::<i32>())
        })
        .collect();
    for reader in readers {
        check!(reader.join().unwrap_or(0) == 6);
    }
    table.write().unwrap().push(4);
    check!(table.read().unwrap().len() == 4);
    // A barrier lets threads through only together.
    let barrier = Arc::new(Barrier::new(4));
    let arrived = Arc::new(AtomicUsize::new(0));
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let (barrier, arrived) = (barrier.clone(), arrived.clone());
            thread::spawn(move || {
                arrived.fetch_add(1, Ordering::SeqCst);
                barrier.wait();
                arrived.load(Ordering::SeqCst)
            })
        })
        .collect();
    for waiter in waiters {
        check!(waiter.join().unwrap_or(0) == 4);
    }
    // Channels: many senders to one receiver, and a rendezvous.
    let (send, receive) = mpsc::channel();
    for n in 0..4 {
        let send = send.clone();
        thread::spawn(move || {
            for i in 0..100 {
                let _ = send.send(n * 100 + i);
            }
        });
    }
    drop(send);
    let mut got: Vec<u32> = receive.iter().collect();
    got.sort();
    check!(got == (0..400).collect::<Vec<_>>());
    let (send, receive) = mpsc::sync_channel(0);
    let sender = thread::spawn(move || send.send("met").is_ok());
    check!(receive.recv() == Ok("met") && sender.join().unwrap_or(false));
    check!(receive.recv_timeout(Duration::from_millis(10)).is_err());
    // Values made once, whoever asks first.
    static ONCE: OnceLock<u64> = OnceLock::new();
    static LAZY: LazyLock<String> = LazyLock::new(|| "made".repeat(2));
    let makers: Vec<_> =
        (0..4).map(|n| thread::spawn(move || *ONCE.get_or_init(|| n + 100))).collect();
    let made: Vec<u64> = makers.into_iter().map(|m| m.join().unwrap_or(0)).collect();
    check!(made.iter().all(|&m| m == made[0]) && made[0] >= 100);
    check!(*LAZY == "mademade");
    // Parking: unpark wakes a parked thread, and park_timeout ends.
    let woken = Arc::new(AtomicBool::new(false));
    let flag = woken.clone();
    let parked = thread::spawn(move || {
        while !flag.load(Ordering::Acquire) {
            thread::park();
        }
    });
    thread::sleep(Duration::from_millis(10));
    woken.store(true, Ordering::Release);
    parked.thread().unpark();
    parked.join().map_err(|_| "the parked thread panicked")?;
    let start = Instant::now();
    thread::park_timeout(Duration::from_millis(20));
    check!(start.elapsed() < Duration::from_secs(5));
    // A poisoned mutex says so.
    let poisoned = Arc::new(Mutex::new(0));
    let holder = poisoned.clone();
    let _ = thread::spawn(move || {
        let _quiet = Quiet::new();
        let _guard = holder.lock().unwrap();
        panic!("poisoning");
    })
    .join();
    check!(poisoned.is_poisoned() && poisoned.lock().is_err());
    Ok(())
}

/// The monotonic clock, the wall clock and sleeping.
fn time() -> Outcome {
    let start = Instant::now();
    let mut last = start;
    for _ in 0..1000 {
        let now = Instant::now();
        check!(now >= last);
        last = now;
    }
    thread::sleep(Duration::from_millis(30));
    let slept = start.elapsed();
    check!(slept >= Duration::from_millis(30) && slept < Duration::from_secs(10));
    // The wall clock is past 2020 (the RTC's date).
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    check!(now.as_secs() > 1_577_836_800);
    let later = SystemTime::now() + Duration::from_secs(10);
    check!(later.duration_since(SystemTime::now())? <= Duration::from_secs(10));
    Ok(())
}

/// The test program, as a child doing `what`.
fn child_command(what: &[&str]) -> Command {
    let mut command = Command::new(EXE);
    command.arg("child").args(what);
    command
}

/// Starting programs, their exit statuses, their input and output, their
/// environment, signals to them, and errors.
fn processes() -> Outcome {
    check!(child_command(&["exit", "7"]).status()?.code() == Some(7));
    check!(child_command(&["exit", "0"]).status()?.success());
    // Input through a pipe, both outputs captured.
    let mut echo = child_command(&["echo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    echo.stdin.take().ok_or("no stdin")?.write_all(b"quiet words")?;
    let output = echo.wait_with_output()?;
    check!(output.status.success());
    check!(output.stdout == b"QUIET WORDS" && output.stderr == b"echoed 11 bytes");
    // Its own environment and working directory.
    let output = child_command(&["env"])
        .env_clear()
        .env("DUKOVO_TEST", "yes")
        .current_dir("/tmp")
        .output()?;
    check!(String::from_utf8_lossy(&output.stdout) == "yes 0 /tmp");
    let output = child_command(&["group"]).process_group(0).output()?;
    check!(output.stdout == b"true");
    // Signals: one it is killed with, and its own abort.
    let mut sleeper = child_command(&["sleep"]).spawn()?;
    check!(matches!(sleeper.try_wait(), Ok(None)));
    sleeper.kill()?;
    let status = sleeper.wait()?;
    check!(status.signal() == Some(libc::SIGKILL) && status.code().is_none());
    let status = child_command(&["abort"]).stderr(Stdio::null()).status()?;
    check!(status.signal() == Some(libc::SIGABRT));
    // A panic in a child: exit code 101, with the message on its errors.
    let output = child_command(&["panic"]).output()?;
    let errors = String::from_utf8_lossy(&output.stderr);
    check!(output.status.code() == Some(101) && errors.contains("the child panics"));
    // With RUST_BACKTRACE, the message comes with a backtrace that names
    // the functions.
    let output = child_command(&["panic"]).env("RUST_BACKTRACE", "1").output()?;
    let errors = String::from_utf8_lossy(&output.stderr);
    check!(errors.contains("stack backtrace:") && errors.contains("std_test::child"));
    // A program that is not there.
    let missing = Command::new("/system/tests/posix/no-such-program").spawn();
    check!(missing.err().map(|e| e.kind()) == Some(io::ErrorKind::NotFound));
    Ok(())
}

/// Anonymous pipes, between threads and to a child, and a pipe whose
/// reader is gone.
fn pipes() -> Outcome {
    let (mut reader, mut writer) = io::pipe()?;
    let writing = thread::spawn(move || writer.write_all(&[7u8; 100_000]));
    let mut got = Vec::new();
    reader.read_to_end(&mut got)?;
    writing.join().map_err(|_| "the writer panicked")??;
    check!(got.len() == 100_000 && got.iter().all(|&b| b == 7));
    // A child writes into one.
    let (mut reader, writer) = io::pipe()?;
    let mut child = child_command(&["echo"])
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null())
        .spawn()?;
    child.wait()?;
    let mut text = String::new();
    reader.read_to_string(&mut text)?;
    check!(text.is_empty());
    // Rust programs ignore SIGPIPE: a write with no reader fails.
    let (reader, mut writer) = io::pipe()?;
    drop(reader);
    check!(writer.write_all(b"x").err().map(|e| e.kind()) == Some(io::ErrorKind::BrokenPipe));
    Ok(())
}

/// Socket pairs, as `UnixStream::pair` makes them (mio and crossterm's
/// way to wake a thread from a signal handler): both ways, the end of the
/// stream after a shutdown, non-blocking ends, and what they are.
fn sockets() -> Outcome {
    let (mut one, mut other) = UnixStream::pair()?;
    one.write_all(b"ping")?;
    let mut got = [0u8; 4];
    other.read_exact(&mut got)?;
    check!(&got == b"ping");
    other.write_all(b"pong")?;
    one.read_exact(&mut got)?;
    check!(&got == b"pong");
    let copy = File::from(std::os::fd::OwnedFd::from(one.try_clone()?));
    check!(copy.metadata()?.file_type().is_socket());
    drop(copy);
    check!(one.peer_addr()?.is_unnamed() && one.local_addr()?.is_unnamed());
    check!(one.take_error()?.is_none());
    // A thread streams into it while this one reads.
    let mut writer = one.try_clone()?;
    let writing = thread::spawn(move || {
        writer.write_all(&[9u8; 200_000])?;
        writer.shutdown(Shutdown::Write)
    });
    let mut all = Vec::new();
    other.read_to_end(&mut all)?;
    writing.join().map_err(|_| "the writer panicked")??;
    check!(all.len() == 200_000 && all.iter().all(|&b| b == 9));
    // Shut for writing, it cannot send any more; the other way still works.
    check!(one.write_all(b"x").err().map(|e| e.kind()) == Some(io::ErrorKind::BrokenPipe));
    other.write_all(b"back")?;
    one.read_exact(&mut got)?;
    check!(&got == b"back");
    // Non-blocking, an empty end would block.
    // (The other way has ended, so a read there gives 0, not WouldBlock.)
    one.set_nonblocking(true)?;
    check!(one.read(&mut got).err().map(|e| e.kind()) == Some(io::ErrorKind::WouldBlock));
    drop(other);
    check!(one.read(&mut got)? == 0);
    Ok(())
}

/// Silences the panic message of what it guards: for panics the test
/// makes on purpose.
struct Quiet;

/// The panic hook as it was, while a [`Quiet`] is held.
static QUIET: AtomicUsize = AtomicUsize::new(0);

impl Quiet {
    fn new() -> Quiet {
        if QUIET.fetch_add(1, Ordering::SeqCst) == 0 {
            let default = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                if QUIET.load(Ordering::SeqCst) == 0 {
                    default(info);
                }
            }));
        }
        Quiet
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        QUIET.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Unwinding: catching panics, drops on the way, the hook, resuming.
fn panics() -> Outcome {
    let _quiet = Quiet::new();
    // A constant message is a `&str`; one formatted at run time a `String`.
    let caught = panic::catch_unwind(|| -> u32 { panic!("boom") });
    check!(caught.err().and_then(|p| p.downcast::<&str>().ok()).is_some_and(|m| *m == "boom"));
    let number = hint::black_box(1);
    let caught = panic::catch_unwind(|| -> u32 { panic!("boom {number}") });
    let message = caught.err().and_then(|p| p.downcast::<String>().ok()).map(|m| *m);
    check!(message.as_deref() == Some("boom 1"));
    // Values are dropped as a panic passes them.
    let dropped = Arc::new(AtomicBool::new(false));
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let guard = Guard(dropped.clone());
    let _ = panic::catch_unwind(AssertUnwindSafe(move || {
        let _guard = guard;
        panic!("dropping");
    }));
    check!(dropped.load(Ordering::SeqCst));
    // A payload of any type, resumed, and a catch inside a catch.
    let outer = panic::catch_unwind(|| {
        let inner = panic::catch_unwind(|| panic::resume_unwind(Box::new(42u64)));
        check!(inner.err().and_then(|p| p.downcast::<u64>().ok()).map(|v| *v) == Some(42));
        panic::resume_unwind(Box::new("again"))
    });
    check!(outer.err().and_then(|p| p.downcast::<&str>().ok()).map(|m| *m) == Some("again"));
    // The hook sees where a panic was.
    static LINE: AtomicU64 = AtomicU64::new(0);
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        LINE.store(info.location().map_or(0, |l| l.line().into()), Ordering::SeqCst);
    }));
    let line = line!() + 1;
    let _ = panic::catch_unwind(|| panic!("located"));
    panic::set_hook(previous);
    check!(LINE.load(Ordering::SeqCst) == u64::from(line));
    Ok(())
}

/// A backtrace of this thread, with function names from the program's
/// symbols.
#[inline(never)]
fn backtraces() -> Outcome {
    let backtrace = Backtrace::force_capture();
    check!(backtrace.status() == BacktraceStatus::Captured);
    let text = backtrace.to_string();
    check!(text.contains("std_test::backtraces"));
    check!(text.contains("std_test::main"));
    Ok(())
}

/// Large allocations, aligned ones, and a map of many entries.
fn memory() -> Outcome {
    // 64 MiB, every page touched.
    let mut big = vec![0u8; 64 << 20];
    for page in big.chunks_mut(4096) {
        page[0] = 1;
    }
    check!(big.iter().map(|&b| usize::from(b)).sum::<usize>() == (64 << 20) / 4096);
    drop(big);
    for align in [16, 4096, 2 << 20] {
        let layout = std::alloc::Layout::from_size_align(align * 2, align)?;
        // SAFETY: the layout's size is not zero.
        let pointer = unsafe { std::alloc::alloc(layout) };
        check!(!pointer.is_null() && pointer.addr() % align == 0);
        if !pointer.is_null() {
            // SAFETY: allocated just above, with this layout.
            unsafe { std::alloc::dealloc(pointer, layout) };
        }
    }
    // A hash map, with the random keys its hasher needs.
    let map: HashMap<u32, u32> = (0..100_000).map(|i| (i, i * 2)).collect();
    check!(map.len() == 100_000 && map[&77_777] == 155_554);
    let path: PathBuf = ["/system", "tests", "posix"].iter().collect();
    check!(path == Path::new("/system/tests/posix"));
    Ok(())
}
