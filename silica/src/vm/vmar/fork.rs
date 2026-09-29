// SPDX-License-Identifier: GPL-2.0

//! `fork(2)`: a new address space that shares the parent's pages until either
//! side writes.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use ostd::mm::{
    CachePolicy, PageFlags, tlb::TlbFlushOp, vm_space::{CursorMut, VmQueriedItem},
};
use ostd::task::disable_preempt;
use super::{Vmar, VMAR_CAP_ADDR};

impl Vmar {
    /// Creates an address space that starts out as a copy-on-write image of
    /// `parent`.
    ///
    /// Every resident page is shared between the two, and both are marked
    /// read-only, so nothing is copied until one of them writes. The pages stay
    /// shared until then, which is also what makes a `MAP_SHARED` mapping stay
    /// shared across a `fork`.
    pub fn fork_from(parent: &Arc<Vmar>) -> Arc<Vmar> {
        let child = Vmar::new();
        child
            .max_addr_space
            .store(parent.max_addr_space().unwrap_or(0), Ordering::Relaxed);

        let guard = disable_preempt();
        // One cursor over the whole parent space, so that a single full TLB
        // flush at the end covers every page that was made read-only.
        let whole = 0..VMAR_CAP_ADDR;
        let Ok(mut parent_cursor) = parent.vm_space().cursor_mut(&guard, &whole) else {
            return child;
        };

        {
            let parent_inner = parent.inner.read();
            let mut child_inner = child.inner.write();

            for mapping in parent_inner.mappings.iter() {
                let range = mapping.range();
                child_inner.total_vm += mapping.len();
                child_inner.mappings.insert(mapping.dup());

                let Ok(mut child_cursor) = child.vm_space().cursor_mut(&guard, &range) else {
                    continue;
                };
                parent_cursor.jump(range.start).expect("page-aligned");
                child_cursor.jump(range.start).expect("page-aligned");
                Self::share_pages(&mut parent_cursor, &mut child_cursor, range.len());
            }
        }

        // Nothing may be written through a COW page once `fork` returns, so the
        // read-only bits have to be visible everywhere before the child runs.
        parent_cursor.flusher().issue_tlb_flush(TlbFlushOp::for_all());
        parent_cursor.flusher().dispatch_tlb_flush();
        parent_cursor.flusher().sync_tlb_flush();
        child
    }

    /// Shares the resident pages of `size` bytes from `src` into `dst`, leaving
    /// `src` read-only, and returns how many pages were shared.
    fn share_pages(src: &mut CursorMut<'_>, dst: &mut CursorMut<'_>, size: usize) -> usize {
        let end = src.virt_addr() + size;
        let mut shared = 0;

        while let Some(mapped) = src.find_next(end - src.virt_addr()) {
            let (_, Some(item)) = src.query().expect("a mapped page was just found") else {
                break;
            };
            match item {
                VmQueriedItem::MappedRam { frame, mut prop } => {
                    let frame = (*frame).clone();

                    // `protect_next` also advances the source cursor past the
                    // page it changed.
                    src.protect_next(end - mapped, make_read_only)
                        .expect("a mapped page was just found");

                    dst.jump(mapped).expect("page-aligned");
                    make_read_only(&mut prop.flags, &mut prop.cache);
                    dst.map(frame, prop);
                    shared += 1;
                }
                VmQueriedItem::MappedIoMem { .. } => {
                    unreachable!("device mappings are not supported")
                }
            }
        }
        shared
    }
}

/// Clears the write bit, which is what turns a shared page into a COW one.
fn make_read_only(flags: &mut PageFlags, _cache: &mut CachePolicy) {
    *flags -= PageFlags::W;
}

#[cfg(ktest)]
mod tests {
    use ostd::mm::{PAGE_SIZE, Vaddr};
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        vm::{
            flags::MmapFlags,
            perms::VmPerms,
            tests::{peek, poke, touch},
        },
    };

    const AT: Vaddr = 0x4000_0000;

    fn parent_with(pages: usize) -> Arc<Vmar> {
        let vmar = Vmar::new();
        vmar.mmap_anonymous(
            AT,
            pages * PAGE_SIZE,
            VmPerms::READ | VmPerms::WRITE,
            MmapFlags::PRIVATE,
        )
        .unwrap();
        // The pages have to be resident before the fork, otherwise there is
        // nothing to share and nothing to copy.
        touch(&vmar, AT..AT + pages * PAGE_SIZE).unwrap();
        vmar
    }

    #[ktest]
    fn the_child_inherits_the_contents() {
        let parent = parent_with(1);
        poke(&parent, AT, 0x1234_5678);
        let child = Vmar::fork_from(&parent);

        assert_eq!(peek(&child, AT), 0x1234_5678);
        assert_eq!(child.total_mapped_size(), parent.total_mapped_size());
    }

    #[ktest]
    fn a_write_in_one_side_does_not_reach_the_other() {
        let parent = parent_with(1);
        poke(&parent, AT, 0xaaaa_aaaa);
        let child = Vmar::fork_from(&parent);

        // The parent writes: only its own copy may change.
        poke(&parent, AT, 0xbbbb_bbbb);
        assert_eq!(peek(&parent, AT), 0xbbbb_bbbb);
        assert_eq!(peek(&child, AT), 0xaaaa_aaaa);

        // And now the other way round, on a second word of the same page, so
        // that the child copies the page it shares with the parent.
        poke(&parent, AT + 8, 1);
        poke(&child, AT + 8, 2);
        assert_eq!(peek(&parent, AT + 8), 1);
        assert_eq!(peek(&child, AT + 8), 2);
        // The word nobody wrote to is still the original.
        assert_eq!(peek(&child, AT), 0xaaaa_aaaa);
    }

    #[ktest]
    fn unmapped_pages_are_not_shared() {
        let parent = Vmar::new();
        parent
            .mmap_anonymous(
                AT,
                4 * PAGE_SIZE,
                VmPerms::READ | VmPerms::WRITE,
                MmapFlags::PRIVATE,
            )
            .unwrap();
        // Only the first page is resident.
        touch(&parent, AT..AT + PAGE_SIZE).unwrap();
        poke(&parent, AT, 7);

        let child = Vmar::fork_from(&parent);
        // The resident page is shared...
        assert_eq!(peek(&child, AT), 7);
        // ...and the rest is faulted in separately on each side.
        poke(&child, AT + 3 * PAGE_SIZE, 9);
        assert_eq!(peek(&child, AT + 3 * PAGE_SIZE), 9);
        poke(&parent, AT + 3 * PAGE_SIZE, 10);
        assert_eq!(peek(&parent, AT + 3 * PAGE_SIZE), 10);
        assert_eq!(peek(&child, AT + 3 * PAGE_SIZE), 9);
    }

    #[ktest]
    fn a_shared_mapping_stays_shared_across_a_fork() {
        let parent = Vmar::new();
        parent
            .mmap_anonymous(
                AT,
                PAGE_SIZE,
                VmPerms::READ | VmPerms::WRITE,
                MmapFlags::SHARED,
            )
            .unwrap();
        touch(&parent, AT..AT + PAGE_SIZE).unwrap();
        poke(&parent, AT, 0x5555);

        let child = Vmar::fork_from(&parent);
        // A shared mapping is not copied, so a write in the child is a write
        // in the parent too.
        poke(&child, AT, 0x6666);
        assert_eq!(peek(&parent, AT), 0x6666);
    }
}
