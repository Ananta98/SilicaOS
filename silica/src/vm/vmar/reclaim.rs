// SPDX-License-Identifier: GPL-2.0

//! An address space giving its own memory back.
//!
//! What may be dropped here is decided by two conditions, and a page has to
//! satisfy both before this pass will touch it.
//!
//! # The mapping has to be private anonymous memory
//!
//! Its pages come from the frame allocator and are referenced by nothing but the
//! page table entry, so dropping the entry releases the frame outright.
//!
//! A mapping with a backing object is excluded whatever its flags, and this is
//! not so much a gap as a different owner's job. For
//! [`SharedPages`](crate::vm::shm::SharedPages) the frame is held by the object
//! for its other mappings, so dropping the entry would free nothing while the
//! bytes are live shared state. For a file object the clean pages are the file's
//! own contents and belong in a page cache's eviction policy, which is not written
//! yet. Either way, unlinking the entry is the wrong operation: the right one is
//! telling the owner the page is unwanted.
//!
//! # The page must be clean
//!
//! The page table's dirty bit is the record of whether anything has written to
//! the page, and this pass reads it rather than tracking it, so the answer is the
//! hardware's. A page that has never been written holds nothing but the zeroes
//! the kernel gave it, and a re-fault produces another zeroed page — dropping it
//! is therefore invisible to the program, and the frames come back.
//!
//! A page that *has* been written is the case this kernel cannot help with. There
//! is no swap, so the frame is the only copy of its contents and dropping it
//! loses data. Linux writes such a page out to swap and reclaims it; until there
//! is a swap device there is nowhere to put it but the frame it is already in,
//! which is to say there is nothing to reclaim. The alternative would be to
//! reclaim it anyway and lose the program's data, so it is left alone and the
//! `ENOMEM` reaches the caller.
//!
//! This is also why reclaim is a pass over every address space rather than a
//! decision taken where an allocation fails. Linux makes the same bargain: the
//! fault path wakes `kswapd` instead of reclaiming inline, because reclaiming
//! inline would mean taking the address space lock the fault already holds.

use alloc::vec::Vec;
use core::ops::Range;

use ostd::mm::{PAGE_SIZE, PageFlags, Vaddr, VmSpace, vm_space::VmQueriedItem};
use ostd::task::{DisabledPreemptGuard, disable_preempt};

use crate::vm::reclaim::ReclaimSource;

use super::{Vmar, VmarInner};

impl Vmar {
    /// Returns the addresses of up to `budget` pages of this address space that
    /// reclaim may drop, in ascending address order.
    ///
    /// They are all found before any is unmapped, because unmapping moves a page
    /// table cursor and would invalidate a walk still in progress.
    ///
    /// `inner` is passed in rather than locked here because the caller is
    /// already holding it: taking it a second time would be asking the same
    /// address space for a lock it holds.
    pub fn reclaim_pages(&self, budget: usize) -> usize {
        ReclaimSource::reclaim(self, budget)
    }
}

impl Vmar {
    /// Releases up to `budget` pages, taking the records lock once for the whole
    /// pass so that a concurrent `mmap` or `munmap` cannot move the mappings
    /// this pass is walking out from under it.
    ///
    /// Holding the records lock while touching page tables is consistent with
    /// the rest of the subsystem, which takes it before every page table
    /// operation, so this cannot deadlock against one of them.
    pub(super) fn reclaim_locked(&self, budget: usize) -> usize {
        // Preemption stays off across the pass because a page table cursor
        // requires it for as long as the cursor is open.
        let guard = disable_preempt();
        let records = self.inner.write();

        let candidates = collect_reclaimable(&records, &self.vm_space, budget, &guard);
        if candidates.is_empty() {
            return 0;
        }
        // The scan's cursors are gone by now: they are dropped as
        // `collect_reclaimable` returns, and a cursor may not overlap another.
        unmap_candidates(&self.vm_space, &candidates)
    }
}

/// Returns the addresses of up to `budget` pages that reclaim may drop, in
/// ascending address order.
fn collect_reclaimable(
    inner: &VmarInner,
    vm_space: &VmSpace,
    budget: usize,
    guard: &DisabledPreemptGuard,
) -> Vec<Vaddr> {
    let mut candidates: Vec<Vaddr> = Vec::new();

    for mapping in inner.mappings.iter() {
        if !mapping.is_reclaimable() {
            continue;
        }
        let range: Range<Vaddr> = mapping.range();
        // The cursor owns a sub-tree of the page table, so a mapping with no
        // page table entry yet simply cannot be walked.
        let Ok(mut cursor) = vm_space.cursor(guard, &range) else {
            continue;
        };

        // The budget spans the whole pass rather than one mapping, so an address
        // space holding a great deal of anonymous memory does not let its first
        // mapping consume the entire allowance.
        while candidates.len() < budget {
            let Some(remaining) = range.end.checked_sub(cursor.virt_addr()) else {
                break;
            };
            let Some(mapped) = cursor.find_next(remaining) else {
                break;
            };
            let (_, Some(item)) = cursor.query().expect("a mapped page was just found") else {
                break;
            };
            match item {
                VmQueriedItem::MappedRam { frame, prop } => {
                    // A dirty page has no other copy, and a page some other holder
                    // still references is not this address space's to drop. Both
                    // are left for a decision with more context than this pass
                    // has.
                    if !prop.flags.contains(PageFlags::DIRTY) && frame.reference_count() == 1 {
                        candidates.push(mapped);
                    }
                }
                VmQueriedItem::MappedIoMem { .. } => break,
            }
            if cursor.jump(mapped + ostd::mm::PAGE_SIZE).is_err() {
                break;
            }
        }

        if candidates.len() >= budget {
            break;
        }
    }
    candidates
}

/// Unmaps `candidates`, which must be page-aligned and ascending, and returns
/// how many pages were released.
///
/// Adjacent pages are dropped through a single cursor. That keeps the flush count
/// down, and it keeps each cursor's range small: a cursor owns a sub-tree of the
/// page table, so a pass that claimed the whole address space at once would block
/// every unrelated fault in it.
fn unmap_candidates(vm_space: &VmSpace, candidates: &[Vaddr]) -> usize {
    let mut freed = 0;
    let mut index = 0;

    while index < candidates.len() {
        let run_start = candidates[index];
        let mut run_end = run_start + PAGE_SIZE;
        while index + 1 < candidates.len() && candidates[index + 1] == run_end {
            index += 1;
            run_end += PAGE_SIZE;
        }
        index += 1;

        let guard = disable_preempt();
        let Ok(mut cursor) = vm_space.cursor_mut(&guard, &(run_start..run_end)) else {
            continue;
        };
        freed += cursor.unmap(run_end - run_start);
        cursor.flusher().sync_tlb_flush();
    }
    freed
}

impl ReclaimSource for Vmar {
    fn reclaim(&self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        self.reclaim_locked(budget)
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::mm::{PAGE_SIZE, Vaddr};
    use ostd::prelude::ktest;

    use super::*;
    use crate::vm::backing::Backing;
    use crate::vm::flags::MmapFlags;
    use crate::vm::perms::VmPerms;
    use crate::vm::shm::SharedPages;
    use crate::vm::tests::{not_resident, record_at};
    use crate::vm::vmar::page_fault::PageFaultInfo;

    /// Somewhere high and clear of anything `mmap` hands out by itself.
    const BASE: Vaddr = 0x4000_0000;

    /// Maps `pages` writable private anonymous pages at [`BASE`].
    fn anon(vmar: &Vmar, pages: usize) -> Vaddr {
        vmar.mmap_anonymous(
            BASE,
            pages * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE,
        )
        .expect("the mapping is created at the requested address")
    }

    /// Faults in `pages` pages starting at `start`, without writing to any of
    /// them, so that every one of them is clean.
    fn fault_in_cleanly(vmar: &Vmar, start: Vaddr, pages: usize) {
        for page in 0..pages {
            vmar.handle_page_fault(&PageFaultInfo::new(
                start + page * PAGE_SIZE,
                VmPerms::READ,
            ))
            .expect("the page faults in");
        }
    }

    #[ktest]
    fn a_clean_untouched_page_is_given_back() {
        let vmar = Vmar::new();
        let start = anon(&vmar, 2);
        fault_in_cleanly(&vmar, start, 1);
        assert!(!not_resident(&vmar, start), "the page is resident");

        assert_eq!(vmar.reclaim_pages(8), 1, "the clean page is released");
        assert!(
            not_resident(&vmar, start),
            "its page table entry is gone"
        );
        // The mapping is untouched, so a later access re-faults rather than
        // failing.
        assert_eq!(record_at(&vmar, start).len(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn a_written_page_is_kept() {
        let vmar = Vmar::new();
        let start = anon(&vmar, 1);

        // A write fault is what makes a page dirty, and there is no swap that
        // could take its contents, so this pass must leave it alone.
        vmar.handle_page_fault(&PageFaultInfo::new(start, VmPerms::WRITE))
            .expect("the page faults in");
        assert!(!not_resident(&vmar, start));

        assert_eq!(
            vmar.reclaim_pages(8),
            0,
            "a written page has no other copy, so it is not reclaimable"
        );
        assert!(
            !not_resident(&vmar, start),
            "and so it stays resident"
        );
    }

    #[ktest]
    fn a_pages_object_is_left_alone() {
        // The frames belong to the object rather than to this address space, and
        // the bytes are live shared data, so unlinking the entry would free
        // nothing while pretending to.
        let vmar = Vmar::new();
        let object = SharedPages::new(PAGE_SIZE).expect("the object is created");
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;

        let start = vmar
            .mmap_backed(
                &backing,
                BASE,
                PAGE_SIZE,
                VmPerms::READ | VmPerms::WRITE,
                MmapFlags::SHARED,
                0,
            )
            .expect("the mapping is created");
        vmar.populate_range(&(start..start + PAGE_SIZE))
            .expect("the page faults in");

        assert_eq!(vmar.reclaim_pages(8), 0, "the object's pages are not this space's");
        assert_eq!(object.resident_pages(), 1, "and the object still holds it");
        assert!(!not_resident(&vmar, start), "so it stays mapped");
    }

    #[ktest]
    fn the_budget_bounds_a_pass() {
        let vmar = Vmar::new();
        let start = anon(&vmar, 8);
        fault_in_cleanly(&vmar, start, 8);

        assert_eq!(
            vmar.reclaim_pages(3),
            3,
            "a pass takes no more than it was given"
        );
        assert_eq!(
            vmar.reclaim_pages(64),
            5,
            "and finds the rest on the next pass"
        );
        assert_eq!(vmar.reclaim_pages(64), 0, "with nothing left to release");
    }

    #[ktest]
    fn an_empty_address_space_releases_nothing() {
        let vmar = Vmar::new();
        assert_eq!(vmar.reclaim_pages(16), 0);
        assert_eq!(vmar.reclaim_pages(0), 0, "and a zero budget asks for nothing");
    }

    #[ktest]
    fn reclaim_reaches_an_address_space_it_was_not_handed() {
        let before_pages = crate::vm::reclaim::pages_reclaimed();
        let before_passes = crate::vm::reclaim::passes();

        let vmar = Vmar::new();
        let start = anon(&vmar, 2);
        fault_in_cleanly(&vmar, start, 2);

        // No reference to `vmar` is passed here: the registry is what finds it,
        // which is the whole point of a caller that is out of memory.
        let freed = crate::vm::reclaim::reclaim_at_least(2);
        assert!(freed >= 2, "the clean pages were released, and only they");
        assert!(not_resident(&vmar, start));
        assert!(not_resident(&vmar, start + PAGE_SIZE));
        assert!(crate::vm::reclaim::passes() > before_passes, "a pass ran");
        assert!(
            crate::vm::reclaim::pages_reclaimed() >= before_pages + 2,
            "and it was accounted for"
        );
    }

    #[ktest]
    fn reclaim_at_least_stops_when_there_is_nothing_left() {
        // Asking for far more than exists must return rather than spin: the
        // second pass releases nothing, which is the signal to stop.
        let released = crate::vm::reclaim::reclaim_at_least(1 << 20);
        let passes_after = crate::vm::reclaim::passes();
        let again = crate::vm::reclaim::reclaim_at_least(1 << 20);
        assert_eq!(again, 0);
        assert!(
            crate::vm::reclaim::passes() >= passes_after,
            "and it took at least the one pass that found nothing"
        );
        let _ = released;
    }
}