// SPDX-License-Identifier: GPL-2.0

//! `munmap(2)` and `madvise(2)`.

use core::ops::Range;

use ostd::mm::Vaddr;

use crate::{
    errno::Result,
    vm::flags::MadviseAdvice,
};

use super::{Vmar, check_page_aligned_range, is_mappable_range};

impl Vmar {
    /// Releases the mappings that intersect `range`.
    ///
    /// Unmapping a range that is not mapped is not an error, so a range that
    /// covers holes is released in part and reported as a success.
    pub fn munmap(&self, range: Range<Vaddr>) -> Result<()> {
        check_page_aligned_range(&range)?;
        if range.end < range.start {
            crate::return_errno!(EINVAL, "the range wraps around the address space");
        }

        let mut inner = self.inner.write();
        inner.unmap_range(&self.vm_space, &range);
        Ok(())
    }

    /// Gives the kernel a hint about how `range` will be used.
    pub fn madvise(&self, advice: MadviseAdvice, range: Range<Vaddr>) -> Result<()> {
        check_page_aligned_range(&range)?;

        if advice == MadviseAdvice::WillNeed {
            let _ = self.populate_range(&range);
            return Ok(());
        }

        if !advice.drops_pages() {
            // `MADV_NORMAL`, `MADV_RANDOM` and `MADV_SEQUENTIAL` only tune the
            // kernel's readahead heuristics, which a RAM-only backing has no use
            // for.
            return Ok(());
        }

        if !is_mappable_range(&range) {
            crate::return_errno!(EFAULT, "the range {range:#x?} is not in the user address space");
        }

        // Dropping the page table entries is all that is needed, and it is all
        // that is correct: the pages themselves belong to the backing object,
        // which keeps both its contents and the pages other mappings of it are
        // using. For private anonymous memory a later fault allocates a fresh
        // zeroed page, which is what `MADV_DONTNEED` promises.
        //
        // The record is put back, so unlike `munmap` the address range stays
        // mapped and a later access re-faults instead of failing.
        let mut inner = self.inner.write();
        let outcome = inner.unmap_pages_only(&self.vm_space, &range);

        if outcome.mapped < range.len() {
            crate::return_errno!(
                ENOMEM,
                "{:#x} of the {}-byte range are not mapped",
                range.len() - outcome.mapped,
                range.len()
            );
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::mm::PAGE_SIZE;
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{flags::MmapFlags, perms::VmPerms},
    };

    const AT: Vaddr = 0x4000_0000;

    fn vmar_with(pages: usize) -> alloc::sync::Arc<crate::vm::Vmar> {
        let vmar = crate::vm::Vmar::new();
        vmar.mmap_anonymous(AT, pages * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        vmar
    }

    #[ktest]
    fn munmap_releases_the_range() {
        let vmar = vmar_with(4);
        vmar.munmap(AT + PAGE_SIZE..AT + 3 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
        assert!(!vmar.is_fully_mapped(&(AT..AT + 4 * PAGE_SIZE)));
        assert!(vmar.is_fully_mapped(&(AT..AT + PAGE_SIZE)));
        assert!(vmar.is_fully_mapped(&(AT + 3 * PAGE_SIZE..AT + 4 * PAGE_SIZE)));

        vmar.munmap(AT..AT + 4 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), 0);
    }

    #[ktest]
    fn unmapping_a_hole_succeeds() {
        let vmar = vmar_with(1);
        // A range that misses the mapping entirely changes nothing, and is not
        // an error.
        vmar.munmap(AT + PAGE_SIZE..AT + 8 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);
        vmar.munmap(AT + 64 * PAGE_SIZE..AT + 65 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);

        // A range that overlaps it at either end is released in part.
        vmar.munmap(AT - PAGE_SIZE..AT + 1).unwrap_err();
        vmar.munmap(AT..AT + PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), 0);
    }

    #[ktest]
    fn bad_ranges_are_rejected() {
        let vmar = vmar_with(1);
        assert_eq!(vmar.munmap(AT..AT).unwrap_err(), Errno::EINVAL);
        assert_eq!(
            vmar.munmap(AT + 8..AT + PAGE_SIZE).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);
    }

    #[ktest]
    fn dontneed_keeps_the_mapping_but_drops_the_pages() {
        let vmar = vmar_with(2);
        vmar.madvise(MadviseAdvice::DontNeed, AT..AT + 2 * PAGE_SIZE)
            .unwrap();
        // The mapping record survives; only the page table entries are gone, so
        // a later access re-faults.
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
        assert!(vmar.is_fully_mapped(&(AT..AT + 2 * PAGE_SIZE)));
    }

    #[ktest]
    fn dontneed_over_a_hole_is_enomem() {
        let vmar = vmar_with(1);
        assert_eq!(
            vmar.madvise(MadviseAdvice::DontNeed, AT..AT + 4 * PAGE_SIZE)
                .unwrap_err(),
            Errno::ENOMEM
        );
    }

    #[ktest]
    fn unknown_advice_is_rejected_and_hints_succeed() {
        let vmar = vmar_with(1);
        let unknown = MadviseAdvice::from_raw(5);
        assert_eq!(unknown.unwrap_err(), Errno::EINVAL);
        for advice in [
            MadviseAdvice::Normal,
            MadviseAdvice::Random,
            MadviseAdvice::Sequential,
            MadviseAdvice::WillNeed,
        ] {
            vmar.madvise(advice, AT..AT + PAGE_SIZE).unwrap();
        }
    }
}
