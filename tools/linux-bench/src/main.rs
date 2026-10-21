// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `linux-bench`: the cost on the host's Linux of the operations that
//! `ob bench` measures on Radix, as a baseline (docs/design/ipc.md).
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
//! on one CPU and once on two. `ob bench` builds and runs it.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
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
    report("getppid", calls, || {
        // SAFETY: getppid takes no arguments and cannot fail. It is called
        // through syscall(2) so that no library caches its answer.
        unsafe { libc::syscall(libc::SYS_getppid) };
    });
    report("sched_yield", calls, || {
        // SAFETY: sched_yield takes no arguments.
        unsafe { libc::sched_yield() };
    });
    let mut bytes = [0u8; 64];
    report("getrandom_64", calls / 4, || {
        // SAFETY: `bytes` is 64 writable bytes.
        let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        assert_eq!(got, 64, "getrandom returned {got}");
    });

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
