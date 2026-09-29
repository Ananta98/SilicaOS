// SPDX-License-Identifier: GPL-2.0

//! Resolving page faults in user space.
//!
//! A fault arrives with an address and the access that caused it. The handler
//! finds the mapping that covers the address, checks that the access is allowed,
//! and then either admits that another context already installed the page or
//! installs it: from the mapping's backing object, or as a fresh zeroed page for
//! private anonymous memory.

use ostd::mm::{
    PAGE_SIZE, PageFlags, UFrame, Vaddr, VmSpace, io::util::HasVmReaderWriter, tlb::TlbFlushOp,
    vm_space::VmQueriedItem,
};
use ostd::task::disable_preempt;

use crate::{
    errno::Result,
    vm::{
        backing::alloc_zeroed_frame,
        perms::VmPerms,
    },
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
    pub fn handle_page_fault(&self, info: &PageFaultInfo) -> Result<()> {
        let inner = self.inner.read();
        let Some(mapping) = inner.mappings.get(info.address) else {
            crate::return_errno!(
                EACCES,
                "no mapping contains the faulting address {:#x}",
                info.address
            );
        };
        handle_mapping_fault(mapping, &self.vm_space, info)
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
    use alloc::sync::Arc;

    use ostd::mm::{PAGE_SIZE, PageFlags, Vaddr};
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{
            Vmar,
            backing::Backing,
            flags::MmapFlags,
            shm::SharedPages,
            tests::{not_resident, paddr_at, page_at, peek, poke},
        },
    };

    const AT: Vaddr = 0x4000_0000;

    fn anonymous(perms: VmPerms, pages: usize) -> Arc<Vmar> {
        let vmar = Vmar::new();
        vmar.mmap_anonymous(
            AT,
            pages * PAGE_SIZE,
            perms,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        vmar
    }

    /// A private mapping of one shared page, which is what a `fork` child sees
    /// before it writes.
    fn private_over_shared(object: &Arc<SharedPages>) -> Arc<Vmar> {
        let vmar = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(object) as Arc<dyn Backing>;
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        vmar
    }

    #[ktest]
    fn an_unmapped_address_is_not_mapped() {
        let vmar = anonymous(VmPerms::READ, 1);
        assert!(not_resident(&vmar, AT));
        assert!(page_at(&vmar, AT).is_none());
    }

    #[ktest]
    fn a_first_fault_allocates_a_zeroed_page() {
        let vmar = anonymous(VmPerms::READ, 1);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ))
            .unwrap();
        assert!(!not_resident(&vmar, AT));
        assert_eq!(peek(&vmar, AT), 0);
    }

    #[ktest]
    fn a_fresh_private_page_is_writable_at_once() {
        // A page nobody else refers to needs no copy-on-write, so the fault
        // grants write permission immediately rather than costing a second
        // fault.
        let vmar = anonymous(VmPerms::READ | VmPerms::WRITE, 1);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ | VmPerms::WRITE))
            .unwrap();
        let (_, flags) = page_at(&vmar, AT).unwrap();
        assert!(flags.contains(PageFlags::W), "{flags:?} should be writable");
        assert!(flags.contains(PageFlags::R));
    }

    #[ktest]
    fn a_private_page_over_a_shared_object_starts_read_only() {
        // The frame belongs to the object, so the mapping must not be able to
        // write through it; the write is what triggers the copy.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        poke_private_source(&object);
        let vmar = private_over_shared(&object);

        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ))
            .unwrap();
        let (_, flags) = page_at(&vmar, AT).unwrap();
        assert!(!flags.contains(PageFlags::W), "{flags:?} must not be writable yet");

        // Writing it now copies, and leaves the object's page alone.
        let before = paddr_at(&vmar, AT);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap();
        let after = paddr_at(&vmar, AT);
        assert_ne!(before, after, "the write must have copied the page");
        assert!(page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));
    }

    /// Creates the shared page that `private_over_shared` copies from, by having
    /// a second mapping fault it in first.
    fn poke_private_source(object: &Arc<SharedPages>) {
        let other = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(object) as Arc<dyn Backing>;
        other.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        poke(&other, AT, 0x5a5a);
    }

    #[ktest]
    fn a_write_to_an_exclusive_page_is_granted_in_place() {
        // `mprotect` never sets the write bit itself, so widening a mapping back
        // leaves the page read-only even though writes are permitted. The fault
        // is what grants it, and because nobody else refers to the frame, no copy
        // is needed.
        let rw = VmPerms::READ | VmPerms::WRITE;
        let vmar = anonymous(rw, 1);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, rw)).unwrap();
        let before = paddr_at(&vmar, AT);
        assert!(page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));

        vmar.mprotect(VmPerms::READ, AT..AT + PAGE_SIZE).unwrap();
        assert!(!page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));

        // Widening again restores the permission but not the write bit, by
        // design: only the fault handler may set that.
        vmar.mprotect(rw, AT..AT + PAGE_SIZE).unwrap();
        assert!(!page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));

        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap();
        assert_eq!(paddr_at(&vmar, AT), before, "no copy should be needed");
        assert!(page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));
    }

    #[ktest]
    fn a_shared_page_is_never_copied() {
        // The whole point of a shared mapping: a write reaches the object.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        let vmar = Vmar::new();
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        poke(&vmar, AT, 0x1111);
        let before = paddr_at(&vmar, AT);
        assert!(page_at(&vmar, AT).unwrap().1.contains(PageFlags::W));

        // Forcing a write fault onto an already-writable page is a no-op that
        // must not copy.
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap();
        assert_eq!(paddr_at(&vmar, AT), before);
        assert_eq!(object.peek_u64(0).unwrap(), 0x1111);
    }

    #[ktest]
    fn a_copy_preserves_the_rest_of_the_page() {
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        poke_private_source(&object);
        let vmar = private_over_shared(&object);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ))
            .unwrap();
        // Put a marker in the private copy, then force the copy-on-write.
        poke(&vmar, AT + 8, 0x9999);
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap();
        assert_eq!(peek(&vmar, AT + 8), 0x9999);
    }

    #[ktest]
    fn a_fault_on_an_unmapped_address_is_denied() {
        let vmar = anonymous(VmPerms::READ, 1);
        let err = vmar
            .handle_page_fault(&PageFaultInfo::new(AT + 64 * PAGE_SIZE, VmPerms::READ))
            .unwrap_err();
        assert_eq!(err, Errno::EACCES);
    }

    #[ktest]
    fn a_write_beyond_the_permissions_is_denied() {
        let vmar = anonymous(VmPerms::READ, 1);
        let err = vmar
            .handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap_err();
        assert_eq!(err, Errno::EACCES);
        assert!(not_resident(&vmar, AT));
    }

    #[ktest]
    fn an_execution_fault_needs_execute_permission() {
        let vmar = anonymous(VmPerms::READ, 1);
        let err = vmar
            .handle_page_fault(&PageFaultInfo::new(AT, VmPerms::EXEC))
            .unwrap_err();
        assert_eq!(err, Errno::EACCES);

        let exec = anonymous(VmPerms::READ | VmPerms::EXEC, 1);
        exec.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::EXEC))
            .unwrap();
        assert!(page_at(&exec, AT).unwrap().1.contains(PageFlags::X));
    }

    #[ktest]
    fn a_provoked_fault_is_bounded_by_the_ceiling() {
        // Reading an executable page on another process's behalf is what a
        // provoked fault is for: it is checked against what `mmap` allowed, not
        // against what is in force now.
        let vmar = Vmar::new();
        vmar.mmap_anonymous(
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::EXEC,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        vmar.mprotect(VmPerms::EXEC, AT..AT + PAGE_SIZE).unwrap();
        assert_eq!(
            vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ))
                .unwrap_err(),
            Errno::EACCES
        );
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ).provoked())
            .unwrap();
        assert!(!not_resident(&vmar, AT));
    }

    #[ktest]
    fn two_offsets_of_one_object_are_different_pages() {
        let object = SharedPages::new(2 * PAGE_SIZE).unwrap();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        let vmar = Vmar::new();
        vmar.mmap_backed(
            &backing,
            AT,
            2 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        poke(&vmar, AT, 0x1111);
        poke(&vmar, AT + PAGE_SIZE, 0x2222);
        assert_ne!(paddr_at(&vmar, AT), paddr_at(&vmar, AT + PAGE_SIZE));
        assert_eq!(object.peek_u64(0).unwrap(), 0x1111);
        assert_eq!(object.peek_u64(PAGE_SIZE).unwrap(), 0x2222);
    }

    #[ktest]
    fn a_write_through_a_shared_mapping_reaches_the_object() {
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        let vmar = Vmar::new();
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::READ))
            .unwrap();
        vmar.handle_page_fault(&PageFaultInfo::new(AT, VmPerms::WRITE))
            .unwrap();
        poke(&vmar, AT, 0x7777);
        assert_eq!(object.peek_u64(0).unwrap(), 0x7777);
    }
}

/// Tests for turning a CPU page-fault error code into a [`PageFaultInfo`].
///
/// OSTD does not expose a constructor for `CpuException`, so this cannot be
/// tested through the real trap. Everything below is the decoding, which is the
/// part with the decisions in it.
#[cfg(all(ktest, target_arch = "x86_64"))]
mod decoding {
    use ostd::arch::cpu::context::PageFaultErrorCode;
    use ostd::prelude::ktest;

    use super::*;
    use crate::vm::perms::VmPerms;

    const ADDR: Vaddr = 0x1234_5000;

    /// Builds a fault from the given error-code bits.
    fn fault(bits: u32) -> Option<PageFaultInfo> {
        let code = PageFaultErrorCode::from_bits(bits as usize)
            .unwrap_or_else(|| panic!("{bits:#x} is not a known error code"));
        PageFaultInfo::from_fault(ADDR, code)
    }

    #[ktest]
    fn a_kernel_space_fault_is_not_ours() {
        // No user bit: the CPU was in kernel mode. This is the case that must
        // never be resolved by a process's address space.
        assert!(fault(PageFaultErrorCode::WRITE.bits() as u32).is_none());
        assert!(fault(0).is_none());
        // The user bit alone is enough to make it ours.
        assert!(fault(PageFaultErrorCode::USER.bits() as u32).is_some());
    }

    #[ktest]
    fn a_write_needs_write_permission_and_nothing_more() {
        let info = fault(
            (PageFaultErrorCode::USER | PageFaultErrorCode::WRITE).bits() as u32,
        )
        .unwrap();
        // A write implies read on x86, so requiring both would wrongly reject a
        // write to a page that is merely readable.
        assert_eq!(info.required_perms(), VmPerms::WRITE);
        assert_eq!(info.address(), ADDR);
    }

    #[ktest]
    fn a_read_needs_read_permission() {
        let info = fault(PageFaultErrorCode::USER.bits() as u32).unwrap();
        assert_eq!(info.required_perms(), VmPerms::READ);
    }

    #[ktest]
    fn an_instruction_fetch_needs_execute_permission() {
        let code = PageFaultErrorCode::USER | PageFaultErrorCode::INSTRUCTION;
        let info = fault(code.bits() as u32).unwrap();
        assert_eq!(info.required_perms(), VmPerms::EXEC);
    }

    #[ktest]
    fn a_write_wins_over_an_instruction_fetch() {
        // A write that also carries the instruction bit is still a write; the
        // order of the tests decides which permission is required, and a write is
        // the more restrictive of the two.
        let code = PageFaultErrorCode::USER
            | PageFaultErrorCode::WRITE
            | PageFaultErrorCode::INSTRUCTION;
        assert_eq!(fault(code.bits() as u32).unwrap().required_perms(), VmPerms::WRITE);
    }

    #[ktest]
    fn the_present_and_protection_bits_do_not_matter() {
        // A fault on a present page (a permission fault) and on an absent one (a
        // translation fault) are handled identically: the mapping decides.
        for extra in [
            PageFaultErrorCode::PRESENT,
            PageFaultErrorCode::PROTECTION,
            PageFaultErrorCode::RESERVED,
        ] {
            let code = PageFaultErrorCode::USER | extra;
            let info = fault(code.bits() as u32).unwrap();
            assert_eq!(info.required_perms(), VmPerms::READ);
        }
    }

    #[ktest]
    fn the_address_is_taken_verbatim() {
        // The in-page offset matters, so it must not be rounded away here; the
        // handler rounds it when it needs a whole page.
        let code = PageFaultErrorCode::from_bits(PageFaultErrorCode::USER.bits()).unwrap();
        for addr in [0x1000, 0x1234_5000, 0x3fff_ffff_f000] {
            let info = PageFaultInfo::from_fault(addr, code).unwrap();
            assert_eq!(info.address(), addr);
        }
    }

    #[ktest]
    fn a_fault_starts_unprovoked() {
        let code = PageFaultErrorCode::from_bits(PageFaultErrorCode::USER.bits()).unwrap();
        let info = PageFaultInfo::from_fault(ADDR, code).unwrap();
        assert!(!info.is_provoked());
        assert!(info.provoked().is_provoked());
        // Provoking is one-way.
        assert!(info.provoked().provoked().is_provoked());
    }
}
