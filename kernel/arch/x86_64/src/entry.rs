//! The boot CPU's path from the trampoline to the kernel's main function.
//!
//! The order matters. GS base comes first, because the kernel log asks
//! which CPU it runs on; then the serial console and exception handling, so
//! anything that goes wrong later is reported; then the memory map, early
//! microcode, the kernel's page tables and the screen. Only then does
//! `radix_kernel_main` run, with everything described in [`BootParams`].

use core::sync::atomic::Ordering;

use radix_bootinfo::{BootInfo, MULTIBOOT2_MAGIC, framebuffer_type};
use radix_hal::cmdline::CommandLine;
use radix_hal::{BootParams, Channel, Firmware, Framebuffer, PixelFormat};
use radix_sync::{Once, TakeOnce};

use crate::memory::{self, PAGE};
use crate::paging::{self, Access, Cache};
use crate::percpu::{self, IST_STACKS, Stacks};
use crate::serial::SerialSink;
use crate::vga::VgaSink;
use crate::{interrupts, kaslr, microcode, smp, time};

unsafe extern "Rust" {
    /// The kernel's main function, defined by the `radix` crate.
    fn radix_kernel_main(params: BootParams) -> !;
}

unsafe extern "C" {
    static __kernel_phys_start: u8;
    static __kernel_phys_end: u8;
    static bsp_stack_bottom: u8;
    static bsp_stack_top: u8;
}

/// Size of each of the boot CPU's interrupt stacks.
const BSP_IST_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct IstStacks([[u8; BSP_IST_SIZE]; IST_STACKS]);

static BSP_IST: TakeOnce<IstStacks> = TakeOnce::new(IstStacks([[0; BSP_IST_SIZE]; IST_STACKS]));

/// The RSDP copy from the boot information.
pub static RSDP_COPY: Once<&'static [u8]> = Once::new();

/// The kernel log's view of the machine.
struct KlogPlatform;

impl radix_klog::Platform for KlogPlatform {
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }

    fn cpu(&self) -> usize {
        percpu::current_index()
    }

    fn disable_interrupts(&self) -> bool {
        crate::cpu::disable_interrupts()
    }

    fn restore_interrupts(&self, were_enabled: bool) {
        if were_enabled {
            crate::cpu::enable_interrupts();
        }
    }
}

static KLOG_PLATFORM: KlogPlatform = KlogPlatform;
static SERIAL_SINK: SerialSink = SerialSink;
static VGA_SINK: VgaSink = VgaSink;

/// Sets up the boot CPU's descriptor tables and stacks.
fn init_boot_cpu() {
    let ist = BSP_IST.take().expect("the boot CPU starts once");
    let ranges = ist.0.each_ref().map(|stack| {
        let start = stack.as_ptr() as usize;
        start..start + stack.len()
    });
    let (bottom, top) = (&raw const bsp_stack_bottom as usize, &raw const bsp_stack_top as usize);
    let _ = percpu::current().stacks.set(Stacks {
        kernel: bottom..top,
        ist: ranges.clone(),
        guards: [paging::bsp_stack_guard(), 0..0, 0..0, 0..0],
    });
    percpu::load_descriptors(ranges.map(|range| range.end));
    interrupts::init();
    interrupts::load();
    crate::fpu::init_boot_cpu();
}

/// Turns the boot information address into a byte slice, through the
/// mapping of the first 4 GiB at `base`: 0 (the boot tables' identity map,
/// before the direct map has its place) or the direct map.
fn boot_info_bytes(info_phys: u32, base: u64) -> &'static [u8] {
    if info_phys == 0 || !info_phys.is_multiple_of(8) {
        panic!("boot information address {info_phys:#x} is invalid");
    }
    let virt = (base + u64::from(info_phys)) as usize;
    // SAFETY: GRUB passed this address in EBX, so it is below 4 GiB, where
    // the boot tables map it at 0 and in the direct map; the first field is
    // the structure's total size. The structure is reserved before the
    // frame allocator starts, so nothing overwrites it, and the direct map
    // keeps mapping it at the same address afterwards (the identity view
    // is used only before that, and not kept).
    let total = unsafe { (virt as *const u32).read() } as usize;
    // SAFETY: as above, for the whole structure.
    unsafe { core::slice::from_raw_parts(virt as *const u8, total) }
}

/// Maps the boot loader's framebuffer, if it is one this kernel can draw on.
fn framebuffer(info: &BootInfo<'_>) -> (Option<Framebuffer>, bool) {
    let Some(fb) = info.framebuffer() else { return (None, false) };
    match (fb.kind, fb.rgb) {
        (framebuffer_type::RGB, Some([red, green, blue])) if matches!(fb.bpp, 16 | 24 | 32) => {
            let len = u64::from(fb.pitch) * u64::from(fb.height);
            let virt =
                paging::map_physical(fb.address, len, Access::ReadWrite, Cache::WriteCombining);
            // SAFETY: `virt` maps the whole framebuffer (`pitch * height`
            // bytes) write-combining, the mapping is never removed, and this
            // is the only reference ever made to it.
            let memory = unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, len as usize) };
            let channel = |(shift, bits): (u8, u8)| Channel { shift, bits };
            let framebuffer = Framebuffer {
                memory,
                phys_addr: fb.address,
                width: fb.width as usize,
                height: fb.height as usize,
                pitch: fb.pitch as usize,
                format: PixelFormat {
                    red: channel(red),
                    green: channel(green),
                    blue: channel(blue),
                    bytes_per_pixel: fb.bpp / 8,
                },
            };
            (Some(framebuffer), false)
        }
        (framebuffer_type::EGA_TEXT, _) => {
            let virt = paging::map_physical(fb.address, PAGE, Access::ReadWrite, Cache::Uncached);
            crate::vga::init(virt);
            radix_klog::add_sink(&VGA_SINK, true);
            (None, true)
        }
        _ => {
            radix_klog::warn!(
                "Framebuffer: {}x{} with {} bits per pixel of type {} is not supported",
                fb.width,
                fb.height,
                fb.bpp,
                fb.kind
            );
            (None, false)
        }
    }
}

/// Called by the boot trampoline in long mode with the Multiboot2 magic,
/// the physical address of the boot information and the KASLR slide.
#[unsafe(no_mangle)]
extern "C" fn radix_arch_entry(magic: u32, info_phys: u32, slide: u64) -> ! {
    kaslr::set_slide(slide);
    percpu::set_current(0);
    time::record_start();
    radix_klog::set_platform(&KLOG_PLATFORM);
    if crate::serial::init() {
        radix_klog::add_sink(&SERIAL_SINK, true);
    }
    init_boot_cpu();

    if magic != MULTIBOOT2_MAGIC {
        panic!("not started by a Multiboot2 boot loader (magic {magic:#x})");
    }
    // A first look at the boot information, through the identity map:
    // the memory map decides where the direct map goes.
    let early = BootInfo::parse(boot_info_bytes(info_phys, 0))
        .unwrap_or_else(|err| panic!("bad boot information: {err}"));
    // Trace points record from here when asked to, before anything else.
    if CommandLine::new(early.command_line().unwrap_or("")).has("trace") {
        radix_klog::trace::set_enabled(true);
    }
    radix_klog::tracepoint!("boot.entry", magic, info_phys);
    let Some(map) = early.memory_map() else { panic!("the boot loader passed no memory map") };
    memory::set_memory_map(map.regions());
    memory::choose_layout(kaslr::status() != kaslr::Status::Disabled);

    // From here on the boot information is read through the direct map.
    let bytes = boot_info_bytes(info_phys, memory::direct_map());
    let info = BootInfo::parse(bytes).unwrap_or_else(|err| panic!("bad boot information: {err}"));
    let command_line = info.command_line().unwrap_or("");
    let flags = CommandLine::new(command_line);

    // Physical memory.
    let (image_start, image_end) =
        (&raw const __kernel_phys_start as u64, &raw const __kernel_phys_end as u64);
    memory::reserve("first MiB (firmware, AP start-up)", 0, 1 << 20);
    memory::reserve("kernel image", image_start, image_end);
    memory::reserve(
        "boot information",
        u64::from(info_phys),
        u64::from(info_phys) + bytes.len() as u64,
    );
    for module in info.modules() {
        let name = if module.command_line.split_ascii_whitespace().any(|w| w == "microcode") {
            "module: microcode"
        } else {
            "module"
        };
        memory::reserve(name, u64::from(module.start), u64::from(module.end));
    }
    if flags.value("crashlog") != Some("off")
        && let Some(start) = memory::choose_crash_log_region()
    {
        memory::reserve("crash log", start, start + memory::CRASH_LOG_SIZE);
        crate::set_crash_log_region(start);
    }
    if let Some(page) = memory::choose_trampoline_page() {
        smp::set_trampoline_page(page);
    }
    memory::init_allocator();
    radix_klog::tracepoint!("boot.memory", memory::memory_map().len());

    // Early microcode, as soon as the module can be found.
    if let Some(module) =
        info.modules().find(|m| m.command_line.split_ascii_whitespace().any(|w| w == "microcode"))
    {
        let virt = memory::phys_to_virt(u64::from(module.start));
        let len = module.end.saturating_sub(module.start) as usize;
        // SAFETY: GRUB loaded the module at these physical addresses, below
        // 4 GiB and in RAM, so the direct map (and its boot alias) reaches
        // it; it was reserved above, so nothing overwrites it.
        microcode::set_file(unsafe { core::slice::from_raw_parts(virt as *const u8, len) });
        microcode::load(true);
    }

    paging::build_and_switch(!flags.has("nopcid"));
    radix_klog::tracepoint!("boot.paging");

    if let Some(rsdp) = info.acpi_rsdp() {
        let _ = RSDP_COPY.set(rsdp);
    }
    let (framebuffer, text_screen) = framebuffer(&info);
    let params = BootParams {
        command_line,
        boot_loader: info.boot_loader_name().unwrap_or("unknown"),
        firmware: match info.firmware() {
            radix_bootinfo::Firmware::Uefi => Firmware::Uefi,
            radix_bootinfo::Firmware::Bios => Firmware::Bios,
        },
        memory_map: memory::memory_map(),
        reservations: memory::reservations(),
        framebuffer,
        text_screen,
    };
    percpu::current().online.store(true, Ordering::Relaxed);
    radix_klog::tracepoint!("boot.kernel-main");
    // SAFETY: the `radix` crate defines `radix_kernel_main` with exactly
    // this signature (see `radix_hal::KernelMain`); it never returns.
    unsafe { radix_kernel_main(params) }
}
