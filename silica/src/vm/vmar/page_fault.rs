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
