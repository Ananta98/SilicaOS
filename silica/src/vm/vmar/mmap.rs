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
    Vmar, VMAR_CAP_ADDR, VMAR_LOWEST_ADDR, check_page_aligned_range, is_mappable_range,
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
                crate::return_errno!(EOVERFLOW, "the offset {offset:#x} plus {len} bytes overflows");
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
                Some(hint) if is_mappable_range(&hint) && inner.mappings.count_overlap(&hint) == 0 => {
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

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{shm::SharedPages, vmar::VMAR_CAP_ADDR},
    };

    /// The address `mmap` hands out when nothing constrains it.
    const FREE: Vaddr = 0;

    fn anon(len: usize) -> (Arc<super::super::Vmar>, Vaddr) {
        let vmar = super::super::Vmar::new();
        let addr = vmar
            .mmap_anonymous(FREE, len, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        (vmar, addr)
    }

    #[ktest]
    fn anonymous_mapping_lands_in_user_space() {
        let (vmar, addr) = anon(4 * PAGE_SIZE);
        assert!(addr.is_multiple_of(PAGE_SIZE));
        assert!(addr >= VMAR_LOWEST_ADDR && addr + 4 * PAGE_SIZE <= VMAR_CAP_ADDR);
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);
        // A second anonymous mapping must not reuse the address.
        let other = vmar
            .mmap_anonymous(FREE, 4 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        assert_ne!(other, addr);
        assert_eq!(vmar.total_mapped_size(), 8 * PAGE_SIZE);
    }

    #[ktest]
    fn partial_trailing_page_is_rounded_up() {
        let (vmar, addr) = anon(PAGE_SIZE + 1);
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
        assert!(vmar.is_fully_mapped(&(addr..addr + 2 * PAGE_SIZE)));
    }

    #[ktest]
    fn bad_arguments_are_rejected() {
        let vmar = super::super::Vmar::new();
        let prot = VmPerms::READ;
        let shared = MmapFlags::PRIVATE;

        assert_eq!(
            vmar.mmap_anonymous(FREE, 0, prot, shared).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            vmar.mmap_anonymous(PAGE_SIZE + 1, PAGE_SIZE, prot, shared).unwrap_err(),
            Errno::EINVAL
        );
        // A `MAY_*` bit is not a `PROT_*` bit and must not reach `mmap`.
        assert_eq!(
            vmar.mmap_anonymous(FREE, PAGE_SIZE, VmPerms::MAY_READ, shared).unwrap_err(),
            Errno::EINVAL
        );
        // SHARED and PRIVATE at once.
        assert_eq!(
            vmar
                .mmap_anonymous(FREE, PAGE_SIZE, prot, MmapFlags::SHARED | MmapFlags::PRIVATE)
                .unwrap_err(),
            Errno::EINVAL
        );
        // An address below `mmap_min_addr` is only an error when it is
        // demanded; as a hint it is simply ignored and the kernel picks
        // somewhere else.
        assert!(
            vmar.mmap_anonymous(VMAR_LOWEST_ADDR - PAGE_SIZE, PAGE_SIZE, prot, shared).is_ok()
        );
        // `MAP_FIXED` demands it.
        assert_eq!(
            vmar.mmap_anonymous(
                VMAR_LOWEST_ADDR - PAGE_SIZE,
                PAGE_SIZE,
                prot,
                shared | MmapFlags::FIXED
            )
            .unwrap_err(),
            Errno::ENOMEM
        );
    }

    #[ktest]
    fn fixed_noreplace_refuses_to_clobber() {
        let vmar = super::super::Vmar::new();
        let at = 0x4000_0000;
        let flags = MmapFlags::PRIVATE | MmapFlags::FIXED_NOREPLACE;

        vmar.mmap_anonymous(at, PAGE_SIZE, VmPerms::READ, flags).unwrap();
        assert_eq!(
            vmar.mmap_anonymous(at, PAGE_SIZE, VmPerms::READ, flags).unwrap_err(),
            Errno::EEXIST
        );
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);

        // A disjoint range at the same base is fine.
        vmar
            .mmap_anonymous(at + 2 * PAGE_SIZE, PAGE_SIZE, VmPerms::READ, flags)
            .unwrap();
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn fixed_replaces_what_was_there() {
        let vmar = super::super::Vmar::new();
        let at = 0x4000_0000;
        let read = VmPerms::READ;
        let rw = VmPerms::READ | VmPerms::WRITE;
        let perms_of = |addr: Vaddr| crate::vm::tests::perms_at(&vmar, addr);

        vmar
            .mmap_anonymous(at, 4 * PAGE_SIZE, read, MmapFlags::PRIVATE)
            .unwrap();
        // Replace the second page of the run with a writable one. The total is
        // unchanged because exactly one page was swapped for another, but the
        // protection around it shows where the replacement landed.
        vmar.mmap_anonymous(
            at + PAGE_SIZE,
            PAGE_SIZE,
            rw,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();

        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);
        assert_eq!(perms_of(at), read.with_ceiling());
        assert_eq!(perms_of(at + PAGE_SIZE), rw.with_ceiling());
        assert_eq!(perms_of(at + 2 * PAGE_SIZE), read.with_ceiling());
        // Three records, because the writable middle cannot merge with either
        // read-only neighbour.
        assert_eq!(vmar.mappings_in(at..at + 4 * PAGE_SIZE).len(), 3);

        // Replacing the whole run changes the protection of every page.
        vmar.mmap_anonymous(
            at,
            4 * PAGE_SIZE,
            rw,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);
        assert_eq!(perms_of(at), rw.with_ceiling());
        assert_eq!(vmar.mappings_in(at..at + 4 * PAGE_SIZE).len(), 1);
    }

    #[ktest]
    fn a_used_hint_is_ignored() {
        let vmar = super::super::Vmar::new();
        let prot = VmPerms::READ;
        let at = 0x4000_0000;

        vmar
            .mmap_anonymous(at, PAGE_SIZE, prot, MmapFlags::PRIVATE)
            .unwrap();
        // The hint is occupied, so the kernel picks somewhere else instead of
        // failing.
        let other = vmar
            .mmap_anonymous(at, PAGE_SIZE, prot, MmapFlags::PRIVATE)
            .unwrap();
        assert_ne!(other, at);
    }

    #[ktest]
    fn the_address_space_limit_is_enforced() {
        let vmar = super::super::Vmar::new();
        vmar.set_max_addr_space(Some(4 * PAGE_SIZE));

        vmar
            .mmap_anonymous(FREE, 2 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        assert_eq!(
            vmar
                .mmap_anonymous(FREE, 4 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
                .unwrap_err(),
            Errno::ENOMEM
        );
        // Exactly reaching the limit is fine.
        vmar
            .mmap_anonymous(FREE, 2 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);

        vmar.set_max_addr_space(None);
        assert_eq!(vmar.max_addr_space(), None);
        vmar
            .mmap_anonymous(FREE, PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
    }

    #[ktest]
    fn map_fixed_is_exempt_from_the_address_space_limit() {
        // Linux does not apply `RLIMIT_AS` to `MAP_FIXED`, and a program that
        // relies on `MAP_FIXED` to replace a large region must not be refused.
        let vmar = super::super::Vmar::new();
        let prot = VmPerms::READ;
        let private = MmapFlags::PRIVATE;

        let at = at_of(4);

        // Sit exactly on the limit.
        vmar.set_max_addr_space(Some(4 * PAGE_SIZE));
        vmar.mmap_anonymous(at, 4 * PAGE_SIZE, prot, private | MmapFlags::FIXED)
            .unwrap();
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);

        // An unhinted mapping is held to the limit, and there is none left.
        assert_eq!(
            vmar.mmap_anonymous(FREE, PAGE_SIZE, prot, private).unwrap_err(),
            Errno::ENOMEM
        );

        // `MAP_FIXED` is exempt. Naming an address that is free succeeds even
        // though it takes the address space past the limit, which is the whole
        // point of the exemption: a program replacing a region cannot be refused
        // because of where it happens to stand.
        let spare = at_of(64);
        vmar.mmap_anonymous(spare, PAGE_SIZE, prot, private | MmapFlags::FIXED)
            .unwrap();
        assert_eq!(vmar.total_mapped_size(), 5 * PAGE_SIZE);
        // And the limit is still in force for everything else.
        assert_eq!(
            vmar.mmap_anonymous(FREE, PAGE_SIZE, prot, private).unwrap_err(),
            Errno::ENOMEM
        );

        // `MAP_FIXED` over part of a run replaces only that part, so the run is
        // split rather than truncated: one page plus the three that were not
        // named, still four pages in total.
        vmar.mmap_anonymous(at, PAGE_SIZE, prot, private | MmapFlags::FIXED)
            .unwrap();
        assert_eq!(vmar.total_mapped_size(), 5 * PAGE_SIZE);
        assert_eq!(vmar.mappings_in(at..at + 4 * PAGE_SIZE).len(), 2);

        // Dropping the limit lets an unhinted mapping through again.
        vmar.set_max_addr_space(None);
        vmar.mmap_anonymous(FREE, PAGE_SIZE, prot, private).unwrap();
    }

    /// A page-aligned address well clear of the other tests in this module.
    fn at_of(page: usize) -> Vaddr {
        0x3000_0000 + page * PAGE_SIZE
    }

    #[ktest]
    fn a_mapping_may_not_reach_past_its_object() {
        let vmar = super::super::Vmar::new();
        let object = SharedPages::new(4 * PAGE_SIZE).unwrap();
        let backing: Arc<dyn Backing> = object;
        let flags = MmapFlags::PRIVATE;

        vmar
            .mmap_backed(&backing, FREE, 4 * PAGE_SIZE, VmPerms::READ, flags, 0)
            .unwrap();
        assert_eq!(
            vmar
                .mmap_backed(&backing, FREE, PAGE_SIZE, VmPerms::READ, flags, 4 * PAGE_SIZE)
                .unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            vmar
                .mmap_backed(&backing, FREE, 2 * PAGE_SIZE, VmPerms::READ, flags, 3 * PAGE_SIZE)
                .unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            vmar
                .mmap_backed(&backing, FREE, PAGE_SIZE, VmPerms::READ, flags, PAGE_SIZE + 8)
                .unwrap_err(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn each_shared_anonymous_mapping_gets_its_own_object() {
        // Two `MAP_SHARED` anonymous mappings are two distinct objects, exactly
        // as in Linux. Sharing requires naming the object, which is what
        // `mmap_backed` and [`crate::vm::shm`] are for.
        let vmar = super::super::Vmar::new();
        let shared = |vmar: &Arc<super::super::Vmar>| {
            vmar.mmap_anonymous(
                FREE,
                PAGE_SIZE,
                VmPerms::READ | VmPerms::WRITE,
                MmapFlags::SHARED,
            )
            .unwrap()
        };
        let first = shared(&vmar);
        let second = shared(&vmar);

        let object_of = |addr: Vaddr| {
            crate::vm::tests::record_at(&vmar, addr)
                .backing()
                .unwrap()
                .clone()
        };
        assert!(!Arc::ptr_eq(&object_of(first), &object_of(second)));
    }
}
