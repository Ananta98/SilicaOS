// SPDX-License-Identifier: GPL-2.0

//! Resolving page faults in user space.
//!
//! A fault arrives with an address and the access that caused it. The handler
//! finds the mapping that covers the address, checks that the access is allowed,
//! and then either admits that another context already installed the page or
//! installs it: from the mapping's backing object, or as a fresh zeroed page for
//! private anonymous memory.
//!
//! One address needs no mapping: a fault just below a `MAP_GROWSDOWN` mapping is
//! what a stack running out of room looks like, and the mapping is extended to
//! cover it. See [`Vmar::handle_page_fault`].

use ostd::mm::{
    PAGE_SIZE, PageFlags, UFrame, Vaddr, VmSpace, io::util::HasVmReaderWriter, tlb::TlbFlushOp,
    vm_space::VmQueriedItem,
};
use ostd::task::disable_preempt;

use crate::{
    errno::Result,
    vm::{backing::alloc_zeroed_frame, perms::VmPerms},
};

use super::{Vmar, mappings::VmMapping};

/// A page fault in user space.
#[derive(Clone, Copy, Debug)]
pub struct PageFaultInfo {
    /// The address that faulted.
    address: Vaddr,
    /// The permission the faulting access needed.
    required: VmPerms,
    /// Whether the fault was provoked rather than caused by an access.
    ///
    /// A provoked fault, such as one raised to read another process's memory on
    /// its behalf, is checked against the mapping's ceiling instead of its
    /// current permissions, so that a `PROT_EXEC` page can still be read.
    provoked: bool,
}

impl PageFaultInfo {
    /// Creates the information for a fault caused by an access.
    pub fn new(address: Vaddr, required: VmPerms) -> Self {
        Self {
            address,
            required,
            provoked: false,
        }
    }

    /// Marks this fault as provoked rather than caused by an access.
    pub fn provoked(mut self) -> Self {
        self.provoked = true;
        self
    }

    /// Returns the address that faulted.
    pub fn address(&self) -> Vaddr {
        self.address
    }

    /// Returns the permission the faulting access needed.
    pub fn required_perms(&self) -> VmPerms {
        self.required
    }

    /// Returns whether the fault was provoked.
    pub fn is_provoked(&self) -> bool {
        self.provoked
    }

    /// Builds the fault information for a CPU page-fault error code, if it
    /// describes a fault the kernel should try to resolve.
    ///
    /// Returns `None` for a fault the kernel must not touch — one the CPU
    /// reports without the user bit, which came from kernel space, or one that is
    /// not a page fault at all.
    ///
    /// The decoding is pulled out of [`Self::from_cpu_exception`] and takes the
    /// error code directly, because OSTD does not let a dependent crate build a
    /// `CpuException`, and the bit decoding is the part with the decisions in it.
    #[cfg(target_arch = "x86_64")]
    pub fn from_fault(
        addr: Vaddr,
        error_code: ostd::arch::cpu::context::PageFaultErrorCode,
    ) -> Option<Self> {
        use ostd::arch::cpu::context::PageFaultErrorCode;

        // Without the user bit the fault was taken in kernel mode against a user
        // address, which is OSTD's `read_fallible`/`write_fallible` path, not a
        // process's own access.
        if !error_code.contains(PageFaultErrorCode::USER) {
            return None;
        }
        // A write implies a read as far as the hardware is concerned, so a write
        // is reported as needing write permission rather than both. Asking for
        // both would wrongly reject a write to a page that is readable.
        let required = if error_code.contains(PageFaultErrorCode::WRITE) {
            VmPerms::WRITE
        } else if error_code.contains(PageFaultErrorCode::INSTRUCTION) {
            VmPerms::EXEC
        } else {
            VmPerms::READ
        };
        Some(Self::new(addr, required))
    }

    /// Builds the fault information for a CPU exception, if it is a page fault
    /// in user mode.
    #[cfg(target_arch = "x86_64")]
    pub fn from_cpu_exception(exception: &ostd::arch::cpu::context::CpuException) -> Option<Self> {
        use ostd::arch::cpu::context::CpuException;

        let CpuException::PageFault(info) = exception else {
            return None;
        };
        Self::from_fault(info.addr, info.error_code)
    }
}

impl Vmar {
    /// Resolves a page fault, if this address space can.
    ///
    /// Returns [`crate::errno::Errno::EACCES`] when no mapping covers the
    /// address, which the caller reports to the faulting process as a fault
    /// signal.
    ///
    /// # A fault below the mappings
    ///
    /// An address that no mapping covers is nearly always a genuine error, and
    /// that is what the [`EACCES`](crate::errno::Errno::EACCES) below reports.
    /// The exception is a fault on the page just below a stack that has run out
    /// of room: a mapping created with `MAP_GROWSDOWN` is extended to cover it,
    /// which is what lets an ordinary function call push a frame without a
    /// system call to resize anything.
    ///
    /// Growth is bounded twice over. Only private anonymous mappings take part,
    /// and only a fault within [`STACK_GUARD_GAP`](super::STACK_GUARD_GAP) of
    /// the mapping's start is allowed to extend it, so a wild pointer below the
    /// stack faults instead of growing the mapping without limit.
    pub fn handle_page_fault(&self, info: &PageFaultInfo) -> Result<()> {
        // The usual case, under the cheaper lock.
        {
            let inner = self.inner.read();
            if let Some(mapping) = inner.mappings.get(info.address) {
                return handle_mapping_fault(mapping, &self.vm_space, info);
            }
        }

        let Some(mapping) = self.grow_for_fault(info.address) else {
            crate::return_errno!(
                EACCES,
                "no mapping contains the faulting address {:#x}",
                info.address
            );
        };
        handle_mapping_fault(&mapping, &self.vm_space, info)
    }
}

/// Resolves a page fault on a known mapping.
///
/// The mapping is passed in rather than looked up so that callers which already
/// hold a record, such as prefaulting, do not have to take the address space
/// lock again.
pub(super) fn handle_mapping_fault(
    mapping: &VmMapping,
    vm_space: &VmSpace,
    info: &PageFaultInfo,
) -> Result<()> {
    // A provoked fault may go as far as the mapping's ceiling allows; a fault
    // caused by an access may not go past what is in force.
    let allowed = if info.provoked {
        mapping.perms().may()
    } else {
        mapping.perms().granted()
    };
    if !allowed.contains(info.required) {
        crate::return_errno!(
            EACCES,
            "{:?} at {:#x} is not permitted by {:?}",
            info.required,
            info.address,
            mapping.perms()
        );
    }

    let page = info.address & !(PAGE_SIZE - 1);
    let range = page..page + PAGE_SIZE;
    let is_write = info.required.contains(VmPerms::WRITE);

    let guard = disable_preempt();
    let mut cursor = vm_space
        .cursor_mut(&guard, &range)
        .expect("a mapped address always lies inside the VM space");

    let (_, item) = cursor.query().expect("the range is page-aligned");
    match item {
        // The page is already installed. Either another context resolved this
        // fault a moment ago, or the access needed more permission than the page
        // table entry carries.
        Some(VmQueriedItem::MappedRam { frame, mut prop }) => {
            if VmPerms::from(prop.flags).contains(info.required) {
                // Someone else got here first; make sure this CPU sees the page.
                TlbFlushOp::for_range(range.clone()).perform_on_current();
                return Ok(());
            }

            if mapping.is_cow() && frame.reference_count() > 1 {
                // The page is still shared, so this write needs a private copy.
                let copy = copy_frame(&frame)?;
                prop.flags |= PageFlags::W | PageFlags::ACCESSED | PageFlags::DIRTY;
                cursor.unmap(PAGE_SIZE);
                cursor.jump(page).expect("page-aligned");
                cursor.map(copy, prop);
            } else {
                // The page is ours alone, or is deliberately shared, so the
                // only thing missing is the write bit.
                prop.flags |= PageFlags::W | PageFlags::ACCESSED | PageFlags::DIRTY;
                cursor.protect_next(PAGE_SIZE, |flags, _| *flags = prop.flags);
                cursor
                    .flusher()
                    .issue_tlb_flush(TlbFlushOp::for_range(range.clone()));
                cursor.flusher().dispatch_tlb_flush();
            }
            cursor.flusher().sync_tlb_flush();
        }
        Some(VmQueriedItem::MappedIoMem { .. }) => {
            unreachable!("device mappings are not supported")
        }
        // The page is not installed yet.
        None => {
            let offset = page - mapping.start();
            let frame = obtain_frame(mapping, offset)?;
            // A page that other mappings also refer to has to be installed
            // read-only here, so that the first write to it copies instead of
            // writing through. A shared mapping never does this: writing
            // through is the whole point of it.
            let must_copy = mapping.is_cow() && frame.reference_count() > 1;
            let property = mapping.fault_property(!must_copy, is_write);
            cursor.map(frame, property);
        }
    }
    Ok(())
}

/// Returns the page that backs `offset` bytes into `mapping`.
///
/// The mapping's object supplies it when the offset is within the object;
/// otherwise the page is private and anonymous, which is what an oversized
/// mapping or `MAP_PRIVATE` memory gets.
fn obtain_frame(mapping: &VmMapping, offset: usize) -> Result<UFrame> {
    match mapping.backing() {
        Some(backing) if mapping.is_backed(offset) => {
            backing.frame(mapping.backing_offset_of(offset))
        }
        _ => alloc_zeroed_frame(),
    }
}

/// Returns a private copy of `frame`.
fn copy_frame(frame: &UFrame) -> Result<UFrame> {
    let copy = alloc_zeroed_frame()?;
    copy.writer().write(&mut frame.reader());
    Ok(copy)
}

#[cfg(ktest)]
mod tests {
    use ostd::mm::{PAGE_SIZE, Vaddr};
    use ostd::prelude::ktest;

    use super::*;
    use crate::vm::{
        flags::MmapFlags,
        perms::VmPerms,
        tests::{not_resident, paddr_at, record_at, touch},
    };
    use crate::vm::vmar::{STACK_GUARD_GAP, VMAR_LOWEST_ADDR};

    /// Somewhere high enough to be well clear of anything `mmap` hands out,
    /// and low enough to leave room for the stack to grow below itself.
    const STACK_TOP: Vaddr = 0x4000_0000;

    /// Maps a two-page private anonymous stack at `STACK_TOP`.
    fn growdown_stack(vmar: &Vmar, flags: MmapFlags) -> Vaddr {
        vmar.mmap_anonymous(
            STACK_TOP,
            2 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            flags,
        )
        .expect("the stack mapping is created at the requested address")
    }

    /// Takes a write fault at `addr`, which a real access would.
    fn fault(vmar: &Vmar, addr: Vaddr) -> Result<()> {
        vmar.handle_page_fault(&PageFaultInfo::new(addr, VmPerms::WRITE))
    }

    #[ktest]
    fn a_fault_below_a_growdown_mapping_extends_it() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);
        assert_eq!(start, STACK_TOP);
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);

        // The page just below the mapping, which is what a call pushing a frame
        // looks like.
        let below = start - PAGE_SIZE;
        fault(&vmar, below).expect("a stack with room grows downwards");

        let grown = record_at(&vmar, below);
        assert_eq!(grown.start(), below, "the record now reaches the faulting page");
        assert_eq!(grown.len(), 3 * PAGE_SIZE);
        assert_eq!(
            vmar.total_mapped_size(),
            3 * PAGE_SIZE,
            "the address space grew by the page that was added"
        );
    }

    #[ktest]
    fn growth_faults_the_page_in_and_it_reads_as_zero() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);

        let below = start - PAGE_SIZE;
        assert!(not_resident(&vmar, below), "the page is not in the page table yet");
        fault(&vmar, below).expect("the stack grows");
        assert!(!not_resident(&vmar, below), "the faulting page is now resident");

        // Growing only makes the page addressable; it must not invent contents.
        touch(&vmar, below..below + PAGE_SIZE).expect("the grown page faults in");
        assert_eq!(crate::vm::tests::peek(&vmar, below), 0);
    }

    #[ktest]
    fn growth_reaches_several_pages_down_at_once() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);

        // Well inside the guard gap, so this is a deep stack rather than a wild
        // pointer.
        let target = start - 16 * PAGE_SIZE;
        fault(&vmar, target).expect("the stack grows to reach the page");

        assert_eq!(record_at(&vmar, target).start(), target);
        assert_eq!(vmar.total_mapped_size(), 18 * PAGE_SIZE);
        // Only the page that faulted is resident: growth maps, it does not fill.
        assert!(not_resident(&vmar, target + 8 * PAGE_SIZE));
    }

    #[ktest]
    fn a_fault_past_the_guard_gap_is_refused() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);

        // One page beyond the guard gap. This is a wild pointer below the stack,
        // not a stack running out of room, and growing for it would let any bad
        // address quietly become a mapping.
        let wild = start - (STACK_GUARD_GAP + PAGE_SIZE);
        assert!(
            fault(&vmar, wild).is_err(),
            "a fault beyond the guard gap must not grow the mapping"
        );
        assert_eq!(
            vmar.total_mapped_size(),
            2 * PAGE_SIZE,
            "the mapping is left exactly as it was"
        );
    }

    #[ktest]
    fn a_fault_at_the_guard_gap_edge_still_grows() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);

        // The last address growth is allowed to reach.
        let edge = start - STACK_GUARD_GAP;
        fault(&vmar, edge).expect("the guard gap is an inclusive bound");
        assert_eq!(record_at(&vmar, edge).start(), edge);
    }

    #[ktest]
    fn growth_stops_at_the_bottom_of_the_address_space() {
        let vmar = Vmar::new();
        // A stack sitting just above the floor, so the floor itself is the
        // furthest down it can ever grow.
        let start = vmar
            .mmap_anonymous(
                VMAR_LOWEST_ADDR + 2 * PAGE_SIZE,
                PAGE_SIZE,
                VmPerms::READ | VmPerms::WRITE,
                MmapFlags::PRIVATE | MmapFlags::GROWSDOWN,
            )
            .expect("the stack mapping is created");

        let at_floor = VMAR_LOWEST_ADDR;
        fault(&vmar, at_floor).expect("the floor is still user space");
        assert_eq!(record_at(&vmar, at_floor).start(), at_floor);
        assert_eq!(start, VMAR_LOWEST_ADDR + 2 * PAGE_SIZE);
    }

    #[ktest]
    fn a_mapping_without_grows_down_does_not_grow() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE);

        let below = start - PAGE_SIZE;
        assert!(
            fault(&vmar, below).is_err(),
            "an ordinary mapping does not extend itself"
        );
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn a_shared_growdown_mapping_does_not_grow() {
        let vmar = Vmar::new();
        // A file-backed mapping may carry the flag on Linux and still never grow,
        // because dropping the mapping would lose the file's contents. What stands
        // in for a file here is a shared object: its pages are owned by the
        // object, so the mapping is not this address space's to extend either.
        let start = growdown_stack(&vmar, MmapFlags::SHARED | MmapFlags::GROWSDOWN);

        let below = start - PAGE_SIZE;
        assert!(
            fault(&vmar, below).is_err(),
            "a shared mapping does not extend itself"
        );
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn growth_respects_the_address_space_limit() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);
        // Exactly enough for the mapping and nothing more, so any growth at all
        // exceeds the limit.
        vmar.set_max_addr_space(Some(2 * PAGE_SIZE));

        let below = start - PAGE_SIZE;
        assert!(
            fault(&vmar, below).is_err(),
            "growth is subject to the address space limit"
        );
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn a_grown_page_is_its_own() {
        let vmar = Vmar::new();
        let start = growdown_stack(&vmar, MmapFlags::PRIVATE | MmapFlags::GROWSDOWN);

        // Write to the original stack, then grow and check the new page did not
        // inherit it: growth extends a mapping, it does not copy anything.
        let on_stack = start + PAGE_SIZE;
        crate::vm::tests::poke(&vmar, on_stack, 0x5eed_5eed);
        let below = start - PAGE_SIZE;
        fault(&vmar, below).expect("the stack grows");

        assert_ne!(
            paddr_at(&vmar, below),
            paddr_at(&vmar, on_stack),
            "the new page is a fresh frame"
        );
        assert_eq!(crate::vm::tests::peek(&vmar, on_stack), 0x5eed_5eed);
    }
}
