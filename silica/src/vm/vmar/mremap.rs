// SPDX-License-Identifier: GPL-2.0

//! `mremap(2)` and the address space break used by `sbrk(3)`.

use core::ops::Range;
use ostd::mm::{PAGE_SIZE, Vaddr, vm_space::VmQueriedItem};
use crate::{errno::Result, vm::flags::MremapFlags};

use super::{
    Vmar, VmarInner, check_page_aligned_range, is_intersected, is_mappable_range,
    mappings::VmMapping,
};

impl Vmar {
    /// Moves or resizes the mapping that starts at `old_addr`.
    ///
    /// With `MREMAP_FIXED` the mapping goes to `new_addr`, replacing whatever
    /// was there. Otherwise it stays put if it can grow in place, and moves to a
    /// free address if it cannot. `MREMAP_DONTUNMAP` leaves the old range mapped
    /// onto the same physical pages as the new one, so the two ranges alias.
    pub fn mremap(
        &self,
        old_addr: Vaddr,
        old_size: usize,
        new_size: usize,
        flags: MremapFlags,
        new_addr: Option<Vaddr>,
    ) -> Result<Vaddr> {
        // ---- validate ----
        let old_size = page_aligned_size(old_size, "old_size")?;
        let new_size = page_aligned_size(new_size, "new_size")?;
        if !old_addr.is_multiple_of(PAGE_SIZE) {
            crate::return_errno!(EINVAL, "the old address {old_addr:#x} is not page-aligned");
        }
        if let Some(new_addr) = new_addr
            && !new_addr.is_multiple_of(PAGE_SIZE)
        {
            crate::return_errno!(EINVAL, "the new address {new_addr:#x} is not page-aligned");
        }

        let fixed = flags.contains(MremapFlags::FIXED);
        let keep_old = flags.contains(MremapFlags::DONTUNMAP);
        if fixed && new_addr.is_none() {
            crate::return_errno!(EINVAL, "MREMAP_FIXED requires a new address");
        }
        if keep_old && fixed {
            // Linux rejects this pair: `MREMAP_FIXED` says the old range is
            // replaced, `MREMAP_DONTUNMAP` says it is kept.
            crate::return_errno!(EINVAL, "MREMAP_DONTUNMAP cannot be combined with MREMAP_FIXED");
        }
        if !flags.contains(MremapFlags::MAYMOVE) && new_size > old_size {
            crate::return_errno!(
                EINVAL,
                "growing from {old_size} to {new_size} bytes requires MREMAP_MAYMOVE"
            );
        }
        if keep_old && new_size < old_size {
            // Keeping the old range while making the new one shorter cannot both
            // hold: the old range is `old_size` bytes and stays that way.
            crate::return_errno!(
                EINVAL,
                "MREMAP_DONTUNMAP cannot shrink {old_size} to {new_size} bytes"
            );
        }

        // A destination is only honoured together with `MREMAP_FIXED`.
        let new_addr = if fixed { new_addr } else { None };

        let old_range = old_addr..old_addr + old_size;
        if !is_mappable_range(&old_range) {
            crate::return_errno!(EFAULT, "the old range {old_range:#x?} is not in user space");
        }
        if !self.is_fully_mapped(&old_range) {
            crate::return_errno!(EFAULT, "the old range {old_range:#x?} is not fully mapped");
        }
        if let Some(new_addr) = new_addr {
            let new_range = new_addr..new_addr + new_size;
            if !is_mappable_range(&new_range) {
                crate::return_errno!(ENOMEM, "the new range {new_range:#x?} is not in user space");
            }
            if is_intersected(&old_range, &new_range) {
                crate::return_errno!(
                    EINVAL,
                    "the new range {new_range:#x?} overlaps the old one {old_range:#x?}"
                );
            }
        }

        let mut inner = self.inner.write();
        // Where the address space ends up is the difference between the old and
        // the new size, which may be negative, so the total is assigned rather
        // than adjusted.
        let before = inner.total_vm;

        if keep_old {
            // Without `MREMAP_FIXED` the destination is the kernel's choice,
            // exactly as it is for an ordinary move.
            let new_start = match new_addr {
                Some(new_start) => new_start,
                None => inner
                    .find_free_region(new_size, inner.default_high_limit())?
                    .start,
            };
            return Self::remap_keep_old(self, &mut inner, &old_range, new_start, new_size);
        }

        // A shrinking move releases the tail first, so that the rest of the
        // operation only has to deal with the part that survives.
        if new_size < old_size {
            inner.unmap_range(&self.vm_space, &(old_addr + new_size..old_addr + old_size));
        }
        let kept = old_size.min(new_size);

        // Shrinking is always done where the mapping already is; there is no
        // reason to move a mapping to make it smaller.
        if new_addr.is_none() && new_size < old_size {
            inner.total_vm = before - old_size + new_size;
            return Ok(old_addr);
        }

        // Growing in place is the common case and costs only a record edit, so
        // it is tried before looking for a new home.
        if new_addr.is_none() && new_size > old_size {
            let grow = old_addr + old_size..old_addr + new_size;
            let usable = is_mappable_range(&grow) && inner.mappings.count_overlap(&grow) == 0;
            if usable
                && let Some(key) = inner.mappings.get(old_range.end - 1).map(VmMapping::start)
            {
                inner.check_total_fits(self, before - old_size + new_size)?;
                let mapping = inner
                    .mappings
                    .remove(key)
                    .expect("a key came from this very set");
                inner.mappings.insert(mapping.enlarge(new_size - old_size));
                inner.total_vm = before - old_size + new_size;
                return Ok(old_addr);
            }
        }

        let new_start = match new_addr {
            Some(new_addr) => new_addr,
            None => inner.find_free_region(new_size, inner.default_high_limit())?.start,
        };
        let new_range = new_start..new_start + new_size;
        inner.check_total_fits(self, before - old_size + new_size)?;
        // Replacement of whatever sits at the destination.
        inner.unmap_range(&self.vm_space, &new_range);

        let moved = old_range.start..old_range.start + kept;
        self.move_pt(&moved, &(new_start..new_start + kept));

        let grow_by = new_size - kept;
        let grown_end = new_start + kept;
        inner.carve(&moved, |piece, sub| {
            let delta = sub.start - moved.start;
            let mut piece = piece.relocate(new_start + delta);
            piece.rebase(delta);
            if piece.end() == grown_end {
                piece = piece.enlarge(grow_by);
            }
            Some(piece)
        });
        inner.total_vm = before - old_size + new_size;
        Ok(new_start)
    }

    /// Implements `MREMAP_DONTUNMAP`: a second record is created for the new
    /// range over the same physical pages, and the old record is left untouched.
    fn remap_keep_old(
        vmar: &Vmar,
        inner: &mut VmarInner,
        old_range: &Range<Vaddr>,
        new_start: Vaddr,
        new_size: usize,
    ) -> Result<Vaddr> {
        // A single record has to cover the old range, otherwise one new record
        // could not describe the same pages and the same backing.
        let source = inner
            .mappings
            .get(old_range.start)
            .map(VmMapping::dup)
            .ok_or(crate::errno::Errno::EFAULT)?;
        if source.end() < old_range.end {
            crate::return_errno!(
                EFAULT,
                "the old range {old_range:#x?} spans more than one mapping, which MREMAP_DONTUNMAP cannot duplicate"
            );
        }

        let new_range = new_start..new_start + new_size;
        if !is_mappable_range(&new_range) {
            crate::return_errno!(ENOMEM, "the new range {new_range:#x?} is not in user space");
        }
        // The old range stays exactly as it was, so the address space grows by
        // the whole of the new one.
        let before = inner.total_vm;
        inner.check_total_fits(vmar, before + new_size)?;
        // The destination never overlaps the old range, which the caller
        // checked; this only matters if a caller ever passes one in explicitly.
        inner.unmap_range(&vmar.vm_space, &new_range);

        // The pages stay in the old range; the new range gets the same frames.
        // One cursor has to span both, because the walk steps back and forth
        // between them and a cursor may not be moved outside the range it locks.
        {
            let span = old_range.start.min(new_start)..old_range.end.max(new_range.end);
            let guard = ostd::task::disable_preempt();
            let mut cursor = vmar
                .vm_space
                .cursor_mut(&guard, &span)
                .expect("the ranges lie inside the VM space");
            let mut offset = 0;
            while offset < old_range.len() {
                // `map` leaves the cursor at the destination, so the walk has to
                // be restarted from the source every time.
                cursor
                    .jump(old_range.start + offset)
                    .expect("the old range is page-aligned");
                let Some(mapped) = cursor.find_next(old_range.len() - offset) else {
                    break;
                };
                let (_, Some(item)) = cursor.query().expect("a mapped page was just found") else {
                    break;
                };
                let at_new = new_start + (mapped - old_range.start);
                match item {
                    VmQueriedItem::MappedRam { frame, prop } => {
                        let frame = (*frame).clone();
                        cursor.jump(at_new).expect("the new range is page-aligned");
                        cursor.map(frame, prop);
                    }
                    VmQueriedItem::MappedIoMem { .. } => {
                        unreachable!("device mappings are not supported")
                    }
                }
                offset = (mapped - old_range.start) + PAGE_SIZE;
            }
            cursor.flusher().dispatch_tlb_flush();
            cursor.flusher().sync_tlb_flush();
        }

        // A record for the new range: same properties, and the same backing
        // offsets, so both ranges read and write the same object bytes.
        let mapping = source
            .relocate(new_start)
            .enlarge(new_size - old_range.len());
        inner.mappings.insert(mapping);
        inner.total_vm = before + new_size;
        Ok(new_start)
    }

    /// Moves the page table entries of `old_range` to `new_range`, which must
    /// have the same length and must not overlap it.
    ///
    /// The records are moved separately by the caller, through
    /// [`VmarInner::carve`]. Pages that are not resident simply stay absent at
    /// the destination and are faulted in there.
    fn move_pt(&self, old_range: &Range<Vaddr>, new_range: &Range<Vaddr>) {
        debug_assert_eq!(old_range.len(), new_range.len());
        debug_assert!(!is_intersected(old_range, new_range));

        let guard = ostd::task::disable_preempt();
        let span = old_range.start.min(new_range.start)..old_range.end.max(new_range.end);
        let mut cursor = self
            .vm_space
            .cursor_mut(&guard, &span)
            .expect("the ranges lie inside the VM space");

        let mut offset = 0;
        while offset < old_range.len() {
            cursor
                .jump(old_range.start + offset)
                .expect("the old range is page-aligned");
            let Some(mapped) = cursor.find_next(old_range.len() - offset) else {
                break;
            };
            let (va, Some(item)) = cursor.query().expect("a mapped page was just found") else {
                break;
            };
            debug_assert_eq!(mapped, va.start);

            let at_new = new_range.start + (mapped - old_range.start);
            match item {
                VmQueriedItem::MappedRam { frame, prop } => {
                    let frame = (*frame).clone();
                    cursor.unmap(PAGE_SIZE);
                    cursor
                        .jump(at_new)
                        .expect("the new range is page-aligned");
                    cursor.map(frame, prop);
                }
                VmQueriedItem::MappedIoMem { .. } => {
                    unreachable!("device mappings are not supported")
                }
            }
            offset = (mapped - old_range.start) + PAGE_SIZE;
        }

        cursor.flusher().dispatch_tlb_flush();
        cursor.flusher().sync_tlb_flush();
    }

    /// Resizes the mapping that covers `addr..addr + old_size` to `new_size`
    /// bytes, which is how the address space break of `sbrk(3)` moves.
    ///
    /// The range must lie inside a single mapping: growing it in several steps
    /// would let another mapping appear in between.
    pub fn resize_mapping(&self, addr: Vaddr, old_size: usize, new_size: usize) -> Result<()> {
        let old_size = page_aligned_size(old_size, "old_size")?;
        let new_size = page_aligned_size(new_size, "new_size")?;
        let old_range = addr..addr + old_size;
        check_page_aligned_range(&old_range)?;
        check_page_aligned_range(&(addr..addr + new_size))?;

        let mut inner = self.inner.write();

        // The whole range has to be inside one record, not merely mapped.
        let owner = inner
            .mappings
            .get(addr)
            .map(|mapping| (mapping.start(), mapping.end()))
            .ok_or(crate::errno::Errno::EFAULT)?;
        if owner.1 < old_range.end {
            crate::return_errno!(
                EFAULT,
                "the range {old_range:#x?} spans more than one mapping"
            );
        }

        if new_size == old_size {
            return Ok(());
        }
        if new_size < old_size {
            inner.unmap_range(&self.vm_space, &(addr + new_size..old_range.end));
            return Ok(());
        }

        let grow = old_range.end..addr + new_size;
        if !is_mappable_range(&grow) {
            crate::return_errno!(ENOMEM, "the range {grow:#x?} is not in user space");
        }
        if inner.mappings.count_overlap(&grow) != 0 {
            crate::return_errno!(EFAULT, "the range {grow:#x?} is already mapped");
        }
        inner.check_total_fits(self, inner.total_vm + (new_size - old_size))?;

        let mapping = inner
            .mappings
            .remove(owner.0)
            .expect("the owner was just found");
        inner.mappings.insert(mapping.enlarge(new_size - old_size));
        inner.total_vm += new_size - old_size;
        Ok(())
    }
}

/// Rounds a length up to whole pages, rejecting zero.
fn page_aligned_size(size: usize, what: &str) -> Result<usize> {
    if size == 0 {
        crate::return_errno!(EINVAL, "{what} must not be zero");
    }
    match size.checked_next_multiple_of(PAGE_SIZE) {
        Some(size) => Ok(size),
        None => crate::return_errno!(ENOMEM, "{what} of {size} bytes overflows"),
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        errno::Errno,
        vm::{flags::MmapFlags, perms::VmPerms},
    };

    const AT: Vaddr = 0x4000_0000;
    const TO: Vaddr = 0x5000_0000;

    fn vmar_with(pages: usize) -> Arc<super::super::Vmar> {
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(AT, pages * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        vmar
    }

    fn flags() -> MremapFlags {
        MremapFlags::MAYMOVE
    }

    #[ktest]
    fn growing_keeps_the_address() {
        let vmar = vmar_with(1);
        let moved = vmar
            .mremap(AT, PAGE_SIZE, 4 * PAGE_SIZE, flags(), None)
            .unwrap();
        assert_eq!(moved, AT);
        assert!(vmar.is_fully_mapped(&(AT..AT + 4 * PAGE_SIZE)));
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);
    }

    #[ktest]
    fn shrinking_keeps_the_address_and_drops_the_tail() {
        let vmar = vmar_with(4);
        let moved = vmar
            .mremap(AT, 4 * PAGE_SIZE, PAGE_SIZE, flags(), None)
            .unwrap();
        assert_eq!(moved, AT);
        assert_eq!(vmar.total_mapped_size(), PAGE_SIZE);
        assert!(!vmar.is_fully_mapped(&(AT + PAGE_SIZE..AT + 4 * PAGE_SIZE)));
    }

    #[ktest]
    fn fixed_moves_the_mapping_and_frees_the_old_range() {
        let vmar = vmar_with(2);
        let moved = vmar
            .mremap(
                AT,
                2 * PAGE_SIZE,
                2 * PAGE_SIZE,
                flags() | MremapFlags::FIXED,
                Some(TO),
            )
            .unwrap();
        assert_eq!(moved, TO);
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
        assert!(vmar.is_fully_mapped(&(TO..TO + 2 * PAGE_SIZE)));
        assert!(!vmar.is_fully_mapped(&(AT..AT + 2 * PAGE_SIZE)));
    }

    #[ktest]
    fn dontunmap_leaves_both_ranges_aliasing() {
        let vmar = vmar_with(1);
        // `MREMAP_DONTUNMAP` may not be combined with `MREMAP_FIXED`, so the
        // destination is chosen by the kernel.
        let moved = vmar
            .mremap(AT, PAGE_SIZE, PAGE_SIZE, flags() | MremapFlags::DONTUNMAP, None)
            .unwrap();
        assert_ne!(moved, AT);
        // Both ranges are still mapped, and the total counts both.
        assert!(vmar.is_fully_mapped(&(AT..AT + PAGE_SIZE)));
        assert!(vmar.is_fully_mapped(&(moved..moved + PAGE_SIZE)));
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);

        // A second call must not fail now that the space is used twice over.
        let again = vmar
            .mremap(AT, PAGE_SIZE, PAGE_SIZE, flags() | MremapFlags::DONTUNMAP, None)
            .unwrap();
        assert_ne!(again, moved);
        assert_eq!(vmar.total_mapped_size(), 3 * PAGE_SIZE);
    }

    #[ktest]
    fn maymove_finds_a_free_address_when_growth_is_impossible() {
        let vmar = vmar_with(1);
        // Occupy the space right above, so growing in place is not possible.
        vmar.mmap_anonymous(AT + PAGE_SIZE, 4 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        let moved = vmar
            .mremap(AT, PAGE_SIZE, 4 * PAGE_SIZE, flags(), None)
            .unwrap();
        assert_ne!(moved, AT);
        assert!(vmar.is_fully_mapped(&(moved..moved + 4 * PAGE_SIZE)));
        assert_eq!(vmar.total_mapped_size(), 8 * PAGE_SIZE);
    }

    #[ktest]
    fn bad_arguments_are_rejected() {
        let vmar = vmar_with(1);
        let fixed = flags() | MremapFlags::FIXED;

        assert_eq!(vmar.mremap(AT, 0, PAGE_SIZE, flags(), None).unwrap_err(), Errno::EINVAL);
        assert_eq!(vmar.mremap(AT, PAGE_SIZE, 0, flags(), None).unwrap_err(), Errno::EINVAL);
        assert_eq!(
            vmar.mremap(AT + 8, PAGE_SIZE, PAGE_SIZE, flags(), None).unwrap_err(),
            Errno::EINVAL
        );
        // MREMAP_FIXED without a destination.
        assert_eq!(
            vmar.mremap(AT, PAGE_SIZE, PAGE_SIZE, fixed, None).unwrap_err(),
            Errno::EINVAL
        );
        // MREMAP_DONTUNMAP together with MREMAP_FIXED.
        assert_eq!(
            vmar.mremap(
                AT,
                PAGE_SIZE,
                PAGE_SIZE,
                flags() | MremapFlags::DONTUNMAP | MremapFlags::FIXED,
                Some(TO)
            )
            .unwrap_err(),
            Errno::EINVAL
        );
        // Growing without MREMAP_MAYMOVE.
        assert_eq!(
            vmar.mremap(AT, PAGE_SIZE, 2 * PAGE_SIZE, MremapFlags::empty(), None).unwrap_err(),
            Errno::EINVAL
        );
        // MREMAP_DONTUNMAP cannot shrink, since the old range stays as it was.
        let two = vmar_with(2);
        assert_eq!(
            two.mremap(AT, 2 * PAGE_SIZE, PAGE_SIZE, flags() | MremapFlags::DONTUNMAP, None)
                .unwrap_err(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn an_unmapped_or_overlapping_range_is_rejected() {
        let vmar = vmar_with(2);
        // Nothing is mapped at AT + 64 MiB.
        assert_eq!(
            vmar.mremap(AT + 64 * 1024 * 1024, PAGE_SIZE, PAGE_SIZE, flags(), None)
                .unwrap_err(),
            Errno::EFAULT
        );
    }

    #[ktest]
    fn an_overlapping_destination_is_rejected() {
        let vmar = vmar_with(2);
        let err = vmar
            .mremap(
                AT,
                2 * PAGE_SIZE,
                2 * PAGE_SIZE,
                flags() | MremapFlags::FIXED,
                Some(AT + PAGE_SIZE),
            )
            .unwrap_err();
        ostd::warn!("BREADCRUMB got {err}");
        assert_eq!(err, Errno::EINVAL);
    }

    #[ktest]
    fn resize_mapping_needs_one_mapping_and_a_free_tail() {
        let vmar = Vmar::new();
        vmar.mmap_anonymous(AT, 2 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        vmar.resize_mapping(AT, 2 * PAGE_SIZE, 4 * PAGE_SIZE).unwrap();
        assert!(vmar.is_fully_mapped(&(AT..AT + 4 * PAGE_SIZE)));
        assert_eq!(vmar.total_mapped_size(), 4 * PAGE_SIZE);

        // Growing further needs the space above to be free, and it is.
        vmar.resize_mapping(AT, 4 * PAGE_SIZE, 8 * PAGE_SIZE).unwrap();
        assert_eq!(vmar.total_mapped_size(), 8 * PAGE_SIZE);

        // With something sitting on the tail, growing fails.
        let blocked = Vmar::new();
        blocked.mmap_anonymous(AT, 2 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        blocked.mmap_anonymous(AT + 4 * PAGE_SIZE, PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        assert_eq!(
            blocked.resize_mapping(AT, 2 * PAGE_SIZE, 5 * PAGE_SIZE).unwrap_err(),
            Errno::EFAULT
        );
        assert_eq!(blocked.total_mapped_size(), 3 * PAGE_SIZE);

        // Shrinking always works.
        blocked.resize_mapping(AT, 2 * PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(blocked.total_mapped_size(), 2 * PAGE_SIZE);
    }
}

/// Tests that a move relocates the *pages*, not merely the records.
///
/// Everything above only inspects the record bookkeeping, which is easy to get
/// right while leaving the page table untouched. These tests therefore look at
/// the physical address behind each virtual address, before and after.
#[cfg(ktest)]
mod residency {
    use alloc::{sync::Arc, vec::Vec};

    use ostd::mm::{Paddr, PAGE_SIZE};
    use ostd::prelude::ktest;

    use super::*;
    use crate::vm::{
        flags::MmapFlags,
        perms::VmPerms,
        tests::{not_resident, paddr_at, page_at, poke, touch},
    };

    const AT: Vaddr = 0x4000_0000;
    const TO: Vaddr = 0x5000_0000;

    fn maymove() -> MremapFlags {
        MremapFlags::MAYMOVE
    }

    /// A space with `count` read-write pages at [`AT`], every one resident and
    /// holding a distinct value.
    fn resident(count: usize) -> Arc<super::super::Vmar> {
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(
            AT,
            count * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE,
        )
        .unwrap();
        for index in 0..count {
            poke(&vmar, AT + index * PAGE_SIZE, 0x1000 + index as u64);
        }
        vmar
    }

    /// Records the physical address of every resident page of `vmar` over
    /// `base..base + count` pages.
    fn frames_of(vmar: &super::super::Vmar, base: Vaddr, count: usize) -> Vec<Option<Paddr>> {
        (0..count)
            .map(|index| page_at(vmar, base + index * PAGE_SIZE).map(|(paddr, _)| paddr))
            .collect()
    }

    #[ktest]
    fn a_move_relocates_the_pages() {
        let vmar = resident(4);
        let before = frames_of(&vmar, AT, 4);
        assert!(before.iter().all(Option::is_some));

        let moved = vmar.mremap(AT, 4 * PAGE_SIZE, 4 * PAGE_SIZE, maymove(), None).unwrap();
        assert_ne!(moved, AT);

        // The pages are at the new address, in the same order...
        assert_eq!(frames_of(&vmar, moved, 4), before);
        // ...and the old address is empty.
        assert!(frames_of(&vmar, AT, 4).iter().all(Option::is_none));
    }

    #[ktest]
    fn a_move_keeps_the_contents_readable() {
        let vmar = resident(4);
        let moved = vmar.mremap(AT, 4 * PAGE_SIZE, 4 * PAGE_SIZE, maymove(), None).unwrap();
        for index in 0..4usize {
            let expected = 0x1000 + index as u64;
            assert_eq!(crate::vm::tests::peek(&vmar, moved + index * PAGE_SIZE), expected);
        }
    }

    #[ktest]
    fn a_move_preserves_which_pages_were_resident() {
        // Fault in pages 0, 1 and 3 of 4 and leave page 2 out, so that the walk
        // has to skip a hole rather than assuming a contiguous run.
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(
            AT,
            4 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        for index in [0usize, 1, 3] {
            poke(&vmar, AT + index * PAGE_SIZE, index as u64);
        }
        let before = frames_of(&vmar, AT, 4);
        assert!(before[2].is_none());

        let moved = vmar.mremap(AT, 4 * PAGE_SIZE, 4 * PAGE_SIZE, maymove(), None).unwrap();
        assert_eq!(frames_of(&vmar, moved, 4), before);
    }

    #[ktest]
    fn a_move_upwards_and_downwards_agree() {
        // Upwards: the destination is above the source.
        let up = resident(4);
        let before = frames_of(&up, AT, 4);
        let moved = up.mremap(AT, 4 * PAGE_SIZE, 4 * PAGE_SIZE, maymove(), None).unwrap();
        assert!(moved > AT);
        assert_eq!(frames_of(&up, moved, 4), before);
        assert!(frames_of(&up, AT, 4).iter().all(Option::is_none));

        // Downwards: the destination is below the source, which is the case
        // where a stale cursor position would move the wrong page.
        let down = resident(4);
        let before = frames_of(&down, AT, 4);
        let target = AT - 64 * PAGE_SIZE;
        let moved = down
            .mremap(
                AT,
                4 * PAGE_SIZE,
                4 * PAGE_SIZE,
                maymove() | MremapFlags::FIXED,
                Some(target),
            )
            .unwrap();
        assert_eq!(moved, target);
        assert!(target < AT);
        assert_eq!(frames_of(&down, moved, 4), before);
        assert!(frames_of(&down, AT, 4).iter().all(Option::is_none));
    }

    #[ktest]
    fn a_move_downwards_into_the_page_below_it_works() {
        // The destination ends exactly where the source begins, so the two share
        // a boundary page-table node.
        let vmar = resident(2);
        let before = frames_of(&vmar, AT, 2);
        let target = AT - 2 * PAGE_SIZE;
        let moved = vmar
            .mremap(
                AT,
                2 * PAGE_SIZE,
                2 * PAGE_SIZE,
                maymove() | MremapFlags::FIXED,
                Some(target),
            )
            .unwrap();
        assert_eq!(moved, target);
        assert_eq!(frames_of(&vmar, moved, 2), before);
    }

    #[ktest]
    fn a_move_onto_an_occupied_range_replaces_it() {
        let vmar = resident(4);
        // Fill the destination with pages of its own. It has to be placed
        // explicitly, because a zero hint would let the kernel choose.
        vmar.mmap_anonymous(
            TO,
            2 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        poke(&vmar, TO, 0xdead);
        poke(&vmar, TO + PAGE_SIZE, 0xbeef);
        let displaced = [paddr_at(&vmar, TO), paddr_at(&vmar, TO + PAGE_SIZE)];

        let before = frames_of(&vmar, AT, 4);
        let moved = vmar
            .mremap(
                AT,
                4 * PAGE_SIZE,
                2 * PAGE_SIZE,
                maymove() | MremapFlags::FIXED,
                Some(TO),
            )
            .unwrap();
        assert_eq!(moved, TO);

        // The first two pages of the source are now at the destination...
        let dest = frames_of(&vmar, TO, 2);
        assert_eq!(dest, before[..2].to_vec());
        // ...and the pages that were there are gone, not merely unmapped from
        // the record.
        for (index, paddr) in displaced.iter().enumerate() {
            let reused = frames_of(&vmar, TO + index * PAGE_SIZE, 1)[0];
            assert_ne!(reused, Some(*paddr));
        }
        assert!(frames_of(&vmar, AT, 4).iter().all(Option::is_none));
    }

    #[ktest]
    fn a_shrink_keeps_the_surviving_pages() {
        let vmar = resident(4);
        let before = frames_of(&vmar, AT, 4);
        let moved = vmar
            .mremap(AT, 4 * PAGE_SIZE, 2 * PAGE_SIZE, maymove(), None)
            .unwrap();
        assert_eq!(moved, AT);
        // The first two pages keep their frames; the rest are released.
        assert_eq!(frames_of(&vmar, AT, 2), before[..2].to_vec());
        assert!(frames_of(&vmar, AT + 2 * PAGE_SIZE, 2).iter().all(Option::is_none));
        assert_eq!(vmar.total_mapped_size(), 2 * PAGE_SIZE);
    }

    #[ktest]
    fn growing_in_place_leaves_the_pages_alone() {
        let vmar = resident(2);
        let before = frames_of(&vmar, AT, 2);
        let moved = vmar
            .mremap(AT, 2 * PAGE_SIZE, 4 * PAGE_SIZE, maymove(), None)
            .unwrap();
        assert_eq!(moved, AT);
        assert_eq!(frames_of(&vmar, AT, 2), before);
        // The two new pages are not resident until something touches them.
        assert!(not_resident(&vmar, AT + 2 * PAGE_SIZE));
        assert!(not_resident(&vmar, AT + 3 * PAGE_SIZE));
    }

    #[ktest]
    fn dontunmap_makes_both_ranges_alias_the_same_pages() {
        let vmar = resident(3);
        let before = frames_of(&vmar, AT, 3);

        let moved = vmar
            .mremap(AT, 3 * PAGE_SIZE, 3 * PAGE_SIZE, maymove() | MremapFlags::DONTUNMAP, None)
            .unwrap();
        assert_ne!(moved, AT);

        // This is the point of the flag: one physical page, two virtual
        // addresses, in both ranges.
        assert_eq!(frames_of(&vmar, moved, 3), before);
        assert_eq!(frames_of(&vmar, AT, 3), before);
        for index in 0..3usize {
            assert_eq!(paddr_at(&vmar, AT + index * PAGE_SIZE), paddr_at(&vmar, moved + index * PAGE_SIZE));
        }

        // A write through either address is visible through the other.
        poke(&vmar, moved, 0xabcd);
        assert_eq!(crate::vm::tests::peek(&vmar, AT), 0xabcd);
        poke(&vmar, AT, 0xdcba);
        assert_eq!(crate::vm::tests::peek(&vmar, moved), 0xdcba);

        // Both ranges count towards the address space.
        assert_eq!(vmar.total_mapped_size(), 6 * PAGE_SIZE);
    }

    #[ktest]
    fn dontunmap_copies_onto_a_partly_resident_source() {
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(
            AT,
            4 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        for index in [0usize, 2] {
            poke(&vmar, AT + index * PAGE_SIZE, index as u64);
        }
        let before = frames_of(&vmar, AT, 4);

        let moved = vmar
            .mremap(AT, 4 * PAGE_SIZE, 4 * PAGE_SIZE, maymove() | MremapFlags::DONTUNMAP, None)
            .unwrap();
        assert_eq!(frames_of(&vmar, moved, 4), before);
        assert_eq!(frames_of(&vmar, AT, 4), before);
    }

    #[ktest]
    fn a_moved_backed_mapping_still_refers_to_the_same_object_bytes() {
        // A `SharedPages` mapping carries a backing offset that has to move with
        // it; a mistake here would silently shift the whole object.
        let object = crate::vm::shm::SharedPages::new(4 * PAGE_SIZE).unwrap();
        let vmar = super::super::Vmar::new();
        let backing: Arc<dyn crate::vm::backing::Backing> = Arc::clone(&object) as Arc<_>;
        vmar.mmap_backed(
            &backing,
            AT,
            3 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            PAGE_SIZE,
        )
        .unwrap();
        // Write through the mapping at `AT`, which is object offset `PAGE_SIZE`.
        poke(&vmar, AT, 0x5151);
        assert_eq!(object.peek_u64(PAGE_SIZE).unwrap(), 0x5151);
        let before = frames_of(&vmar, AT, 3);

        let moved = vmar.mremap(AT, 3 * PAGE_SIZE, 3 * PAGE_SIZE, maymove(), None).unwrap();
        assert_eq!(frames_of(&vmar, moved, 3), before);

        // The object byte is still the one the new range addresses, so writing
        // there must update the same object offset.
        poke(&vmar, moved, 0x6262);
        assert_eq!(object.peek_u64(PAGE_SIZE).unwrap(), 0x6262);
        // And a different object offset is untouched.
        assert_eq!(object.peek_u64(0).unwrap(), 0);
    }

    #[ktest]
    fn a_shrunk_and_moved_mapping_rebases_only_the_surviving_pages() {
        let object = crate::vm::shm::SharedPages::new(4 * PAGE_SIZE).unwrap();
        let vmar = super::super::Vmar::new();
        let backing: Arc<dyn crate::vm::backing::Backing> = Arc::clone(&object) as Arc<_>;
        // Map object offset `PAGE_SIZE` onwards, so the mapping is smaller than
        // the object and shrinking it must not shift the backing offset.
        vmar.mmap_backed(
            &backing,
            AT,
            2 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::SHARED | MmapFlags::FIXED,
            2 * PAGE_SIZE,
        )
        .unwrap();
        poke(&vmar, AT, 0x1111);
        poke(&vmar, AT + PAGE_SIZE, 0x2222);

        // Shrink and move in one step, keeping the first page. The destination
        // is given explicitly, because a shrink without one shrinks in place.
        let target = AT - 8 * PAGE_SIZE;
        let moved = vmar
            .mremap(
                AT,
                2 * PAGE_SIZE,
                PAGE_SIZE,
                maymove() | MremapFlags::FIXED,
                Some(target),
            )
            .unwrap();
        assert_eq!(moved, target);
        assert_eq!(crate::vm::tests::peek(&vmar, moved), 0x1111);
        assert_eq!(object.peek_u64(2 * PAGE_SIZE).unwrap(), 0x1111);
        assert!(not_resident(&vmar, AT));
    }

    #[ktest]
    fn a_moved_mapping_can_still_be_faulted_afterwards() {
        // A page that was not resident before the move must be faultable at the
        // new address, which only works if the backing offset came along.
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(
            AT,
            2 * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE | MmapFlags::FIXED,
        )
        .unwrap();
        poke(&vmar, AT, 7);
        let moved = vmar.mremap(AT, 2 * PAGE_SIZE, 2 * PAGE_SIZE, maymove(), None).unwrap();

        assert!(not_resident(&vmar, moved + PAGE_SIZE));
        poke(&vmar, moved + PAGE_SIZE, 9);
        assert!(!not_resident(&vmar, moved + PAGE_SIZE));
        assert_eq!(crate::vm::tests::peek(&vmar, moved + PAGE_SIZE), 9);
        // A fresh private page is zero, not the value from before the move.
        assert_eq!(crate::vm::tests::peek(&vmar, moved), 7);
    }

    #[ktest]
    fn touching_a_whole_mapping_makes_every_page_resident() {
        // Sanity check on the helper itself, so a failure elsewhere cannot be
        // blamed on the assertions being vacuous.
        let vmar = super::super::Vmar::new();
        vmar.mmap_anonymous(AT, 4 * PAGE_SIZE, VmPerms::READ, MmapFlags::PRIVATE)
            .unwrap();
        assert!(frames_of(&vmar, AT, 4).iter().all(Option::is_none));
        touch(&vmar, AT..AT + 4 * PAGE_SIZE).unwrap();
        assert!(frames_of(&vmar, AT, 4).iter().all(Option::is_some));
    }
}
