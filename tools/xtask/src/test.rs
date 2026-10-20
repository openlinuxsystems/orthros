//! `ob test`: host unit tests and QEMU boot tests.
//!
//! Each boot test is a [`Case`]: a machine (firmware, media, CPUs, memory),
//! kernel flags, and a script that follows the serial console like a person
//! would, waiting for lines, typing commands to `kdb`, and reading the
//! framebuffer console from a screenshot. All cases boot copies of one test
//! image; only the kernel command line in its grub.cfg differs, written
//! into a slot reserved for it.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{Firmware, Media};
use crate::image::{self, ImageSpec};
use crate::process::{self, step};
use crate::qemu::{self, Machine, Serial};
use crate::{Args, Ctx, Error, Result, VERSION, kernel, screen};

/// QEMU's exit status after the kernel writes 0x10 to isa-debug-exit.
const QEMU_EXIT_SUCCESS: i32 = 33;
/// QEMU's exit status after the kernel writes 0x11 (a panic).
const QEMU_EXIT_PANIC: i32 = 35;
/// QEMU's exit status when the guest restarts under `-no-reboot`.
const QEMU_EXIT_RESET: i32 = 0;

/// Every library of the kernel workspace with host unit tests.
pub const KERNEL_LIBRARIES: &[&str] = &[
    "radix-acpi",
    "radix-bootinfo",
    "radix-fbcon",
    "radix-hal",
    "radix-kdb",
    "radix-klog",
    "radix-ksyms",
    "radix-microcode",
    "radix-sync",
];

/// Length of the command-line slot in the test image's grub.cfg.
const CMDLINE_SLOT: usize = 160;

/// Which test image a case boots.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestImage {
    /// The kernel alone.
    Plain,
    /// The kernel plus a microcode module of made-up updates.
    Microcode,
}

/// One boot test.
pub struct Case {
    /// Name for `--case` and the log files.
    pub name: &'static str,
    /// What it checks, for the progress output and `ob help test`.
    pub about: &'static str,
    firmware: Firmware,
    media: Media,
    cmdline: &'static str,
    cpus: u32,
    memory: &'static str,
    /// Let the guest restart, for the crash log test.
    reboot: bool,
    image: TestImage,
    /// A CPU model to emulate (without KVM) instead of the host's.
    cpu_model: Option<&'static str>,
    script: fn(&mut Vm, &Case) -> Result<(), String>,
}

/// The CPU the microcode test emulates: an Intel Skylake client part,
/// signature 0x506e3.
const MICROCODE_CPU: &str = "qemu64,vendor=GenuineIntel,family=6,model=94,stepping=3";
const MICROCODE_SIGNATURE: u32 = 0x506e3;
const MICROCODE_REVISION: u32 = 0x7f;

/// The boot tests, in the order they run.
pub const CASES: &[Case] = &[
    Case {
        name: "uefi-usb",
        about: "4 CPUs boot under UEFI from USB",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: boot,
    },
    Case {
        name: "bios-usb",
        about: "4 CPUs boot under BIOS from USB",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: boot,
    },
    Case {
        name: "uefi-cdrom",
        about: "4 CPUs boot under UEFI from CD-ROM, local APICs in xAPIC mode",
        firmware: Firmware::Uefi,
        media: Media::Cdrom,
        cmdline: "qemu-exit nox2apic",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: boot_xapic,
    },
    Case {
        name: "bios-cdrom",
        about: "4 CPUs boot under BIOS from CD-ROM",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit",
        cpus: 4,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: boot,
    },
    Case {
        name: "screen",
        about: "the boot log is readable on the UEFI framebuffer",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: screen_log,
    },
    Case {
        name: "page-fault",
        about: "a page fault panics with registers and a symbolized backtrace, on screen too",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "selftest=page-fault",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: page_fault,
    },
    Case {
        name: "crash-log-uefi",
        about: "the panic report survives a restart under UEFI",
        firmware: Firmware::Uefi,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=crash-reboot panic=0",
        cpus: 2,
        // With more than 4 GiB the log goes above 4 GiB, as on the Dell.
        memory: "5G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        script: crash_reboot,
    },
    Case {
        name: "crash-log-bios",
        about: "the panic report survives a restart under BIOS",
        firmware: Firmware::Bios,
        media: Media::Usb,
        cmdline: "qemu-exit selftest=crash-reboot panic=0",
        cpus: 2,
        // With less, the log goes at 64 MiB.
        memory: "1G",
        reboot: true,
        image: TestImage::Plain,
        cpu_model: None,
        script: crash_reboot,
    },
    Case {
        name: "kdb",
        about: "kdb stops at a breakpoint and answers every command on the serial port",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit kdb trace selftest=breakpoint",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Plain,
        cpu_model: None,
        script: kdb,
    },
    Case {
        name: "microcode",
        about: "a microcode module is found, checked and matched to an emulated Intel CPU",
        firmware: Firmware::Bios,
        media: Media::Cdrom,
        cmdline: "qemu-exit",
        cpus: 2,
        memory: "1G",
        reboot: false,
        image: TestImage::Microcode,
        cpu_model: Some(MICROCODE_CPU),
        script: microcode,
    },
];

/// `ob test`.
pub fn command(ctx: &Ctx, args: &mut Args) -> Result<()> {
    let which = args.positional();
    let timeout = match args.value("--timeout")? {
        Some(value) => value.parse::<u64>().ok().filter(|&secs| secs > 0).ok_or_else(|| {
            Error::new(format!("--timeout takes a number of seconds, not `{value}`"))
        })?,
        None => 300,
    };
    let mut selected = Vec::new();
    while let Some(name) = args.value("--case")? {
        selected.push(name);
    }
    let profile = args.profile(&ctx.config)?;
    args.finish()?;
    let (unit, boot) = match which.as_deref() {
        None if selected.is_empty() => (true, true),
        None | Some("boot") => (false, true),
        Some("unit") if selected.is_empty() => (true, false),
        Some("unit") => crate::bail!("--case selects boot tests; it cannot be used with `unit`"),
        Some(other) => crate::bail!("unknown test suite `{other}`; use `unit` or `boot`"),
    };
    let cases: Vec<&Case> = if selected.is_empty() {
        CASES.iter().collect()
    } else {
        let mut cases = Vec::new();
        for name in &selected {
            match CASES.iter().find(|case| case.name == name) {
                Some(case) => cases.push(case),
                None => {
                    let names: Vec<_> = CASES.iter().map(|case| case.name).collect();
                    crate::bail!("no boot test `{name}`; the tests are {}", names.join(", "));
                }
            }
        }
        cases
    };

    if unit {
        unit_tests(ctx)?;
    }
    if boot {
        let kernel = kernel::build(ctx, profile)?;
        boot_tests(ctx, &kernel, &cases, Duration::from_secs(timeout))?;
    }
    Ok(())
}

fn unit_tests(ctx: &Ctx) -> Result<()> {
    step("Running unit tests for the build tool");
    let mut cmd = process::cargo(ctx);
    cmd.args(["test", "--workspace"]);
    process::run(ctx, &mut cmd)?;

    step("Running unit tests for the kernel libraries on the host");
    let mut cmd = kernel::cargo(ctx, "test");
    for library in KERNEL_LIBRARIES {
        cmd.args(["-p", library]);
    }
    process::run(ctx, &mut cmd)
}

/// Made-up microcode: an update for the emulated CPU and one for another.
pub fn test_microcode() -> Vec<u8> {
    let mut file = vec![0u8; 4096];
    let mut len = 0;
    for (revision, signature, platforms) in
        [(MICROCODE_REVISION, MICROCODE_SIGNATURE, 0xff), (0x10, 0x806ea, 0xc0)]
    {
        len += radix_microcode::build::update(
            revision,
            signature,
            platforms,
            16,
            &[],
            &mut file[len..],
        );
    }
    file.truncate(len);
    file
}

/// Builds a test image whose kernel command line is a placeholder slot.
fn build_test_image(ctx: &Ctx, kernel: &Path, dir: &Path, kind: TestImage) -> Result<Vec<u8>> {
    let name = match kind {
        TestImage::Plain => "orthros-test",
        TestImage::Microcode => "orthros-test-microcode",
    };
    let output = dir.join(format!("{name}.img"));
    image::build(
        ctx,
        &ImageSpec {
            kernel,
            timeout: 0,
            cmdline: &cmdline_slot(),
            microcode: (kind == TestImage::Microcode).then(test_microcode),
            staging: dir.join(format!("{name}-root")),
            output: output.clone(),
        },
    )?;
    std::fs::read(&output).map_err(|err| Error::new(format!("reading {}: {err}", output.display())))
}

/// The placeholder for the kernel command line in the test images.
fn cmdline_slot() -> String {
    format!("{:-<CMDLINE_SLOT$}", "ob-test-cmdline")
}

/// A copy of a test image with `cmdline` in its command-line slot.
fn with_cmdline(image: &[u8], cmdline: &str) -> Result<Vec<u8>, String> {
    let slot = cmdline_slot();
    let found: Vec<usize> = image
        .windows(slot.len())
        .enumerate()
        .filter(|(_, window)| *window == slot.as_bytes())
        .map(|(at, _)| at)
        .collect();
    let [at] = found[..] else {
        return Err(format!(
            "the test image holds the command-line slot {} times, not once",
            found.len()
        ));
    };
    if cmdline.len() > CMDLINE_SLOT {
        return Err(format!("the command line is longer than {CMDLINE_SLOT} bytes"));
    }
    let mut copy = image.to_vec();
    copy[at..at + CMDLINE_SLOT].copy_from_slice(format!("{cmdline:<CMDLINE_SLOT$}").as_bytes());
    Ok(copy)
}

fn boot_tests(ctx: &Ctx, kernel: &Path, cases: &[&Case], timeout: Duration) -> Result<()> {
    qemu::require_qemu()?;
    let dir = ctx.out.join("test");
    process::create_dir(&dir)?;
    let plain = build_test_image(ctx, kernel, &dir, TestImage::Plain)?;
    let microcode = if cases.iter().any(|case| case.image == TestImage::Microcode) {
        Some(build_test_image(ctx, kernel, &dir, TestImage::Microcode)?)
    } else {
        None
    };

    let kvm = qemu::kvm_available();
    step(&format!(
        "Running {} boot tests in QEMU ({})",
        cases.len(),
        if kvm { "KVM" } else { "no KVM, so each boot takes longer" }
    ));
    let mut failures = 0;
    for case in cases {
        let base = match case.image {
            TestImage::Plain => &plain,
            TestImage::Microcode => microcode.as_ref().expect("built when a case needs it"),
        };
        let started = Instant::now();
        let log = dir.join(format!("boot-{}.log", case.name));
        let outcome = run_case(ctx, case, base, &dir, kvm, timeout);
        let secs = started.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => eprintln!("    PASS  {:<15} {} ({secs:.1} s)", case.name, case.about),
            Err(reason) => {
                failures += 1;
                eprintln!("    FAIL  {:<15} {} ({secs:.1} s)", case.name, case.about);
                eprintln!("          {reason}");
                eprintln!("          serial log: {}", ctx.display(&log));
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                for line in
                    clean_log(&text).lines().rev().take(15).collect::<Vec<_>>().into_iter().rev()
                {
                    eprintln!("          | {line}");
                }
            }
        }
    }
    if failures > 0 {
        crate::bail!("{failures} of {} boot tests failed", cases.len());
    }
    step(&format!("All {} boot tests passed", cases.len()));
    Ok(())
}

fn run_case(
    ctx: &Ctx,
    case: &Case,
    base: &[u8],
    dir: &Path,
    kvm: bool,
    timeout: Duration,
) -> Result<(), String> {
    let image = dir.join(format!("{}.img", case.name));
    std::fs::write(&image, with_cmdline(base, case.cmdline)?)
        .map_err(|err| format!("writing {}: {err}", image.display()))?;
    let qmp = dir.join(format!("{}.qmp", case.name));
    let _ = std::fs::remove_file(&qmp);
    let machine = Machine {
        firmware: case.firmware,
        media: case.media,
        ovmf: match case.firmware {
            Firmware::Uefi => Some(
                qemu::prepare_ovmf(ctx, &format!("test-{}", case.name))
                    .map_err(|e| e.to_string())?,
            ),
            Firmware::Bios => None,
        },
        image: image.clone(),
        memory: case.memory.into(),
        cpus: case.cpus,
        kvm: kvm && case.cpu_model.is_none(),
        cpu_model: case.cpu_model.map(str::to_string),
        serial: Serial::Pipe,
        display: false,
        reboot: case.reboot,
        gdb: false,
        qmp: Some(qmp),
        extra: Vec::new(),
    };
    let mut vm = Vm::start(ctx, &machine, dir, case.name, timeout)?;
    let result = (case.script)(&mut vm, case);
    vm.stop();
    let _ = std::fs::remove_file(&image);
    result
}

/// A running test machine.
pub struct Vm {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    qmp: Option<PathBuf>,
    /// End of the last match in the cleaned log.
    cursor: usize,
    deadline: Instant,
    timeout: Duration,
    stderr_log: PathBuf,
    screenshot: PathBuf,
    /// Whether QEMU runs the guest with KVM.
    kvm: bool,
}

impl Vm {
    fn start(
        ctx: &Ctx,
        machine: &Machine,
        dir: &Path,
        name: &str,
        timeout: Duration,
    ) -> Result<Vm, String> {
        let log_path = dir.join(format!("boot-{name}.log"));
        let stderr_log = dir.join(format!("boot-{name}.qemu.log"));
        let stderr = std::fs::File::create(&stderr_log)
            .map_err(|err| format!("creating {}: {err}", stderr_log.display()))?;
        let mut log = std::fs::File::create(&log_path)
            .map_err(|err| format!("creating {}: {err}", log_path.display()))?;
        let mut cmd = machine.command();
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
        if ctx.verbose {
            eprintln!("+ {}", process::render(&cmd));
        }
        let mut child = cmd.spawn().map_err(|err| format!("cannot start QEMU: {err}"))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stdout.read(&mut buf) {
                let _ = log.write_all(&buf[..n]);
                sink.lock().expect("the log lock is not poisoned").extend_from_slice(&buf[..n]);
            }
        });
        Ok(Vm {
            stdin: child.stdin.take(),
            child,
            output,
            reader: Some(reader),
            qmp: machine.qmp.clone(),
            cursor: 0,
            deadline: Instant::now() + timeout,
            timeout,
            stderr_log,
            screenshot: dir.join(format!("{name}.ppm")),
            kvm: machine.kvm,
        })
    }

    /// The serial output so far, cleaned of escape sequences.
    pub fn log(&self) -> String {
        clean_log(&String::from_utf8_lossy(&self.output.lock().expect("not poisoned")))
    }

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn qemu_stderr(&self) -> String {
        let text = std::fs::read_to_string(&self.stderr_log).unwrap_or_default();
        let text = text.trim();
        if text.is_empty() { String::new() } else { format!("; QEMU said: {text}") }
    }

    /// Waits until `needle` appears after the previous match.
    pub fn expect(&mut self, needle: &str) -> Result<(), String> {
        loop {
            let log = self.log();
            if let Some(at) = log.get(self.cursor..).and_then(|rest| rest.find(needle)) {
                self.cursor += at + needle.len();
                return Ok(());
            }
            if let Some(status) = self.exited() {
                // Let the reader collect the last output, then look again.
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                    continue;
                }
                return Err(format!(
                    "QEMU exited ({status}) before printing \"{needle}\"{}",
                    self.qemu_stderr()
                ));
            }
            if Instant::now() >= self.deadline {
                return Err(format!("no \"{needle}\" within {} s", self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Types `text` on the serial port.
    pub fn send(&mut self, text: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("QEMU's input is closed")?;
        // A terminal sends what is typed a byte at a time; so does this, a
        // little apart, so the guest's 16-byte receive FIFO never overflows.
        for byte in text.bytes() {
            stdin.write_all(&[byte]).and_then(|()| stdin.flush()).map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    /// Runs a `kdb` command and waits for each of `expected`, then for the
    /// next prompt.
    pub fn kdb(&mut self, command: &str, expected: &[&str]) -> Result<(), String> {
        self.send(command)?;
        self.send("\r")?;
        for needle in expected {
            self.expect(needle).map_err(|err| format!("kdb `{command}`: {err}"))?;
        }
        self.expect("kdb> ").map_err(|err| format!("kdb `{command}`: {err}"))
    }

    /// Waits for QEMU to exit and checks its status.
    pub fn expect_exit(&mut self, code: i32) -> Result<(), String> {
        loop {
            if let Some(status) = self.exited() {
                return match status.code() {
                    Some(c) if c == code => Ok(()),
                    Some(QEMU_EXIT_PANIC) => Err("the kernel panicked".into()),
                    _ => Err(format!("QEMU exited with {status}{}", self.qemu_stderr())),
                };
            }
            if Instant::now() >= self.deadline {
                return Err(format!("QEMU still running after {} s", self.timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the framebuffer console shows each of `needles`. The
    /// serial port gets every message before the screen does, and drawing
    /// is slow under emulation, so this takes screenshots until the screen
    /// catches up or 15 seconds pass.
    pub fn expect_screen(&mut self, needles: &[&str]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let screen = self.screen()?;
            match screen_shows(&screen, needles) {
                Ok(()) => return Ok(()),
                Err(err) if Instant::now() >= deadline => return Err(err),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }

    /// Takes a screenshot and reads the framebuffer console's text.
    pub fn screen(&mut self) -> Result<Vec<String>, String> {
        let geometry = screen::Geometry::from_log(&self.log())
            .ok_or("the kernel did not report a framebuffer console")?;
        let socket = self.qmp.clone().ok_or("no QMP socket")?;
        let _ = std::fs::remove_file(&self.screenshot);
        let mut qmp = Qmp::connect(&socket)?;
        qmp.execute(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#,
            self.screenshot.display()
        ))?;
        let bytes = std::fs::read(&self.screenshot)
            .map_err(|err| format!("reading {}: {err}", self.screenshot.display()))?;
        let image = screen::Image::parse_ppm(&bytes)?;
        screen::read_text(&image, geometry)
    }

    fn stop(&mut self) {
        if self.exited().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stdin = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // QEMU leaves its QMP socket behind.
        if let Some(socket) = &self.qmp {
            let _ = std::fs::remove_file(socket);
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A connection to QEMU's machine protocol.
struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(path: &Path) -> Result<Qmp, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(format!("connecting to QMP at {}: {err}", path.display())),
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut qmp = Qmp { stream, reader };
        qmp.read_line()?; // the greeting
        qmp.execute(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(qmp)
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("QMP closed the connection".into()),
            Ok(_) => Ok(line),
            Err(err) => Err(format!("reading from QMP: {err}")),
        }
    }

    /// Sends a command and waits for its reply, skipping events.
    fn execute(&mut self, command: &str) -> Result<(), String> {
        writeln!(self.stream, "{command}").map_err(|err| format!("writing to QMP: {err}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains(r#""return""#) {
                return Ok(());
            }
            if line.contains(r#""error""#) {
                return Err(format!("QMP: {}", line.trim()));
            }
        }
    }
}

/// Removes carriage returns and terminal escape sequences from a serial log.
pub fn clean_log(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\u{1b}' => {
                // CSI sequences end with a byte in 0x40..=0x7e.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// Fails unless some line of `screen` contains each of `needles`.
fn screen_shows(screen: &[String], needles: &[&str]) -> Result<(), String> {
    for needle in needles {
        if !screen.iter().any(|line| line.contains(needle)) {
            let shown: Vec<&str> =
                screen.iter().map(String::as_str).filter(|l| !l.is_empty()).collect();
            let tail = shown[shown.len().saturating_sub(8)..].join(" | ");
            return Err(format!("the screen does not show \"{needle}\"; its last lines: {tail}"));
        }
    }
    Ok(())
}

// ---- The test scripts ----

/// The checks every normal boot passes, in the order the kernel logs them.
fn boot_report(vm: &mut Vm, case: &Case) -> Result<(), String> {
    vm.expect(&format!("Radix {VERSION} (OrthrOS Phase 1)"))?;
    vm.expect(&format!("Firmware: {}", case.firmware))?;
    vm.expect("Memory: ")?;
    vm.expect(" MiB usable in ")?;
    vm.expect("CPU: ")?;
    vm.expect("ACPI: RSDP")?;
    vm.expect("ACPI:   APIC at ")?;
    vm.expect("APIC: ")?;
    vm.expect("Time: TSC at ")?;
    let cpus = case.cpus;
    vm.expect(&format!("CPUs: {cpus} of {cpus} online:"))?;
    for cpu in 0..cpus {
        vm.expect(&format!("cpu{cpu}: APIC ID "))?;
    }
    vm.expect(&format!("Timer: all {cpus} CPUs take timer interrupts at 100 Hz"))?;
    if cpus > 1 {
        vm.expect(&format!("IPI: all {} other CPUs answered a ping", cpus - 1))?;
    }
    vm.expect("Boot complete.")
}

fn boot(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    // KVM always offers x2APIC, which QEMU 8's TCG lacks, so with KVM these
    // cases cover the x2APIC path; make sure the kernel took it.
    if vm.kvm && !vm.log().contains("APIC: x2APIC mode") {
        return Err("the local APIC is not in x2APIC mode under KVM".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn boot_xapic(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    if !vm.log().contains("APIC: xAPIC mode") {
        return Err("the local APIC is not in xAPIC mode despite nox2apic".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn screen_log(vm: &mut Vm, case: &Case) -> Result<(), String> {
    boot_report(vm, case)?;
    vm.expect_screen(&["Timer: all 2 CPUs take timer interrupts", "Boot complete."])
}

fn page_fault(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("selftest page-fault: touching an unmapped address")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("Page fault (#PF)")?;
    vm.expect("while writing 0xdead0000: the page is not mapped")?;
    vm.expect("Registers:")?;
    vm.expect("cr2 00000000dead0000")?;
    vm.expect("Backtrace:")?;
    vm.expect("self_test_fault")?;
    vm.expect("radix::selftest::page_fault+0x")?;
    vm.expect("radix_kernel_main+0x")?;
    vm.expect("Press a key to restart")?;
    vm.expect_screen(&[
        "RADIX KERNEL PANIC",
        "Radix panic on CPU 0",
        "Page fault (#PF)",
        "radix::selftest::page_fault+0x",
        "Recent kernel log:",
        "Press a key to restart",
    ])?;
    // A key on the serial port restarts the machine, which ends QEMU.
    vm.send("\r")?;
    vm.expect_exit(QEMU_EXIT_RESET)
}

fn crash_reboot(vm: &mut Vm, case: &Case) -> Result<(), String> {
    // The log's place depends only on the memory map: the first 2 MiB
    // boundary above 4 GiB when there is RAM there, otherwise 64 MiB.
    let start: u64 = if case.memory == "1G" { 64 << 20 } else { 1 << 32 };
    vm.expect(&format!("{start:#014x}-{:#014x}     1 MiB  crash log", start + 0xf_ffff))?;
    vm.expect("Crash log: boot 1;")?;
    vm.expect("selftest crash-reboot: panicking now")?;
    vm.expect("Radix panic on CPU 0")?;
    vm.expect("selftest crash-reboot: deliberate panic")?;
    // The machine restarts and boots again.
    vm.expect("Crash log: boot 2; the previous boot (1) ended with a kernel panic")?;
    vm.expect("| [")?;
    vm.expect("selftest crash-reboot: deliberate panic")?;
    vm.expect("selftest crash-reboot: passed")?;
    vm.expect("Boot complete.")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn kdb(vm: &mut Vm, _case: &Case) -> Result<(), String> {
    vm.expect("selftest breakpoint: running a breakpoint instruction")?;
    vm.expect("kdb on CPU 0, breakpoint at ")?;
    vm.expect("kdb> ")?;
    vm.kdb("regs", &["rip", "rflags", "cr3"])?;
    vm.kdb("bt", &["radix::selftest::breakpoint", "radix_kernel_main"])?;
    vm.send("continue\r")?;
    vm.expect("kdb: continuing")?;
    vm.expect("selftest breakpoint: passed")?;
    vm.expect("Boot complete.")?;
    vm.expect("kdb on CPU 0, boot complete")?;
    vm.expect("kdb> ")?;

    let names: Vec<&str> = radix_kdb::COMMANDS.iter().map(|c| c.name).collect();
    vm.kdb("help", &names)?;
    vm.kdb("cpus", &["2 CPUs", "cpu0: APIC ID", "cpu1: APIC ID", "timer interrupts"])?;
    vm.kdb("mem", &["Memory map:", "usable", "Reserved by the kernel:", "kernel image"])?;
    vm.kdb("acpi", &["ACPI: RSDP", "APIC"])?;
    vm.kdb("log 4", &["Boot complete."])?;
    vm.kdb("trace", &["boot.entry", "events shown"])?;
    vm.kdb("regs", &["no saved registers"])?;
    vm.kdb("sym radix_kernel_main", &["radix_kernel_main is at 0x"])?;
    let log = vm.log();
    let address = log
        .rsplit("radix_kernel_main is at 0x")
        .next()
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or("no address in the `sym` answer")?
        .to_string();
    vm.kdb(&format!("sym 0x{address}"), &["is radix_kernel_main"])?;
    vm.kdb(&format!("x 0x{address} 32"), &[&format!("{address:0>16} ")])?;
    vm.kdb("x 0xdead0000 16", &["not mapped"])?;
    vm.kdb("frobnicate", &["unknown command `frobnicate`"])?;
    vm.send("continue\r")?;
    vm.expect("kdb: continuing")?;
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

fn microcode(vm: &mut Vm, case: &Case) -> Result<(), String> {
    // The boot CPU loads microcode before anything else, so these lines
    // come before the kernel's banner.
    let module = test_microcode().len();
    vm.expect(&format!(
        "microcode: module of {module} bytes with 2 updates; CPU signature {MICROCODE_SIGNATURE:#x}"
    ))?;
    // QEMU accepts the update but cannot apply it, so the revision stays.
    vm.expect(&format!("microcode: revision {MICROCODE_REVISION:#x} did not take effect"))?;
    boot_report(vm, case)?;
    if !vm.log().contains("(GenuineIntel, family 0x6, model 0x5e, stepping 3, microcode revision") {
        return Err("the CPU line does not show the emulated Intel CPU".into());
    }
    vm.expect_exit(QEMU_EXIT_SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        assert_eq!(clean_log("\u{1b}[1;1HRadix\r\n\u{1b}[=3hok"), "Radix\nok");
    }

    #[test]
    fn fills_the_command_line_slot() {
        let slot = cmdline_slot();
        let image = format!("....multiboot2 /boot/radix {slot}\n....").into_bytes();
        let filled = with_cmdline(&image, "qemu-exit kdb").unwrap();
        let text = String::from_utf8(filled).unwrap();
        assert!(text.contains("/boot/radix qemu-exit kdb   "));
        assert_eq!(text.len(), image.len());
        assert!(with_cmdline(b"no slot", "x").is_err());
        let twice = format!("{slot}{slot}").into_bytes();
        assert!(with_cmdline(&twice, "x").is_err());
    }

    #[test]
    fn case_names_are_unique_and_command_lines_fit() {
        for (i, case) in CASES.iter().enumerate() {
            assert!(CASES[..i].iter().all(|other| other.name != case.name), "{}", case.name);
            assert!(case.cmdline.len() <= CMDLINE_SLOT);
            crate::config::validate_cmdline(case.cmdline).unwrap();
        }
    }

    #[test]
    fn test_microcode_parses() {
        let file = test_microcode();
        assert_eq!(radix_microcode::updates(&file).filter(Result::is_ok).count(), 2);
        let cpu = radix_microcode::CpuId { signature: MICROCODE_SIGNATURE, platform: 1 };
        assert_eq!(radix_microcode::find(&file, cpu).unwrap().revision(), MICROCODE_REVISION);
    }
}
