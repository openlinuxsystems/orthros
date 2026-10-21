// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `linux-bench`: the cost on Linux of the operations that `ob bench`
//! measures on Radix, as a baseline (docs/design/ipc.md).
//!
//! ```text
//! linux-bench [--rounds <count>]
//! ```
//!
//! It prints one line per benchmark, in the format of the Radix benchmark
//! programs:
//!
//! ```text
//! BENCH name=<name> ns=<nanoseconds per operation> iterations=<count>
//! ```
//!
//! The system calls are timed in a loop, as lmbench's lat_syscall does.
//! Each round trip is a pair of threads passing an 8-byte message there
//! and back, as in lmbench's lat_pipe and lat_unix, once with both threads
//! on one CPU and once on two. A context switch is two processes on one
//! CPU yielding to each other, as in lmbench's lat_ctx; a fresh page is an
//! anonymous mapping, its first touch and its removal; a TLB shootdown is
//! a page losing its access while another thread of the process runs on
//! another CPU; memory bandwidth is `rep movsb` between two buffers larger
//! than the caches, the same instructions Radix runs.
//!
//! `ob bench` runs it on the host, and also as the init process of a Linux
//! virtual machine set up like the one Radix runs in: as process 1 it
//! prints `linux-bench: done` at the end and powers the machine off.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

fn main() {
    let mut rounds = 100_000u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next().and_then(|value| value.parse().ok())) {
            ("--rounds", Some(count)) if count > 0 => rounds = count,
            _ => {
                eprintln!("usage: linux-bench [--rounds <count>]");
                std::process::exit(2);
            }
        }
    }
    let cpus = allowed_cpus();
    let Some(&first) = cpus.first() else {
        eprintln!("linux-bench: this process may run on no CPU");
        std::process::exit(1);
    };
    pin(first);

    let calls = rounds * 2;
    report("getppid", calls, getppid);
    report("sched_yield", calls, || {
        // SAFETY: sched_yield takes no arguments.
        unsafe { libc::sched_yield() };
    });
    report("fcntl_getfd", calls, fcntl_getfd);
    report("getrandom_64", calls / 4, getrandom_64);
    report("fresh_page", rounds / 2, fresh_page);
    memory_copy();

    for (place, other) in [("same_cpu", Some(first)), ("cross_cpu", cpus.get(1).copied())] {
        let Some(other) = other else {
            eprintln!("linux-bench: one CPU only, so no round trips across two");
            continue;
        };
        let pair = Pair { other, rounds };
        pair.run(&format!("pipe_roundtrip_{place}"), pipes());
        pair.run(&format!("unix_socket_roundtrip_{place}"), sockets());
        pair.run(&format!("eventfd_roundtrip_{place}"), eventfds());
        pair.futex(&format!("futex_pingpong_{place}"));
    }
    context_switch(rounds);
    if let Some(&other) = cpus.get(1) {
        tlb_shootdown(other, rounds / 2);
        for (name, count, body) in [
            ("getppid_parallel", calls, getppid as fn()),
            ("fcntl_getfd_parallel", calls, fcntl_getfd),
            ("getrandom_64_parallel", calls / 4, getrandom_64),
        ] {
            parallel(name, other, count, body);
        }
    }

    if std::process::id() == 1 {
        // The init process of `ob bench`'s Linux virtual machine.
        println!("linux-bench: done");
        // SAFETY: as process 1, this program may power the machine off;
        // there is nothing to save first.
        unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };
        loop {
            std::thread::park();
        }
    }
}

fn getppid() {
    // SAFETY: getppid takes no arguments and cannot fail. It is called
    // through syscall(2) so that no library caches its answer.
    unsafe { libc::syscall(libc::SYS_getppid) };
}

/// A descriptor to look up: an eventfd, which every Linux has.
fn some_fd() -> i32 {
    static FD: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();
    let fd = FD.get_or_init(|| {
        // SAFETY: eventfd takes no pointers; a negative result is an error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "cannot make an eventfd: {}", std::io::Error::last_os_error());
        // SAFETY: `fd` is a new descriptor that nothing else owns.
        unsafe { OwnedFd::from_raw_fd(fd) }
    });
    std::os::fd::AsRawFd::as_raw_fd(fd)
}

/// Looks a descriptor up and reads its flags: the closest Linux has to a
/// capability's `info` method.
fn fcntl_getfd() {
    // SAFETY: F_GETFD takes no argument; the descriptor is open.
    let flags = unsafe { libc::fcntl(some_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "fcntl: {}", std::io::Error::last_os_error());
}

fn getrandom_64() {
    let mut bytes = [0u8; 64];
    // SAFETY: `bytes` is 64 writable bytes.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    assert_eq!(got, 64, "getrandom returned {got}");
}

const PAGE: usize = 4096;

/// Maps `len` bytes of fresh anonymous memory.
fn map_anonymous(len: usize) -> *mut u8 {
    // SAFETY: an anonymous private mapping at an address the kernel picks
    // touches no existing memory.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(at != libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
    at.cast()
}

/// A fresh page: mapped, touched (a page fault that zeroes it), unmapped.
fn fresh_page() {
    let at = map_anonymous(PAGE);
    // SAFETY: `at` is a page mapped read-write just above.
    unsafe { at.cast::<u64>().write_volatile(1) };
    // SAFETY: the page was mapped above and nothing refers to it any more.
    let result = unsafe { libc::munmap(at.cast(), PAGE) };
    assert_eq!(result, 0, "munmap: {}", std::io::Error::last_os_error());
}

/// The bytes of each of the two buffers memory bandwidth is measured
/// between, and how many times it copies one into the other.
const COPY_BYTES: usize = 32 << 20;
const COPY_ROUNDS: u64 = 30;

/// Copies `len` bytes with `rep movsb`, as the Radix benchmark does.
///
/// # Safety
///
/// Both ranges must be mapped, `to` writable, and they must not overlap.
unsafe fn copy(from: *const u8, to: *mut u8, len: usize) {
    // SAFETY: the caller's contract.
    unsafe {
        std::arch::asm!(
            "rep movsb",
            inout("rsi") from => _,
            inout("rdi") to => _,
            inout("rcx") len => _,
            options(nostack, preserves_flags),
        );
    }
}

/// Memory bandwidth: nanoseconds per MiB copied.
fn memory_copy() {
    let (from, to) = (map_anonymous(COPY_BYTES), map_anonymous(COPY_BYTES));
    // Fill the source, and copy once, so that no page is touched for the
    // first time below.
    // SAFETY: both buffers were just mapped, COPY_BYTES long, apart.
    unsafe {
        from.write_bytes(0x5a, COPY_BYTES);
        copy(from, to, COPY_BYTES);
    }
    let started = Instant::now();
    for _ in 0..COPY_ROUNDS {
        // SAFETY: as above.
        unsafe { copy(from, to, COPY_BYTES) };
    }
    print_line("memory_copy", started, COPY_ROUNDS * (COPY_BYTES >> 20) as u64);
    for at in [from, to] {
        // SAFETY: mapped above, and no longer used.
        unsafe { libc::munmap(at.cast(), COPY_BYTES) };
    }
}

/// Two processes on this thread's CPU (a child inherits its pinning), each
/// yielding to the other, in another address space. A yield switches only
/// when the other process may run first (EEVDF), so both count their
/// context switches with getrusage(2), and the line gives what one switch
/// costs, as `ob bench` does for Radix with the kernel's count.
fn context_switch(rounds: u64) {
    let (ready_read, ready_write) = std::io::pipe().expect("cannot make a pipe");
    let (count_read, count_write) = std::io::pipe().expect("cannot make a pipe");
    // SAFETY: the process has one thread here (every pair has been joined),
    // so the child may run anything; it only yields, writes and exits.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
    if child == 0 {
        let (mut ready_write, mut count_write) = (ready_write, count_write);
        let _ = ready_write.write_all(&[1]);
        let before = switches();
        for _ in 0..rounds {
            // SAFETY: sched_yield takes no arguments.
            unsafe { libc::sched_yield() };
        }
        let _ = count_write.write_all(&(switches() - before).to_le_bytes());
        // SAFETY: the child ends here, without unwinding the parent's state.
        unsafe { libc::_exit(0) };
    }
    drop((ready_write, count_write));
    let mut byte = [0u8];
    (&ready_read).read_exact(&mut byte).expect("the other process did not start");
    let before = switches();
    let started = Instant::now();
    for _ in 0..rounds {
        // SAFETY: sched_yield takes no arguments.
        unsafe { libc::sched_yield() };
    }
    let elapsed = started.elapsed();
    let ours = switches() - before;
    let mut theirs = [0u8; 8];
    (&count_read).read_exact(&mut theirs).expect("the other process did not report");
    let total = (ours + u64::from_le_bytes(theirs)).max(1);
    let ns = elapsed.as_nanos() as f64 / total as f64;
    println!("BENCH name=context_switch ns={ns:.2} iterations={total}");
    let mut status = 0;
    // SAFETY: `child` is this process's child, and `status` is writable.
    unsafe { libc::waitpid(child, &mut status, 0) };
}

/// The context switches of this process so far, voluntary or not.
fn switches() -> u64 {
    // SAFETY: an all-zero rusage is valid, and getrusage writes into it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is writable.
    let done = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(done, 0, "getrusage: {}", std::io::Error::last_os_error());
    (usage.ru_nvcsw + usage.ru_nivcsw) as u64
}

/// A TLB shootdown: this thread takes a page's access away and gives it
/// back while another thread of the process spins on `other`, so every
/// removal must reach that CPU's TLB. Giving access back needs no flush.
fn tlb_shootdown(other: usize, rounds: u64) {
    let page = map_anonymous(PAGE);
    // SAFETY: `page` is mapped read-write; touching it makes it present.
    unsafe { page.write_volatile(1) };
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(2));
    let (go, done) = (ready.clone(), stop.clone());
    let spinner = std::thread::spawn(move || {
        pin(other);
        go.wait();
        while !done.load(Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    });
    ready.wait();
    let protect = |prot| {
        // SAFETY: `page` is a whole page this function mapped.
        let result = unsafe { libc::mprotect(page.cast(), PAGE, prot) };
        assert_eq!(result, 0, "mprotect: {}", std::io::Error::last_os_error());
    };
    let started = Instant::now();
    for _ in 0..rounds {
        protect(libc::PROT_NONE);
        protect(libc::PROT_READ | libc::PROT_WRITE);
    }
    print_line("tlb_shootdown", started, rounds);
    stop.store(true, Ordering::Relaxed);
    spinner.join().expect("the spinning thread panicked");
    // SAFETY: mapped above, and no longer used.
    unsafe { libc::munmap(page.cast(), PAGE) };
}

/// Runs `body` `count` times here and on `other` at once; prints what one
/// run cost here.
fn parallel(name: &str, other: usize, count: u64, body: fn()) {
    let ready = Arc::new(Barrier::new(2));
    let go = ready.clone();
    let partner = std::thread::spawn(move || {
        pin(other);
        go.wait();
        for _ in 0..count {
            body();
        }
    });
    ready.wait();
    let started = Instant::now();
    for _ in 0..count {
        body();
    }
    print_line(name, started, count);
    partner.join().expect("the other thread panicked");
}

/// Runs `body` `count` times and prints what one run cost.
fn report(name: &str, count: u64, mut body: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..count {
        body();
    }
    print_line(name, started, count);
}

fn print_line(name: &str, started: Instant, count: u64) {
    let ns = started.elapsed().as_nanos() as f64 / count as f64;
    println!("BENCH name={name} ns={ns:.2} iterations={count}");
}

/// The calling thread, pinned already, and one pinned to `other`.
struct Pair {
    other: usize,
    rounds: u64,
}

/// The two ends of a round trip: the asking thread writes to `ask.1` and
/// reads its answer from `ask.0`; the answering one reads from `answer.0`
/// and writes back to `answer.1`.
type Ends<R, W> = ((R, W), (R, W));

impl Pair {
    /// Times `rounds` 8-byte round trips between the two threads.
    fn run<R, W>(&self, name: &str, ends: Ends<R, W>)
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let ((mut ask_read, mut ask_write), (mut answer_read, mut answer_write)) = ends;
        let ready = Arc::new(Barrier::new(2));
        let (other, rounds, go) = (self.other, self.rounds, ready.clone());
        let answerer = std::thread::spawn(move || {
            pin(other);
            go.wait();
            let mut word = [0u8; 8];
            for _ in 0..rounds {
                answer_read.read_exact(&mut word).expect("reading the question");
                answer_write.write_all(&word).expect("writing the answer");
            }
        });
        ready.wait();
        let mut word = [0u8; 8];
        let started = Instant::now();
        // Never 0, which an eventfd would not pass on.
        for message in 1..=self.rounds {
            ask_write.write_all(&message.to_le_bytes()).expect("writing the question");
            ask_read.read_exact(&mut word).expect("reading the answer");
            assert_eq!(u64::from_le_bytes(word), message, "a wrong answer");
        }
        print_line(name, started, self.rounds);
        answerer.join().expect("the answering thread panicked");
    }

    /// Times `rounds` futex ping-pongs: a word that says whose turn it is,
    /// each thread waking the other after it hands the turn over.
    fn futex(&self, name: &str) {
        let turn = Arc::new(AtomicU32::new(0));
        let ready = Arc::new(Barrier::new(2));
        let (other, rounds, go, theirs) = (self.other, self.rounds, ready.clone(), turn.clone());
        let answerer = std::thread::spawn(move || {
            pin(other);
            go.wait();
            for _ in 0..rounds {
                while theirs.load(Ordering::Acquire) == 0 {
                    futex_wait(&theirs, 0);
                }
                theirs.store(0, Ordering::Release);
                futex_wake(&theirs);
            }
        });
        ready.wait();
        let started = Instant::now();
        for _ in 0..self.rounds {
            turn.store(1, Ordering::Release);
            futex_wake(&turn);
            while turn.load(Ordering::Acquire) == 1 {
                futex_wait(&turn, 1);
            }
        }
        print_line(name, started, self.rounds);
        answerer.join().expect("the answering thread panicked");
    }
}

fn pipes() -> Ends<std::io::PipeReader, std::io::PipeWriter> {
    let question = std::io::pipe().expect("cannot make a pipe");
    let answer = std::io::pipe().expect("cannot make a pipe");
    ((answer.0, question.1), (question.0, answer.1))
}

fn sockets() -> Ends<UnixStream, UnixStream> {
    let (asker, answerer) = UnixStream::pair().expect("cannot make a socket pair");
    let asker_write = asker.try_clone().expect("cannot clone a socket");
    let answerer_write = answerer.try_clone().expect("cannot clone a socket");
    ((asker, asker_write), (answerer, answerer_write))
}

fn eventfds() -> Ends<std::fs::File, std::fs::File> {
    let make = || {
        // SAFETY: eventfd takes no pointers; a negative result is an error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "cannot make an eventfd: {}", std::io::Error::last_os_error());
        // SAFETY: `fd` is a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let clone = fd.try_clone().expect("cannot duplicate an eventfd");
        (std::fs::File::from(fd), std::fs::File::from(clone))
    };
    // An eventfd adds each 8-byte write to its counter, and a read takes
    // the counter: with one message in flight, what was written.
    let (question, answer) = (make(), make());
    ((answer.0, question.1), (question.0, answer.1))
}

fn futex_wait(word: &AtomicU32, expected: u32) {
    // SAFETY: `word` is a live, aligned 32-bit atomic for the whole call;
    // the null timeout waits without a limit.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            expected,
            std::ptr::null::<libc::timespec>(),
        )
    };
}

fn futex_wake(word: &AtomicU32) {
    // SAFETY: as in `futex_wait`; FUTEX_WAKE reads nothing else.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            1,
        )
    };
}

/// The CPUs this process may run on, lowest first.
fn allowed_cpus() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain data, and all zeroes is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a writable cpu_set_t of the size given.
    let result = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    assert_eq!(result, 0, "sched_getaffinity: {}", std::io::Error::last_os_error());
    // SAFETY: every index is below CPU_SETSIZE, so within the set.
    (0..libc::CPU_SETSIZE as usize).filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) }).collect()
}

/// Pins the calling thread to `cpu`.
fn pin(cpu: usize) {
    // SAFETY: as in `allowed_cpus`.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `cpu` came from `allowed_cpus`, so it is below CPU_SETSIZE.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: pid 0 is the calling thread, and `set` is a cpu_set_t of the
    // size given.
    let result = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
    assert_eq!(result, 0, "cannot pin a thread to CPU {cpu}: {}", std::io::Error::last_os_error());
}
