// SPDX-License-Identifier: GPL-2.0

//! `msync(2)`.
//!
//! `msync` asks the kernel to bring a shared mapping in line with the object
//! behind it. It has two jobs, and the flag says which is meant:
//!
//! - `MS_SYNC` and `MS_ASYNC` write back whatever the mapping has changed. That
//!   is [`Backing::flush`], which does nothing for an object that is memory.
//! - `MS_INVALIDATE` drops the mapping's page table entries, so a later access
//!   re-reads the object instead of trusting what is there.
//!
//! # What this is worth for a memory-backed object
//!
//! Less than the name suggests, and it is better to say so than to leave a caller
//! guessing. Every mapping of one object points at the same physical pages, so
//! there is no second copy to bring up to date and nothing cached to discard.
//! `MS_SYNC` over a [`SharedPages`](crate::vm::shm::SharedPages) therefore does
//! nothing at all, and `MS_INVALIDATE` amounts to `MADV_DONTNEED` on the page
//! table: the entries go, and the next fault installs the same frames again.
//!
//! What it *is* worth is the argument validation, the `flush` seam a file-backed
//! object will fill in, and the page table invalidation a file backing needs in
//! order to honour `MS_INVALIDATE` properly. Once such a backing exists the code
//! below already does the right thing.
//!
//! # `MS_ASYNC`
//!
//! `MS_ASYNC` promises only that the writeback has been *started*. There is no
//! I/O layer to start it with, so this performs the work and returns once it is
//! done — that is, `MS_ASYNC` behaves as `MS_SYNC`. The deviation is forced by
//! what the kernel has rather than chosen: a caller relying on `MS_ASYNC` to
//! return early would instead wait, which is slower but never wrong.

use core::ops::Range;

use ostd::mm::{PAGE_SIZE, Vaddr};

use crate::{
    errno::Result,
    vm::flags::MsyncFlags,
};

use super::{Vmar, is_mappable_range, mappings::VmMapping};

impl Vmar {
    /// Brings the mapping of `range` in line with the object behind it.
    ///
    /// The end of `range` need not be page-aligned; like Linux, the length is
    /// rounded up so a partial trailing page is covered. A length of zero
    /// succeeds and does nothing, which is what `msync(addr, 0, ...)` does.
    pub fn msync(&self, flags: MsyncFlags, addr: Vaddr, len: usize) -> Result<()> {
        // Checked here as well as in `from_raw`, because a caller that assembles
        // the flags itself must not be able to slip past the rule.
        flags.validate()?;

        if !addr.is_multiple_of(PAGE_SIZE) {
            crate::return_errno!(EINVAL, "the address {addr:#x} is not page-aligned");
        }
        let Some(len) = len.checked_next_multiple_of(PAGE_SIZE) else {
            crate::return_errno!(EINVAL, "a length of {len} bytes from {addr:#x} overflows");
        };
        if len == 0 {
            return Ok(());
        }
        let Some(end) = addr.checked_add(len) else {
            crate::return_errno!(EINVAL, "a length of {len} bytes from {addr:#x} overflows");
        };
        let range = addr..end;

        if !is_mappable_range(&range) {
            crate::return_errno!(
                ENOMEM,
                "the range {range:#x?} is not inside the user address space"
            );
        }
        {
            let inner = self.inner.read();
            if !inner.mappings.is_fully_mapped(&range) {
                let missing = range.len() - inner.mappings.count_overlap(&range);
                crate::return_errno!(
                    ENOMEM,
                    "{missing:#x} of the {}-byte range are not mapped",
                    range.len()
                );
            }
        }

        // Copies, so that the address space lock is not held across the flush:
        // that is the one step that may block, and it is the one that does not
        // need the lock.
        let mappings: alloc::vec::Vec<VmMapping> = self
            .inner
            .read()
            .mappings
            .iter_in(&range)
            .iter()
            .map(|mapping| mapping.dup())
            .collect();

        // Write back first, so that an invalidation cannot discard a dirty page
        // that was never pushed out.
        for mapping in &mappings {
            if let Some(backing) = mapping.backing()
                && let Some(object) = object_range(mapping, &range, backing.size())
            {
                backing.flush(object)?;
            }
        }

        if flags.contains(MsyncFlags::INVALIDATE) {
            let mut inner = self.inner.write();
            for mapping in &mappings {
                let part = super::intersect(&range, &mapping.range());
                inner.unmap_pages_only(&self.vm_space, &part);
            }
        }
        Ok(())
    }
}

/// Returns the part of `wanted` that `mapping` backs, in object offsets, or
/// `None` if the two do not overlap the object at all.
///
/// A mapping may be larger than its object, and the pages past the object's end
/// are private, so they take no part in a flush.
fn object_range(
    mapping: &VmMapping,
    wanted: &Range<Vaddr>,
    object_size: usize,
) -> Option<Range<usize>> {
    let part = super::intersect(wanted, &mapping.range());
    // Translate the virtual sub-range into object offsets: the mapping's own
    // offset applies at its start, so the sub-range's start is that much further
    // in.
    let start = mapping.backing_offset() + (part.start - mapping.start());
    let backed = start..start + part.len().min(object_size.saturating_sub(start));
    (!backed.is_empty()).then_some(backed)
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{
            Vmar,
            backing::Backing,
            flags::MmapFlags,
            perms::VmPerms,
            shm::SharedPages,
            tests::{not_resident, page_at, peek, poke, paddr_at},
        },
    };

    const AT: Vaddr = 0x4000_0000;

    /// An address space with `pages` writable pages at [`AT`], all resident, each
    /// holding its own value.
    fn resident_anon(pages: usize) -> Arc<Vmar> {
        let vmar = Vmar::new();
        vmar.mmap_anonymous(
            AT,
            pages * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        for index in 0..pages {
            poke(&vmar, AT + index * PAGE_SIZE, index as u64);
        }
        vmar
    }

    /// An address space with a writable mapping of the whole of `object`.
    fn shared_of(object: &Arc<SharedPages>) -> Arc<Vmar> {
        let vmar = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(object) as Arc<dyn Backing>;
        vmar.mmap_backed(
            &backing,
            AT,
            object.size(),
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        vmar
    }

    #[ktest]
    fn a_zero_length_sync_succeeds() {
        let vmar = resident_anon(1);
        vmar.msync(MsyncFlags::SYNC, AT, 0).unwrap();
    }

    #[ktest]
    fn an_unaligned_address_is_rejected() {
        let vmar = resident_anon(1);
        assert_eq!(
            vmar.msync(MsyncFlags::SYNC, AT + 8, PAGE_SIZE).unwrap_err(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn a_partial_trailing_page_is_rounded_up() {
        // The length need not be page-aligned; the page it lands in is covered.
        let vmar = resident_anon(1);
        vmar.msync(MsyncFlags::SYNC, AT, 1).unwrap();
        vmar.msync(MsyncFlags::SYNC, AT, PAGE_SIZE + 1).unwrap_err();
    }

    #[ktest]
    fn a_range_past_the_end_of_the_address_space_is_rejected() {
        let vmar = resident_anon(1);
        assert_eq!(
            vmar.msync(MsyncFlags::SYNC, AT, usize::MAX).unwrap_err(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn a_range_outside_user_space_is_rejected() {
        let vmar = resident_anon(1);
        // Below `mmap_min_addr`, so not something any mapping can be at.
        assert_eq!(
            vmar.msync(MsyncFlags::SYNC, 0, PAGE_SIZE).unwrap_err(),
            Errno::ENOMEM
        );
    }

    #[ktest]
    fn a_range_containing_a_hole_is_rejected() {
        let vmar = resident_anon(1);
        assert_eq!(
            vmar.msync(MsyncFlags::SYNC, AT, 4 * PAGE_SIZE).unwrap_err(),
            Errno::ENOMEM
        );
        // The whole mapping is fine.
        vmar.msync(MsyncFlags::SYNC, AT, PAGE_SIZE).unwrap();
    }

    #[ktest]
    fn the_flags_must_name_exactly_one_action() {
        let vmar = resident_anon(1);
        // Built directly, so this checks that the operation validates rather than
        // trusting a parsed argument.
        assert_eq!(
            vmar.msync(MsyncFlags::empty(), AT, PAGE_SIZE).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            vmar
                .msync(
                    MsyncFlags::SYNC | MsyncFlags::INVALIDATE,
                    AT,
                    PAGE_SIZE
                )
                .unwrap_err(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn sync_over_a_memory_object_keeps_the_contents() {
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let vmar = shared_of(&object);
        poke(&vmar, AT, 0x5151);
        vmar.msync(MsyncFlags::SYNC, AT, PAGE_SIZE).unwrap();
        // Nothing was pushed anywhere, and nothing was lost either.
        assert_eq!(peek(&vmar, AT), 0x5151);
        assert_eq!(object.peek_u64(0).unwrap(), 0x5151);
        // The page is still resident: a writeback is not an invalidation.
        assert!(!not_resident(&vmar, AT));
    }

    #[ktest]
    fn async_behaves_like_sync_and_says_so() {
        // There is no I/O layer to defer to, so `MS_ASYNC` does the work and
        // waits. The deviation is documented in the module.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let vmar = shared_of(&object);
        poke(&vmar, AT, 0x6262);
        vmar.msync(MsyncFlags::ASYNC, AT, PAGE_SIZE).unwrap();
        assert_eq!(object.peek_u64(0).unwrap(), 0x6262);
        assert!(!not_resident(&vmar, AT));
    }

    #[ktest]
    fn invalidate_leaves_a_shared_object_alone() {
        // The sharp end of this operation: dropping the entries must not drop the
        // object's pages, because another mapping may still be using them.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let one = shared_of(&object);
        // A second address space, at a different address, over the same object.
        let other = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        let second = 0x5000_0000;
        other.mmap_backed(
            &backing,
            second,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();

        poke(&one, AT, 0xabcd);
        poke(&other, second, 0xabcd);
        let shared_frame = paddr_at(&other, second);
        assert_eq!(paddr_at(&one, AT), shared_frame);

        one.msync(MsyncFlags::INVALIDATE, AT, PAGE_SIZE).unwrap();

        // The mapping survives, its page does not, and the object still has the
        // frame the other mapping is using.
        assert!(not_resident(&one, AT));
        assert_eq!(one.total_mapped_size(), PAGE_SIZE);
        assert_eq!(paddr_at(&other, second), shared_frame);
        assert_eq!(peek(&other, second), 0xabcd);

        // Faulting the invalidated mapping back in gives the same page back, not
        // a zeroed one: nothing was lost.
        assert_eq!(peek(&one, AT), 0xabcd);
        assert_eq!(paddr_at(&one, AT), shared_frame);
    }

    #[ktest]
    fn invalidate_over_private_anonymous_memory_gives_zeros() {
        // The opposite case, from the same code: a private page belongs to nobody
        // else, so dropping it discards its contents. This is what makes the two
        // outcomes correct rather than accidental.
        let vmar = resident_anon(1);
        poke(&vmar, AT, 0x7777);
        vmar.msync(MsyncFlags::INVALIDATE, AT, PAGE_SIZE).unwrap();

        assert!(not_resident(&vmar, AT));
        // The mapping is still there.
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);
        // And re-faulting gives a fresh zeroed page.
        assert_eq!(peek(&vmar, AT), 0);
    }

    #[ktest]
    fn invalidate_part_of_a_mapping_leaves_the_rest() {
        let vmar = resident_anon(3);
        let before = [
            paddr_at(&vmar, AT),
            paddr_at(&vmar, AT + PAGE_SIZE),
            paddr_at(&vmar, AT + 2 * PAGE_SIZE),
        ];
        vmar.msync(MsyncFlags::INVALIDATE, AT + PAGE_SIZE, PAGE_SIZE)
            .unwrap();

        assert!(!not_resident(&vmar, AT));
        assert!(not_resident(&vmar, AT + PAGE_SIZE));
        assert!(!not_resident(&vmar, AT + 2 * PAGE_SIZE));
        assert_eq!(paddr_at(&vmar, AT), before[0]);
        assert_eq!(paddr_at(&vmar, AT + 2 * PAGE_SIZE), before[2]);
        // The mapping was not split into three records by the operation.
        assert_eq!(vmar.mappings_in(AT..AT + 3 * PAGE_SIZE).len(), 1);
    }

    #[ktest]
    fn a_partial_flush_reaches_only_the_object_bytes_asked_for() {
        // The mapping covers two pages of the object; syncing the first must not
        // be reported as covering the second.
        let object = SharedPages::new(2 * PAGE_SIZE).unwrap();
        let vmar = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        // A window onto the second page only.
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            PAGE_SIZE,
        )
        .unwrap();
        poke(&vmar, AT, 0x1111);
        vmar.msync(MsyncFlags::SYNC, AT, PAGE_SIZE).unwrap();
        assert_eq!(object.peek_u64(PAGE_SIZE).unwrap(), 0x1111);
        assert_eq!(object.peek_u64(0).unwrap(), 0);
    }

    #[ktest]
    fn a_sync_of_an_oversized_mapping_stays_inside_the_object() {
        // `mmap_backed` refuses to make a mapping that is larger than its object,
        // so `mremap` is the only way to reach that state. The page past the end
        // of the object is private, and flushing must not run off the end of the
        // object to reach it.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let vmar = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        // Grow the mapping past the object.
        vmar.resize_mapping(AT, PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);

        poke(&vmar, AT, 0x2222);
        vmar.msync(MsyncFlags::SYNC, AT, 2 * PAGE_SIZE).unwrap();
        assert_eq!(object.peek_u64(0).unwrap(), 0x2222);
    }

    #[ktest]
    fn sync_over_a_range_of_several_mappings() {
        // Two separate mappings of one object, synced in one call.
        let object = SharedPages::new(2 * PAGE_SIZE).unwrap();
        let vmar = Vmar::new();
        let backing: Arc<dyn Backing> = Arc::clone(&object) as Arc<dyn Backing>;
        vmar.mmap_backed(
            &backing,
            AT,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            0,
        )
        .unwrap();
        vmar.mmap_backed(
            &backing,
            AT + PAGE_SIZE,
            PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            PAGE_SIZE,
        )
        .unwrap();
        poke(&vmar, AT, 0x3333);
        poke(&vmar, AT + PAGE_SIZE, 0x4444);

        vmar.msync(MsyncFlags::SYNC, AT, 2 * PAGE_SIZE).unwrap();
        assert_eq!(object.peek_u64(0).unwrap(), 0x3333);
        assert_eq!(object.peek_u64(PAGE_SIZE).unwrap(), 0x4444);

        // Invalidating both keeps the mapping count and the object intact.
        vmar.msync(MsyncFlags::INVALIDATE, AT, 2 * PAGE_SIZE).unwrap();
        assert!(not_resident(&vmar, AT) && not_resident(&vmar, AT + PAGE_SIZE));
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
        assert_eq!(peek(&vmar, AT), 0x3333);
        assert_eq!(peek(&vmar, AT + PAGE_SIZE), 0x4444);
    }

    #[ktest]
    fn the_page_flags_are_untouched() {
        // A writeback is not a permission change, so the page table entry must
        // come out the way it went in.
        let object = SharedPages::new(PAGE_SIZE).unwrap();
        let vmar = shared_of(&object);
        poke(&vmar, AT, 1);
        let before = page_at(&vmar, AT).unwrap().1;
        vmar.msync(MsyncFlags::SYNC, AT, PAGE_SIZE).unwrap();
        assert_eq!(page_at(&vmar, AT).unwrap().1, before);
    }
}
