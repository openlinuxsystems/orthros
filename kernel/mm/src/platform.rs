//! What the memory manager needs from the machine.

/// Most CPUs the memory manager keeps per-CPU caches for. Equal to
/// `radix_hal::MAX_CPUS`, which checks it.
pub const MAX_CPUS: usize = 256;

/// The machine, as the memory manager sees it. The architecture implements
/// it and registers it with [`crate::Frames::set_platform`] and
/// [`crate::Heap::set_platform`] before the first allocation.
///
/// # Safety
///
/// The heap builds pointers from [`Platform::direct_map`], so implementors
/// guarantee that `direct_map() + phys` is a readable and writable mapping
/// of every frame the frame allocator manages, for the kernel's whole life,
/// and that [`Platform::current_cpu`] is below [`MAX_CPUS`] and does not
/// change while interrupts are disabled.
#[allow(unsafe_code)]
pub unsafe trait Platform: Sync {
    /// Index of the CPU running the caller.
    fn current_cpu(&self) -> usize;

    /// NUMA node of `cpu` (0 when the machine has one node).
    fn cpu_node(&self, cpu: usize) -> usize;

    /// Disables interrupts on this CPU and returns whether they were on.
    fn disable_interrupts(&self) -> bool;

    /// Enables interrupts again if `were_enabled`.
    fn restore_interrupts(&self, were_enabled: bool);

    /// Virtual address of physical address 0 in the direct map.
    fn direct_map(&self) -> usize;
}

/// Interrupts disabled until dropped (no-op without a platform, which
/// only happens before the machine is set up, with interrupts off).
pub(crate) struct NoInterrupts<'a> {
    platform: Option<&'a dyn Platform>,
    were_enabled: bool,
}

impl<'a> NoInterrupts<'a> {
    pub(crate) fn new(platform: Option<&'a dyn Platform>) -> NoInterrupts<'a> {
        let were_enabled = platform.is_some_and(|p| p.disable_interrupts());
        NoInterrupts { platform, were_enabled }
    }
}

impl Drop for NoInterrupts<'_> {
    fn drop(&mut self) {
        if let Some(platform) = self.platform {
            platform.restore_interrupts(self.were_enabled);
        }
    }
}
