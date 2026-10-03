// SPDX-License-Identifier: GPL-2.0

//! `munmap(2)` and `madvise(2)`.

use core::ops::Range;

use ostd::mm::Vaddr;

use crate::{api::errno::Result, vm::flags::MadviseAdvice};

use super::{Vmar, check_page_aligned_range, is_mappable_range};

impl Vmar {
    /// Releases the mappings that intersect `range`.
    ///
    /// Unmapping a range that is not mapped is not an error, so a range that
    /// covers holes is released in part and reported as a success.
    pub fn munmap(&self, range: Range<Vaddr>) -> Result<()> {
        check_page_aligned_range(&range)?;
        if range.end <= range.start {
            crate::return_errno!(EINVAL, "the range wraps around the address space or is empty");
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
            crate::return_errno!(
                EFAULT,
                "the range {range:#x?} is not in the user address space"
            );
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
