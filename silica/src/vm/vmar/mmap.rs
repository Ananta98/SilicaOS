// SPDX-License-Identifier: GPL-2.0

//! `mmap(2)`.

use alloc::sync::Arc;
use core::num::NonZeroUsize;

use ostd::mm::{PAGE_SIZE, Vaddr};

use crate::{
    errno::Result,
    vm::{backing::Backing, flags::MmapFlags, perms::VmPerms},
};

use super::{
    VMAR_CAP_ADDR, VMAR_LOWEST_ADDR, Vmar, check_page_aligned_range, is_mappable_range,
    mappings::VmMapping, page_fault::PageFaultInfo,
};

impl Vmar {
    /// Maps `len` bytes of anonymous memory.
    ///
    /// A `MAP_PRIVATE` mapping gets a fresh zeroed page on every fault, so
    /// nothing is materialized here. A `MAP_SHARED` mapping instead needs an
    /// object to share, so it is backed by one created here; use
    /// [`Self::mmap_backed`] to share a specific object with other address
    /// spaces, which is what a named shared memory region is for.
    pub fn mmap_anonymous(
        &self,
        addr: Vaddr,
        len: usize,
        prot: VmPerms,
        flags: MmapFlags,
    ) -> Result<Vaddr> {
        let backing = if flags.is_shared() {
            Some(Arc::clone(&crate::vm::shm::new_anonymous(len)?) as Arc<dyn Backing>)
        } else {
            None
        };
        self.mmap_inner(backing, addr, len, prot, flags, 0)
    }

    /// Maps `len` bytes of `backing`, starting at `offset` within it.
    ///
    /// The mapping is rejected with [`EINVAL`](crate::errno::Errno::EINVAL) if
    /// it would reach past the end of the object, so that a mapping always has
    /// backing and never has to answer a fault with a signal.
    pub fn mmap_backed(
        &self,
        backing: &Arc<dyn Backing>,
        addr: Vaddr,
        len: usize,
        prot: VmPerms,
        flags: MmapFlags,
        offset: usize,
    ) -> Result<Vaddr> {
        self.mmap_inner(Some(Arc::clone(backing)), addr, len, prot, flags, offset)
    }

    /// The body of `mmap(2)`, shared by both wrappers above.
    fn mmap_inner(
        &self,
        backing: Option<Arc<dyn Backing>>,
        addr: Vaddr,
        len: usize,
        prot: VmPerms,
        flags: MmapFlags,
        offset: usize,
    ) -> Result<Vaddr> {
        // ---- validate ----
        if len == 0 {
            crate::return_errno!(EINVAL, "cannot map zero bytes");
        }
        // Linux rounds a partial trailing page up rather than failing.
        let Some(len) = len.checked_next_multiple_of(PAGE_SIZE) else {
            crate::return_errno!(ENOMEM, "the length {len} overflows");
        };
        if !addr.is_multiple_of(PAGE_SIZE) {
            crate::return_errno!(EINVAL, "the address {addr:#x} is not page-aligned");
        }
        if !offset.is_multiple_of(PAGE_SIZE) {
            crate::return_errno!(EINVAL, "the offset {offset:#x} is not page-aligned");
        }
        flags.validate()?;
        if prot != prot.granted() {
            crate::return_errno!(EINVAL, "{prot:?} is not a valid protection");
        }
        // The mapping starts out exactly as permissive as it will ever be.
        let perms = prot.with_ceiling();
        perms.check()?;

        let Some(size) = NonZeroUsize::new(len) else {
            crate::return_errno!(EINVAL, "cannot map zero bytes");
        };

        // A mapping must be fully backed, so that a fault on it can always be
        // resolved from the object.
        if let Some(backing) = &backing {
            let Some(end) = offset.checked_add(len) else {
                crate::return_errno!(
                    EOVERFLOW,
                    "the offset {offset:#x} plus {len} bytes overflows"
                );
            };
            if end > backing.size() {
                crate::return_errno!(
                    EINVAL,
                    "the range {offset:#x}..{end:#x} is past the end of the {:#x}-byte object",
                    backing.size()
                );
            }
        }

        // ---- choose an address ----
        let mut inner = self.inner.write();
        let high_limit = inner.default_high_limit();

        let start = if flags.contains(MmapFlags::FIXED) {
            addr
        } else if flags.contains(MmapFlags::FIXED_NOREPLACE) {
            let wanted = addr..addr + len;
            if inner.mappings.count_overlap(&wanted) != 0 {
                crate::return_errno!(
                    EEXIST,
                    "MAP_FIXED_NOREPLACE: {addr:#x}..{:#x} is already mapped",
                    wanted.end
                );
            }
            addr
        } else {
            #[cfg(target_arch = "x86_64")]
            let search_high = if flags.contains(MmapFlags::MAP_32BIT) {
                super::MAP_32BIT_HIGH_LIMIT.min(high_limit)
            } else {
                high_limit
            };
            #[cfg(not(target_arch = "x86_64"))]
            let search_high = high_limit;

            // A non-zero address is a hint: use it when it is usable, otherwise
            // search. Address zero means "no hint".
            let hint = (addr != 0).then(|| addr..addr + len);
            match hint {
                Some(hint)
                    if is_mappable_range(&hint) && inner.mappings.count_overlap(&hint) == 0 =>
                {
                    hint.start
                }
                _ => inner.find_free_region(len, search_high)?.start,
            }
        };

        let range = start..start + len;
        if !is_mappable_range(&range) {
            crate::return_errno!(
                ENOMEM,
                "{:#x}..{:#x} is not inside the user address space {VMAR_LOWEST_ADDR:#x}..{VMAR_CAP_ADDR:#x}",
                range.start,
                range.end
            );
        }
        // `MAP_FIXED` is exempt from the address space limit, as it is on Linux.
        // It names the address rather than hinting at it, and it replaces what was
        // there, so the limit is not what decides whether it can happen: counting
        // the bytes it displaces would still reject a `MAP_FIXED` that grows the
        // total, which Linux allows. Every other placement is checked.
        if !flags.contains(MmapFlags::FIXED) {
            inner.check_fits_addr_space(self, len)?;
        }

        // Whatever was in the way is replaced. This is the `MAP_FIXED`
        // contract, and a no-op for every other placement.
        inner.unmap_range(&self.vm_space, &range);

        let mapping = VmMapping::new(start, size, perms, flags.is_shared(), backing, offset);
        inner.total_vm += len;
        inner.mappings.insert(mapping);
        drop(inner);

        if flags.contains(MmapFlags::POPULATE) {
            self.populate_range(&range)?;
        }
        Ok(start)
    }

    /// Faults in every page of the mapped part of `range` that is not resident
    /// yet.
    ///
    /// This is how `MADV_WILLNEED` and `MAP_POPULATE` are served: both are
    /// defined in terms of "access the range soon", so both are satisfied by
    /// running the ordinary fault path over the range rather than by a second,
    /// backing-specific mechanism.
    pub fn populate_range(&self, range: &core::ops::Range<Vaddr>) -> Result<()> {
        check_page_aligned_range(range)?;

        // Work from copies of the records so that the address-space lock is not
        // held while faults are resolved, and so that a concurrent `munmap`
        // cannot pull a record out from under the cursor.
        let mappings: alloc::vec::Vec<VmMapping> = self
            .inner
            .read()
            .mappings
            .iter_in(range)
            .iter()
            .map(|m| m.dup())
            .collect();

        for mapping in mappings {
            // Only the part of the mapping that was asked for. Faulting in a
            // whole mapping because one page of it was mentioned would defeat
            // the point of the call.
            let wanted = super::intersect(range, &mapping.range());
            let perms = mapping.perms().granted();
            let mut addr = wanted.start;
            while addr < wanted.end {
                super::page_fault::handle_mapping_fault(
                    &mapping,
                    &self.vm_space,
                    &PageFaultInfo::new(addr, perms),
                )?;
                addr += PAGE_SIZE;
            }
        }
        Ok(())
    }
}
