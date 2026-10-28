// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! lscpu(1): what the processor is, from CPUID, which a program may run
//! itself on x86-64, and how many CPUs the kernel runs, from
//! `/kernel/cpus`, laid out as util-linux's lscpu lays it out (ADR 0144).
//! The flags are the CPUID bits Linux's `/proc/cpuinfo` names, without
//! the flags Linux's kernel adds of its own (`constant_tsc`, `pti`).

use std::io::{self, Write};

use crate::args::Args;
use crate::util::{Fail, fail};

/// The registers CPUID leaf `leaf`, subleaf `sub` gives: EAX, EBX, ECX,
/// EDX.
type Cpuid<'a> = &'a dyn Fn(u32, u32) -> [u32; 4];

/// This processor's CPUID.
#[cfg(target_arch = "x86_64")]
fn cpuid(leaf: u32, sub: u32) -> [u32; 4] {
    // SAFETY: every x86-64 processor has CPUID, which only reads.
    #[allow(unused_unsafe)]
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, sub) };
    [r.eax, r.ebx, r.ecx, r.edx]
}

/// Names by bit.
type Bits = &'static [(u32, &'static str)];

/// The Linux names of the bits of a CPUID register, in Linux's order of
/// feature words: leaf, subleaf, register (0 EAX to 3 EDX) and the names
/// by bit.
const FLAGS: &[(u32, u32, usize, Bits)] = &[
    (
        1,
        0,
        3,
        &[
            (0, "fpu"),
            (1, "vme"),
            (2, "de"),
            (3, "pse"),
            (4, "tsc"),
            (5, "msr"),
            (6, "pae"),
            (7, "mce"),
            (8, "cx8"),
            (9, "apic"),
            (11, "sep"),
            (12, "mtrr"),
            (13, "pge"),
            (14, "mca"),
            (15, "cmov"),
            (16, "pat"),
            (17, "pse36"),
            (18, "pn"),
            (19, "clflush"),
            (21, "dts"),
            (22, "acpi"),
            (23, "mmx"),
            (24, "fxsr"),
            (25, "sse"),
            (26, "sse2"),
            (27, "ss"),
            (28, "ht"),
            (29, "tm"),
            (30, "ia64"),
            (31, "pbe"),
        ],
    ),
    (
        0x8000_0001,
        0,
        3,
        &[
            (11, "syscall"),
            (19, "mp"),
            (20, "nx"),
            (22, "mmxext"),
            (25, "fxsr_opt"),
            (26, "pdpe1gb"),
            (27, "rdtscp"),
            (29, "lm"),
            (30, "3dnowext"),
            (31, "3dnow"),
        ],
    ),
    (
        1,
        0,
        2,
        &[
            (0, "pni"),
            (1, "pclmulqdq"),
            (2, "dtes64"),
            (3, "monitor"),
            (4, "ds_cpl"),
            (5, "vmx"),
            (6, "smx"),
            (7, "est"),
            (8, "tm2"),
            (9, "ssse3"),
            (10, "cid"),
            (11, "sdbg"),
            (12, "fma"),
            (13, "cx16"),
            (14, "xtpr"),
            (15, "pdcm"),
            (17, "pcid"),
            (18, "dca"),
            (19, "sse4_1"),
            (20, "sse4_2"),
            (21, "x2apic"),
            (22, "movbe"),
            (23, "popcnt"),
            (24, "tsc_deadline_timer"),
            (25, "aes"),
            (26, "xsave"),
            (28, "avx"),
            (29, "f16c"),
            (30, "rdrand"),
            (31, "hypervisor"),
        ],
    ),
    (
        0x8000_0001,
        0,
        2,
        &[
            (0, "lahf_lm"),
            (1, "cmp_legacy"),
            (2, "svm"),
            (3, "extapic"),
            (4, "cr8_legacy"),
            (5, "abm"),
            (6, "sse4a"),
            (7, "misalignsse"),
            (8, "3dnowprefetch"),
            (9, "osvw"),
            (10, "ibs"),
            (11, "xop"),
            (12, "skinit"),
            (13, "wdt"),
            (15, "lwp"),
            (16, "fma4"),
            (17, "tce"),
            (19, "nodeid_msr"),
            (21, "tbm"),
            (22, "topoext"),
            (23, "perfctr_core"),
            (24, "perfctr_nb"),
            (26, "bpext"),
            (27, "ptsc"),
            (28, "perfctr_llc"),
            (29, "mwaitx"),
        ],
    ),
    (
        7,
        0,
        1,
        &[
            (0, "fsgsbase"),
            (1, "tsc_adjust"),
            (2, "sgx"),
            (3, "bmi1"),
            (4, "hle"),
            (5, "avx2"),
            (7, "smep"),
            (8, "bmi2"),
            (9, "erms"),
            (10, "invpcid"),
            (11, "rtm"),
            (12, "cqm"),
            (14, "mpx"),
            (15, "rdt_a"),
            (16, "avx512f"),
            (17, "avx512dq"),
            (18, "rdseed"),
            (19, "adx"),
            (20, "smap"),
            (21, "avx512ifma"),
            (23, "clflushopt"),
            (24, "clwb"),
            (25, "intel_pt"),
            (26, "avx512pf"),
            (27, "avx512er"),
            (28, "avx512cd"),
            (29, "sha_ni"),
            (30, "avx512bw"),
            (31, "avx512vl"),
        ],
    ),
    (0xd, 1, 0, &[(0, "xsaveopt"), (1, "xsavec"), (2, "xgetbv1"), (3, "xsaves"), (4, "xfd")]),
    (7, 1, 0, &[(4, "avx_vnni"), (5, "avx512_bf16")]),
    (
        0x8000_0008,
        0,
        1,
        &[(0, "clzero"), (1, "irperf"), (2, "xsaveerptr"), (4, "rdpru"), (9, "wbnoinvd")],
    ),
    (
        6,
        0,
        0,
        &[
            (0, "dtherm"),
            (1, "ida"),
            (2, "arat"),
            (4, "pln"),
            (6, "pts"),
            (7, "hwp"),
            (8, "hwp_notify"),
            (9, "hwp_act_window"),
            (10, "hwp_epp"),
            (11, "hwp_pkg_req"),
        ],
    ),
    (
        7,
        0,
        2,
        &[
            (1, "avx512vbmi"),
            (2, "umip"),
            (3, "pku"),
            (4, "ospke"),
            (5, "waitpkg"),
            (6, "avx512_vbmi2"),
            (8, "gfni"),
            (9, "vaes"),
            (10, "vpclmulqdq"),
            (11, "avx512_vnni"),
            (12, "avx512_bitalg"),
            (13, "tme"),
            (14, "avx512_vpopcntdq"),
            (16, "la57"),
            (22, "rdpid"),
            (24, "bus_lock_detect"),
            (25, "cldemote"),
            (27, "movdiri"),
            (28, "movdir64b"),
            (29, "enqcmd"),
            (30, "sgx_lc"),
        ],
    ),
    (
        7,
        0,
        3,
        &[
            (2, "avx512_4vnniw"),
            (3, "avx512_4fmaps"),
            (4, "fsrm"),
            (8, "avx512_vp2intersect"),
            (10, "md_clear"),
            (14, "serialize"),
            (16, "tsxldtrk"),
            (18, "pconfig"),
            (19, "arch_lbr"),
            (20, "ibt"),
            (22, "amx_bf16"),
            (23, "avx512_fp16"),
            (24, "amx_tile"),
            (25, "amx_int8"),
            (28, "flush_l1d"),
            (29, "arch_capabilities"),
        ],
    ),
];

/// The 12 characters of three registers, as the vendor and hypervisor
/// names are kept.
fn chars(registers: [u32; 3]) -> String {
    let bytes: Vec<u8> = registers.iter().flat_map(|r| r.to_le_bytes()).collect();
    String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string()
}

/// One cache: its level and kind, its size, and how many logical CPUs
/// share each one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cache {
    name: &'static str,
    bytes: u64,
    shared_by: u32,
}

/// What lscpu shows of the processor.
#[derive(Debug, PartialEq, Eq)]
struct Processor {
    vendor: String,
    model_name: Option<String>,
    family: u32,
    model: u32,
    stepping: u32,
    /// Logical CPUs a core runs, and a package holds.
    threads_per_core: u32,
    per_package: u32,
    address_sizes: Option<(u32, u32)>,
    flags: Vec<&'static str>,
    hypervisor: Option<String>,
    caches: Vec<Cache>,
}

/// The caches leaf `leaf` (4 on Intel, 0x8000001d on AMD) describes,
/// one subleaf each.
fn deterministic_caches(cpuid: Cpuid<'_>, leaf: u32) -> Vec<Cache> {
    let mut caches = Vec::new();
    for sub in 0..16 {
        let [a, b, c, _] = cpuid(leaf, sub);
        let name = match (a & 0x1f, (a >> 5) & 7) {
            (0, _) => break,
            (1, 1) => "L1d:",
            (2, 1) => "L1i:",
            (_, 2) => "L2:",
            (_, 3) => "L3:",
            (_, 4) => "L4:",
            _ => continue,
        };
        let ways = u64::from((b >> 22) + 1);
        let partitions = u64::from(((b >> 12) & 0x3ff) + 1);
        let line = u64::from((b & 0xfff) + 1);
        let sets = u64::from(c) + 1;
        let shared_by = ((a >> 14) & 0xfff) + 1;
        caches.push(Cache { name, bytes: ways * partitions * line * sets, shared_by });
    }
    caches
}

impl Processor {
    fn read(cpuid: Cpuid<'_>) -> Processor {
        let [max, b, c, d] = cpuid(0, 0);
        let vendor = chars([b, d, c]);
        let [max_ext, ..] = cpuid(0x8000_0000, 0);
        let leaf = |leaf: u32, sub: u32| -> [u32; 4] {
            let present = if leaf >= 0x8000_0000 { leaf <= max_ext } else { leaf <= max };
            if present { cpuid(leaf, sub) } else { [0; 4] }
        };
        let [signature, misc, features, _] = leaf(1, 0);
        let base_family = (signature >> 8) & 0xf;
        let family =
            if base_family == 0xf { base_family + ((signature >> 20) & 0xff) } else { base_family };
        let mut model = (signature >> 4) & 0xf;
        if base_family == 6 || base_family == 0xf {
            model |= ((signature >> 16) & 0xf) << 4;
        }
        let model_name = (max_ext >= 0x8000_0004).then(|| {
            let mut bytes = Vec::new();
            for l in 0x8000_0002..=0x8000_0004 {
                bytes.extend(cpuid(l, 0).iter().flat_map(|r| r.to_le_bytes()));
            }
            let text = String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string();
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        });
        let model_name = model_name.filter(|name| !name.is_empty());
        let mut flags = Vec::new();
        for &(l, sub, register, names) in FLAGS {
            let value = leaf(l, sub)[register];
            flags.extend(names.iter().filter(|(bit, _)| value & (1 << bit) != 0).map(|(_, n)| *n));
        }
        let hypervisor = (features & (1 << 31) != 0).then(|| {
            let [_, b, c, d] = cpuid(0x4000_0000, 0);
            match chars([b, c, d]).as_str() {
                "KVMKVMKVM" => "KVM".to_string(),
                "Microsoft Hv" => "Microsoft".to_string(),
                "VMwareVMware" => "VMware".to_string(),
                "XenVMMXenVMM" => "Xen".to_string(),
                "TCGTCGTCGTCG" => "TCG".to_string(),
                " lrpepyh  vr" => "Parallels".to_string(),
                "ACRNACRNACRN" => "ACRN".to_string(),
                other => other.to_string(),
            }
        });
        let amd = vendor == "AuthenticAMD" || vendor == "HygonGenuine";
        let topoext = leaf(0x8000_0001, 0)[2] & (1 << 22) != 0;
        let (threads_per_core, per_package) = topology(&leaf, misc, features, amd, topoext);
        let caches = if amd && topoext {
            deterministic_caches(&leaf, 0x8000_001d)
        } else if !amd && max >= 4 {
            deterministic_caches(&leaf, 4)
        } else {
            amd_legacy_caches(&leaf, threads_per_core, per_package)
        };
        let [sizes, ..] = leaf(0x8000_0008, 0);
        let address_sizes = (sizes != 0).then_some((sizes & 0xff, (sizes >> 8) & 0xff));
        Processor {
            vendor,
            model_name,
            family,
            model,
            stepping: signature & 0xf,
            threads_per_core,
            per_package,
            address_sizes,
            flags,
            hypervisor,
            caches,
        }
    }
}

/// Logical CPUs a core runs, and a package holds: leaf 0xb's levels
/// where it has them, else AMD's extended leaves, else leaf 1's count.
fn topology(
    leaf: &dyn Fn(u32, u32) -> [u32; 4],
    misc: u32,
    features: u32,
    amd: bool,
    topoext: bool,
) -> (u32, u32) {
    let (mut threads, mut package) = (0, 0);
    for sub in 0..8 {
        let [_, b, c, _] = leaf(0xb, sub);
        match (c >> 8) & 0xff {
            0 => break,
            1 => threads = b & 0xffff,
            2 => package = b & 0xffff,
            _ => {}
        }
    }
    if threads > 0 && package >= threads {
        return (threads, package);
    }
    if amd {
        let cores = (leaf(0x8000_0008, 0)[2] & 0xff) + 1;
        let threads = if topoext { ((leaf(0x8000_001e, 0)[1] >> 8) & 0xff) + 1 } else { 1 };
        return (threads, cores.max(threads));
    }
    let package = if features & (1 << 28) != 0 { ((misc >> 16) & 0xff).max(1) } else { 1 };
    (1, package)
}

/// AMD's older cache leaves, which say nothing of sharing: each core has
/// its own L1 and L2, and the package one L3.
fn amd_legacy_caches(
    leaf: &dyn Fn(u32, u32) -> [u32; 4],
    threads: u32,
    package: u32,
) -> Vec<Cache> {
    let [_, _, l1d, l1i] = leaf(0x8000_0005, 0);
    let [_, _, l2, l3] = leaf(0x8000_0006, 0);
    let kib = |k: u32| u64::from(k) * 1024;
    let mut caches = Vec::new();
    for (name, bytes, shared_by) in [
        ("L1d:", kib(l1d >> 24), threads),
        ("L1i:", kib(l1i >> 24), threads),
        ("L2:", kib(l2 >> 16), threads),
        ("L3:", kib(l3 >> 18) * 512, package),
    ] {
        if bytes > 0 {
            caches.push(Cache { name, bytes, shared_by });
        }
    }
    caches
}

/// `bytes` as lscpu writes a cache's size: in the largest unit of 1024
/// that leaves at least 1, whole, or with one decimal.
fn size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut scale = 1u64;
    while unit + 1 < units.len() && bytes >= scale * 1024 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = (bytes * 10 + scale / 2) / scale;
    if tenths.is_multiple_of(10) {
        format!("{} {}", tenths / 10, units[unit])
    } else {
        format!("{}.{} {}", tenths / 10, tenths % 10, units[unit])
    }
}

/// `0-3`, or `0` for one CPU.
fn cpu_list(cpus: u32) -> String {
    if cpus <= 1 { "0".to_string() } else { format!("0-{}", cpus - 1) }
}

/// The lines lscpu prints for `p` on a machine of `cpus` CPUs.
fn report(p: &Processor, cpus: u32) -> Vec<(usize, &'static str, String)> {
    let mut rows = vec![
        (0, "Architecture:", "x86_64".to_string()),
        (1, "CPU op-mode(s):", "32-bit, 64-bit".to_string()),
    ];
    if let Some((physical, virtual_)) = p.address_sizes {
        let text = format!("{physical} bits physical, {virtual_} bits virtual");
        rows.push((1, "Address sizes:", text));
    }
    rows.push((1, "Byte Order:", "Little Endian".to_string()));
    rows.push((0, "CPU(s):", cpus.to_string()));
    rows.push((1, "On-line CPU(s) list:", cpu_list(cpus)));
    rows.push((0, "Vendor ID:", p.vendor.clone()));
    rows.push((1, "Model name:", p.model_name.clone().unwrap_or_else(|| "-".to_string())));
    let cores = (p.per_package / p.threads_per_core.max(1)).max(1);
    let sockets = cpus.div_ceil(p.per_package.max(1)).max(1);
    rows.extend([
        (2, "CPU family:", p.family.to_string()),
        (2, "Model:", p.model.to_string()),
        (2, "Thread(s) per core:", p.threads_per_core.max(1).to_string()),
        (2, "Core(s) per socket:", cores.to_string()),
        (2, "Socket(s):", sockets.to_string()),
        (2, "Stepping:", p.stepping.to_string()),
        (2, "Flags:", p.flags.join(" ")),
    ]);
    let virtualization = if p.flags.contains(&"vmx") {
        Some("VT-x")
    } else if p.flags.contains(&"svm") {
        Some("AMD-V")
    } else {
        None
    };
    if virtualization.is_some() || p.hypervisor.is_some() {
        rows.push((0, "Virtualization features:", String::new()));
        if let Some(kind) = virtualization {
            rows.push((1, "Virtualization:", kind.to_string()));
        }
        if let Some(name) = &p.hypervisor {
            rows.push((1, "Hypervisor vendor:", name.clone()));
            rows.push((1, "Virtualization type:", "full".to_string()));
        }
    }
    if !p.caches.is_empty() {
        rows.push((0, "Caches (sum of all):", String::new()));
        for cache in &p.caches {
            let instances = u64::from(cpus.div_ceil(cache.shared_by.max(1)).max(1));
            let plural = if instances == 1 { "instance" } else { "instances" };
            let text = format!("{} ({instances} {plural})", size(cache.bytes * instances));
            rows.push((1, cache.name, text));
        }
    }
    rows
}

/// The CPUs the kernel runs.
fn cpus() -> Result<u32, Fail> {
    let text = std::fs::read_to_string("/kernel/cpus")
        .map_err(|error| fail(format!("/kernel/cpus: {}", crate::util::strip_os_error(&error))))?;
    text.trim().parse().map_err(|_| fail(format!("/kernel/cpus: not a number: {}", text.trim())))
}

/// lscpu(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    let processor = Processor::read(&cpuid);
    let mut out = io::stdout().lock();
    for (depth, name, value) in report(&processor, cpus()?) {
        let label = format!("{:indent$}{name}", "", indent = depth * 2);
        if value.is_empty() {
            writeln!(out, "{label}")?;
        } else {
            writeln!(out, "{label:<26}{value}")?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registers(text: &str) -> [u32; 3] {
        let bytes = text.as_bytes();
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        [word(0), word(1), word(2)]
    }

    /// The brand string's 48 bytes, as leaves 0x80000002 to 4 give them.
    fn brand(text: &str, leaf: u32) -> [u32; 4] {
        let mut bytes = text.as_bytes().to_vec();
        bytes.resize(48, 0);
        let at = (leaf - 0x8000_0002) as usize * 16;
        let word =
            |i: usize| u32::from_le_bytes(bytes[at + i * 4..at + i * 4 + 4].try_into().unwrap());
        [word(0), word(1), word(2), word(3)]
    }

    /// The Dell's Core i7-8550U: 4 cores of 2 threads, as Linux shows it.
    fn kaby_lake(leaf: u32, sub: u32) -> [u32; 4] {
        let [b, d, c] = registers("GenuineIntel");
        match (leaf, sub) {
            (0, _) => [0x16, b, c, d],
            // Family 6, model 0x8e, stepping 10; 16 logical, HTT.
            (1, _) => [0x0008_06ea, 0x0010_0800, 0x7ffa_fbbf, 0xbfeb_fbff],
            (4, 0) => [0x1c00_4121, 0x01c0_003f, 0x3f, 0],
            (4, 1) => [0x1c00_4122, 0x01c0_003f, 0x3f, 0],
            (4, 2) => [0x1c00_4143, 0x00c0_003f, 0x3ff, 0],
            (4, 3) => [0x1c03_c163, 0x03c0_003f, 0x1fff, 6],
            (4, _) => [0; 4],
            (7, 0) => [0, 0x029c_67af, 0, 0x9c00_0400],
            (0xb, 0) => [1, 2, 0x100, 0],
            (0xb, 1) => [4, 8, 0x201, 0],
            (0x8000_0000, _) => [0x8000_0008, 0, 0, 0],
            (0x8000_0001, _) => [0, 0, 0x121, 0x2c10_0800],
            (l @ 0x8000_0002..=0x8000_0004, _) => {
                brand("Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz", l)
            }
            (0x8000_0008, _) => [0x3027, 0, 0, 0],
            _ => [0; 4],
        }
    }

    #[test]
    fn reads_a_laptop() {
        let p = Processor::read(&kaby_lake);
        assert_eq!(p.vendor, "GenuineIntel");
        assert_eq!(p.model_name.as_deref(), Some("Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz"));
        assert_eq!((p.family, p.model, p.stepping), (6, 142, 10));
        assert_eq!((p.threads_per_core, p.per_package), (2, 8));
        assert_eq!(p.address_sizes, Some((39, 48)));
        assert_eq!(p.hypervisor, None);
        for flag in ["fpu", "sse2", "ht", "pni", "vmx", "avx2", "smep", "lm", "nx", "md_clear"] {
            assert!(p.flags.contains(&flag), "no {flag} in {:?}", p.flags);
        }
        assert!(!p.flags.contains(&"hypervisor") && !p.flags.contains(&"svm"));
        // Linux's order: leaf 1's EDX, then the extended EDX, then leaf 1's ECX.
        let at = |flag| p.flags.iter().position(|f| *f == flag).unwrap();
        assert!(at("pbe") < at("syscall") && at("lm") < at("pni"));
        let report = report(&p, 8);
        let row = |name: &str| {
            report.iter().find(|(_, n, _)| *n == name).map(|(_, _, v)| v.clone()).unwrap()
        };
        assert_eq!(row("On-line CPU(s) list:"), "0-7");
        assert_eq!(row("Core(s) per socket:"), "4");
        assert_eq!(row("Socket(s):"), "1");
        assert_eq!(row("Virtualization:"), "VT-x");
        assert_eq!(row("L1d:"), "128 KiB (4 instances)");
        assert_eq!(row("L2:"), "1 MiB (4 instances)");
        assert_eq!(row("L3:"), "8 MiB (1 instance)");
        assert!(report.iter().all(|(_, n, _)| *n != "Hypervisor vendor:"));
    }

    /// QEMU's default CPU under KVM on an AMD host: no brand string
    /// beyond QEMU's, leaf 0xb, AMD's cache leaves without topoext.
    fn qemu_amd(leaf: u32, sub: u32) -> [u32; 4] {
        let [b, d, c] = registers("AuthenticAMD");
        match (leaf, sub) {
            (0, _) => [0xd, b, c, d],
            (1, _) => [0x0006_0fb1, 0x0002_0800, 0x80b8_2203, 0x078b_fbfd],
            (0xb, _) => [0; 4],
            (0x4000_0000, _) => {
                let [b, c, d] = registers("KVMKVMKVM\0\0\0");
                [0x4000_0001, b, c, d]
            }
            (0x8000_0000, _) => [0x8000_000a, 0, 0, 0],
            (0x8000_0001, _) => [0, 0, 0x0000_0065, 0x2193_fbfd],
            (l @ 0x8000_0002..=0x8000_0004, _) => brand("QEMU Virtual CPU version 2.5+", l),
            (0x8000_0005, _) => [0, 0, 0x4002_0140, 0x4002_0140],
            (0x8000_0006, _) => [0, 0, 0x0200_4140, 0x0080_8140],
            (0x8000_0008, _) => [0x3028, 0, 1, 0],
            _ => [0; 4],
        }
    }

    #[test]
    fn reads_a_virtual_machine() {
        let p = Processor::read(&qemu_amd);
        assert_eq!(p.vendor, "AuthenticAMD");
        assert_eq!((p.family, p.model, p.stepping), (15, 107, 1));
        assert_eq!(p.hypervisor.as_deref(), Some("KVM"));
        assert_eq!((p.threads_per_core, p.per_package), (1, 2));
        let report = report(&p, 2);
        let row = |name: &str| {
            report.iter().find(|(_, n, _)| *n == name).map(|(_, _, v)| v.clone()).unwrap()
        };
        assert_eq!(row("Hypervisor vendor:"), "KVM");
        assert_eq!(row("Virtualization type:"), "full");
        assert_eq!(row("Model name:"), "QEMU Virtual CPU version 2.5+");
        assert_eq!(row("Core(s) per socket:"), "2");
        assert_eq!(row("L1d:"), "128 KiB (2 instances)");
        assert_eq!(row("L3:"), "16 MiB (1 instance)");
        assert!(p.flags.contains(&"hypervisor") && p.flags.contains(&"lahf_lm"));
    }

    #[test]
    fn sizes_and_lists() {
        assert_eq!(size(32 << 10), "32 KiB");
        assert_eq!(size(3 << 19), "1.5 MiB");
        assert_eq!(size(512), "512 B");
        assert_eq!(cpu_list(1), "0");
        assert_eq!(cpu_list(16), "0-15");
    }
}
