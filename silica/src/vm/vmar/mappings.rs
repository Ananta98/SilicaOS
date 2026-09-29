// SPDX-License-Identifier: GPL-2.0

//! The mappings of a virtual address space.
//!
//! A [`VmMapping`] is the record of one contiguous run of virtual addresses
//! with a single set of properties. A [`Mappings`] holds those records ordered
//! by start address, guaranteed to be pairwise non-overlapping, and is the only
//! place that maintains that guarantee.
//!
//! # Why one method does almost everything
//!
//! `munmap`, `mprotect`, `mremap` and a `MAP_FIXED` `mmap` all do the same
//! three-step dance: pull out the records that intersect a range, split each of
//! them so the range is covered exactly, and put back whatever should survive.
//! [`Mappings::carve`] is that dance, once. Each caller becomes a single
//! closure saying what happens to the part that is being changed, which is why
//! the three operations are a handful of lines each and stay in step.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{num::NonZeroUsize, ops::Range};

use ostd::mm::{
    CachePolicy, PAGE_SIZE, PageFlags, PageProperty, Vaddr, VmSpace, tlb::TlbFlushOp,
};
use ostd::task::disable_preempt;

use crate::vm::{backing::Backing, perms::VmPerms};

use super::intersect;

/// One contiguous run of virtual addresses sharing a set of properties.
///
/// A `VmMapping` is a linear value: it is never dropped while it still owns
/// page table entries, because dropping it would silently leak them. Callers
/// therefore hand ownership on deliberately — [`Self::unmap`] releases the
/// pages, and [`Mappings`] releases the record when a piece is discarded.
pub struct VmMapping {
    /// The first virtual address of the mapping.
    start: Vaddr,
    /// The length in bytes. Always non-zero, so the record can never represent
    /// an empty range.
    len: NonZeroUsize,
    /// The permissions in force, together with the ceiling `mprotect` may not
    /// exceed.
    perms: VmPerms,
    /// Whether writes are carried through to the backing object and are visible
    /// to every other mapping of it.
    shared: bool,
    /// The object supplying the pages, or `None` for `MAP_PRIVATE` anonymous
    /// memory, where every fault gets a fresh zeroed page.
    backing: Option<Arc<dyn Backing>>,
    /// The offset within the backing object that `start` maps to.
    ///
    /// A mapping may be larger than its backing, in which case the tail has no
    /// backing and faults give private zeroed pages.
    backing_offset: usize,
}

impl VmMapping {
    /// Creates a record. `len` must be non-zero and a multiple of the page size.
    pub(super) fn new(
        start: Vaddr,
        len: NonZeroUsize,
        perms: VmPerms,
        shared: bool,
        backing: Option<Arc<dyn Backing>>,
        backing_offset: usize,
    ) -> Self {
        debug_assert!(start.is_multiple_of(PAGE_SIZE));
        debug_assert!(len.get().is_multiple_of(PAGE_SIZE));
        debug_assert!(backing_offset.is_multiple_of(PAGE_SIZE));
        Self {
            start,
            len,
            perms,
            shared,
            backing,
            backing_offset,
        }
    }

    /// Returns the virtual address range this record covers.
    pub fn range(&self) -> Range<Vaddr> {
        self.start..self.end()
    }

    /// Returns the first address past the end of this mapping.
    pub fn end(&self) -> Vaddr {
        self.start + self.len.get()
    }

    /// Returns the length in bytes.
    ///
    /// A mapping is never empty, so there is deliberately no `is_empty`.
    #[expect(clippy::len_without_is_empty, reason = "a mapping is never empty")]
    pub fn len(&self) -> usize {
        self.len.get()
    }

    /// Returns the start address.
    pub fn start(&self) -> Vaddr {
        self.start
    }

    /// Returns the permissions of this mapping.
    pub fn perms(&self) -> VmPerms {
        self.perms
    }

    /// Replaces the permissions in force, keeping the ceiling in place.
    pub(super) fn set_perms(&mut self, perms: VmPerms) {
        self.perms = perms;
    }

    /// Returns whether this mapping is shared with other mappings of its
    /// backing object.
    pub fn is_shared(&self) -> bool {
        self.shared
    }

    /// Returns the object supplying the pages, if any.
    pub fn backing(&self) -> Option<&Arc<dyn Backing>> {
        self.backing.as_ref()
    }

    /// Returns the offset within the backing object that `start` maps to.
    pub fn backing_offset(&self) -> usize {
        self.backing_offset
    }

    /// Returns whether the offset lies within the backing object.
    ///
    /// Addresses past the end of the object are mapped, but have no backing, so
    /// a fault there produces a fresh private page instead of touching the
    /// object.
    pub fn is_backed(&self, offset_in_mapping: usize) -> bool {
        match &self.backing {
            Some(backing) => self.backing_offset + offset_in_mapping < backing.size(),
            None => false,
        }
    }

    /// Returns the offset in the backing object that the address
    /// `start + offset_in_mapping` maps to.
    pub(super) fn backing_offset_of(&self, offset_in_mapping: usize) -> usize {
        self.backing_offset + offset_in_mapping
    }

    /// Returns whether a write to this mapping must copy the page first.
    ///
    /// A shared mapping is never copied: the whole point is that the write
    /// reaches the other mappings.
    pub fn is_cow(&self) -> bool {
        !self.shared
    }

    /// Returns the page table flags that this mapping's permissions imply.
    pub fn page_flags(&self) -> PageFlags {
        PageFlags::from(self.perms)
    }

    /// Grows the mapping by `extra` bytes at its high end.
    pub(super) fn enlarge(self, extra: usize) -> Self {
        debug_assert!(extra.is_multiple_of(PAGE_SIZE));
        Self {
            len: NonZeroUsize::new(self.len.get() + extra).expect("a mapping is never empty"),
            ..self
        }
    }

    /// Moves the record to `new_start`, keeping its properties and backing.
    ///
    /// The page table entries have to be moved separately; see
    /// `Vmar::move_pt`.
    pub(super) fn relocate(self, new_start: Vaddr) -> Self {
        debug_assert!(new_start.is_multiple_of(PAGE_SIZE));
        Self { start: new_start, ..self }
    }

    /// Shifts the backing offset by `delta` bytes.
    pub(super) fn rebase(&mut self, delta: usize) {
        debug_assert!(delta.is_multiple_of(PAGE_SIZE));
        self.backing_offset += delta;
    }

    /// Returns an independent record with the same properties.
    pub(crate) fn dup(&self) -> Self {
        Self {
            start: self.start,
            len: self.len,
            perms: self.perms,
            shared: self.shared,
            backing: self.backing.clone(),
            backing_offset: self.backing_offset,
        }
    }

    /// Splits the record at `at`, which must be strictly inside it.
    pub(super) fn split(self, at: Vaddr) -> (Self, Self) {
        debug_assert!(self.start < at && at < self.end());
        debug_assert!(at.is_multiple_of(PAGE_SIZE));

        let Self {
            start,
            len,
            perms,
            shared,
            backing,
            backing_offset,
        } = self;
        let left_len = at - start;

        let left = Self {
            start,
            len: NonZeroUsize::new(left_len).expect("split leaves a non-empty side"),
            perms,
            shared,
            backing: backing.clone(),
            backing_offset,
        };
        let right = Self {
            start: at,
            len: NonZeroUsize::new(len.get() - left_len).expect("split leaves a non-empty side"),
            perms,
            shared,
            backing,
            backing_offset: backing_offset + left_len,
        };
        (left, right)
    }

    /// Splits the record so that `sub` is covered by exactly the middle result.
    ///
    /// Returns `(outside-left, within, outside-right)`, either side being
    /// `None` when the record ends there.
    pub(super) fn split_range(self, sub: &Range<Vaddr>) -> (Option<Self>, Self, Option<Self>) {
        let mine = self.range();
        assert!(
            mine.start <= sub.start && sub.end <= mine.end,
            "the splitting range must lie inside the mapping"
        );

        if sub.start == mine.start && sub.end == mine.end {
            (None, self, None)
        } else if mine.start < sub.start {
            let (left, rest) = self.split(sub.start);
            if sub.end < mine.end {
                let (middle, right) = rest.split(sub.end);
                (Some(left), middle, Some(right))
            } else {
                (Some(left), rest, None)
            }
        } else if sub.end < mine.end {
            let (middle, right) = self.split(sub.end);
            (None, middle, Some(right))
        } else {
            (None, self, None)
        }
    }

    /// Returns whether `left` and `right` are adjacent records that may be
    /// represented by a single one.
    ///
    /// Two records merge when they are contiguous in the address space and
    /// indistinguishable in every other respect, so that the set stays as
    /// canonical as possible and lookups find long runs.
    pub(super) fn can_merge(left: &Self, right: &Self) -> bool {
        left.end() == right.start
            && left.shared == right.shared
            && left.perms == right.perms
            && match (&left.backing, &right.backing) {
                (None, None) => true,
                (Some(left_backing), Some(right_backing)) => {
                    Arc::ptr_eq(left_backing, right_backing)
                        && left.backing_offset + left.len() == right.backing_offset
                }
                _ => false,
            }
    }

    /// Returns a record covering `left` immediately followed by `right`.
    ///
    /// The caller must have checked [`Self::can_merge`].
    pub(super) fn merged(left: &Self, right: &Self) -> Self {
        debug_assert!(Self::can_merge(left, right));
        Self {
            start: left.start,
            len: NonZeroUsize::new(left.len() + right.len()).expect("both sides are non-empty"),
            perms: left.perms,
            shared: left.shared,
            backing: left.backing.clone(),
            backing_offset: left.backing_offset,
        }
    }

    /// Removes this mapping's page table entries and returns how many pages
    /// were released.
    pub(super) fn unmap(&self, vm_space: &VmSpace) -> usize {
        let range = self.range();
        let guard = disable_preempt();
        let mut cursor = vm_space
            .cursor_mut(&guard, &range)
            .expect("the mapping lies inside the VM space");
        let num_unmapped = cursor.unmap(range.len());
        cursor.flusher().sync_tlb_flush();
        num_unmapped
    }

    /// Applies `perms` to the page table entries of this mapping.
    ///
    /// The write bit is always cleared, whatever the new permissions are. A
    /// mapping is only ever widened by the page-fault handler, which is the one
    /// place that can tell whether the page has to be copied first; a private
    /// page shared with a sibling mapping must stay read-only until then.
    pub(super) fn protect(&self, vm_space: &VmSpace, perms: VmPerms) {
        let new_flags = PageFlags::from(perms) - PageFlags::W;
        let range = self.range();
        let guard = disable_preempt();
        let mut cursor = vm_space
            .cursor_mut(&guard, &range)
            .expect("the mapping lies inside the VM space");

        while cursor.virt_addr() < range.end {
            let remaining = range.end - cursor.virt_addr();
            match cursor.protect_next(remaining, |flags, _| *flags = new_flags) {
                Some(protected) => cursor
                    .flusher()
                    .issue_tlb_flush(TlbFlushOp::for_range(protected)),
                None => break,
            }
        }
        cursor.flusher().dispatch_tlb_flush();
        cursor.flusher().sync_tlb_flush();
    }

    /// Returns the page property that a fault on this mapping installs, minus
    /// the write bit if the page must stay read-only.
    pub(super) fn fault_property(&self, writable: bool, is_write: bool) -> PageProperty {
        let mut flags = self.page_flags() | PageFlags::ACCESSED;
        if is_write {
            flags |= PageFlags::DIRTY;
        }
        if !writable {
            flags -= PageFlags::W;
        }
        PageProperty::new_user(flags, CachePolicy::Writeback)
    }
}

impl core::fmt::Debug for VmMapping {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmMapping")
            .field("range", &self.range())
            .field("perms", &self.perms)
            .field("shared", &self.shared)
            .field("backing_offset", &self.backing_offset)
            .field("backing", &self.backing)
            .finish()
    }
}

/// What [`Mappings::carve`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CarveOutcome {
    /// The number of bytes of the requested range that were mapped.
    pub mapped: usize,
    /// The number of bytes that `op` chose to discard.
    pub dropped: usize,
}

/// The records of a virtual address space, ordered by start address.
///
/// The records are always pairwise non-overlapping, and neighbours that could
/// be represented by one record always are. Every mutation goes through
/// [`Self::insert`], so the two invariants cannot be broken by a caller.
#[derive(Debug, Default)]
pub(super) struct Mappings {
    /// Keyed by the start address, which is also the record's key in practice
    /// because a `BTreeMap` needs the key to be the ordering.
    map: BTreeMap<Vaddr, VmMapping>,
}

impl Mappings {
    /// Returns an empty set.
    pub(super) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// Returns the number of records.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns the record containing `addr`, if any.
    pub fn get(&self, addr: Vaddr) -> Option<&VmMapping> {
        // Records are non-overlapping, so the only candidate for containing
        // `addr` is the last one that starts at or before it.
        let mapping = self.map.range(..=addr).next_back().map(|(_, m)| m)?;
        (mapping.end() > addr).then_some(mapping)
    }

    /// Returns the last record that starts strictly before `addr`.
    pub fn prev(&self, addr: Vaddr) -> Option<&VmMapping> {
        self.map.range(..addr).next_back().map(|(_, m)| m)
    }

    /// Returns the first record that starts at or after `addr`.
    pub fn next(&self, addr: Vaddr) -> Option<&VmMapping> {
        self.map.range(addr..).next().map(|(_, m)| m)
    }

    /// Returns every record, in ascending address order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &VmMapping> {
        self.map.values()
    }

    /// Returns every record intersecting `range`, in ascending address order.
    pub fn iter_in(&self, range: &Range<Vaddr>) -> Vec<&VmMapping> {
        // Walk down from the first record that could reach `range.start`. Only
        // that one can begin before the range, and because the records do not
        // overlap, the first record that ends too early means every earlier
        // one does too.
        let mut found: Vec<&VmMapping> = self
            .map
            .range(..range.end)
            .rev()
            .take_while(|(_, m)| m.end() > range.start)
            .map(|(_, m)| m)
            .collect();
        found.reverse();
        found
    }

    /// Returns the total length of `range` that is covered by a record.
    pub fn count_overlap(&self, range: &Range<Vaddr>) -> usize {
        self.iter_in(range)
            .iter()
            .map(|m| intersect(range, &m.range()).len())
            .sum()
    }

    /// Returns whether every page of `range` is mapped.
    pub fn is_fully_mapped(&self, range: &Range<Vaddr>) -> bool {
        self.count_overlap(range) == range.len()
    }

    /// Removes and returns every record intersecting `range`.
    pub(super) fn take_in(&mut self, range: &Range<Vaddr>) -> Vec<VmMapping> {
        let keys: Vec<Vaddr> = self.iter_in(range).iter().map(|m| m.start).collect();
        keys.into_iter()
            .map(|key| {
                self.map
                    .remove(&key)
                    .expect("a key came from this very set")
            })
            .collect()
    }

    /// Adds a record, merging it with its neighbours where that is possible.
    ///
    /// Merging only ever joins two contiguous records, so it cannot introduce
    /// an overlap; the debug assertion catches a caller that hands us a record
    /// straddling an existing one.
    pub(super) fn insert(&mut self, mapping: VmMapping) {
        let mut mapping = mapping;

        let prev = self
            .prev(mapping.start)
            .filter(|prev| VmMapping::can_merge(prev, &mapping))
            .map(|prev| prev.start);
        if let Some(key) = prev {
            let left = self
                .map
                .remove(&key)
                .expect("a key came from this very set");
            mapping = VmMapping::merged(&left, &mapping);
        }

        let next = self
            .next(mapping.end())
            .filter(|next| VmMapping::can_merge(&mapping, next))
            .map(|next| next.start);
        if let Some(key) = next {
            let right = self
                .map
                .remove(&key)
                .expect("a key came from this very set");
            mapping = VmMapping::merged(&mapping, &right);
        }

        let replaced = self.map.insert(mapping.start, mapping);
        debug_assert!(replaced.is_none(), "an overlapping record was inserted");
    }

    /// Removes the record starting at `start`.
    pub(super) fn remove(&mut self, start: Vaddr) -> Option<VmMapping> {
        self.map.remove(&start)
    }

    /// Removes every record.
    pub(super) fn clear(&mut self) {
        self.map.clear();
    }

    /// Applies `op` to the part of every record that intersects `range`.
    ///
    /// Each intersected piece is removed from the set, split so that `range` is
    /// covered exactly, and handed to `op` with the sub-range it covers.
    /// Whatever `op` returns is put back and merged with its neighbours;
    /// returning `None` discards the piece, which is how `munmap` releases the
    /// pages. The pieces outside `range` are always preserved.
    ///
    /// Returns how much of `range` was mapped and how much `op` discarded.
    pub(super) fn carve<F>(&mut self, range: &Range<Vaddr>, mut op: F) -> CarveOutcome
    where
        F: FnMut(VmMapping, &Range<Vaddr>) -> Option<VmMapping>,
    {
        assert!(range.start.is_multiple_of(PAGE_SIZE) && range.end.is_multiple_of(PAGE_SIZE));
        assert!(range.start < range.end);

        let mut outcome = CarveOutcome::default();
        let mut survivors = Vec::new();

        for piece in self.take_in(range) {
            let sub = intersect(range, &piece.range());
            outcome.mapped += sub.len();

            let (left, middle, right) = piece.split_range(&sub);
            survivors.extend(left);
            survivors.extend(right);
            match op(middle, &sub) {
                Some(kept) => survivors.push(kept),
                None => outcome.dropped += sub.len(),
            }
        }

        for survivor in survivors {
            self.insert(survivor);
        }
        outcome
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    /// A private anonymous record, which is the simplest thing to build in a
    /// test because it needs no backing object.
    fn record(start: Vaddr, pages: usize) -> VmMapping {
        VmMapping::new(
            start,
            NonZeroUsize::new(pages * PAGE_SIZE).unwrap(),
            VmPerms::READ.with_ceiling(),
            false,
            None,
            0,
        )
    }

    fn writable(start: Vaddr, pages: usize) -> VmMapping {
        let mut mapping = record(start, pages);
        mapping.set_perms((VmPerms::READ | VmPerms::WRITE).with_ceiling());
        mapping
    }

    fn page(n: usize) -> Vaddr {
        n * PAGE_SIZE
    }

    fn starts(mappings: &Mappings, range: &Range<Vaddr>) -> Vec<Vaddr> {
        mappings.iter_in(range).iter().map(|m| m.start()).collect()
    }

    #[ktest]
    fn get_finds_only_containing_records() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 2));
        // The record covers `page(1)..page(3)`, so both of its pages resolve
        // to it and the address just past it does not.
        assert_eq!(mappings.get(page(1)).map(VmMapping::start), Some(page(1)));
        assert_eq!(mappings.get(page(2)).map(VmMapping::start), Some(page(1)));
        assert!(mappings.get(page(3)).is_none());
        assert!(mappings.get(page(4)).is_none());
        assert!(mappings.get(page(0)).is_none());
    }

    #[ktest]
    fn prev_and_next_look_around_the_point() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 1));
        mappings.insert(record(page(4), 1));
        assert_eq!(mappings.prev(page(4)).map(VmMapping::start), Some(page(1)));
        assert_eq!(mappings.next(page(4)).map(VmMapping::start), Some(page(4)));
        // `prev` is strict, `next` is not.
        assert!(mappings.prev(page(1)).is_none());
        assert_eq!(mappings.next(page(1)).map(VmMapping::start), Some(page(1)));
    }

    #[ktest]
    fn iter_in_covers_both_sides() {
        let mut mappings = Mappings::new();
        // Two pages each, with a page of space between them so that no two
        // records are adjacent and therefore none of them merge.
        for index in [1, 5, 9] {
            mappings.insert(record(page(index), 2));
        }

        // A range entirely inside the first record.
        assert_eq!(starts(&mappings, &(page(1)..page(3))), [page(1)]);
        // A range that starts inside the first record and ends inside the
        // second, so both are caught.
        assert_eq!(starts(&mappings, &(page(2)..page(6))), [page(1), page(5)]);
        // A range that starts between two records catches only the later one.
        assert_eq!(starts(&mappings, &(page(6)..page(9))), [page(5)]);
        // A range covering a whole record and the space after it.
        assert_eq!(starts(&mappings, &(page(0)..page(20))), [
            page(1),
            page(5),
            page(9)
        ]);
        // Nothing overlaps.
        assert!(starts(&mappings, &(page(3)..page(5))).is_empty());

        // `page(2)..page(6)` is covered by one page of each of the two records
        // it intersects, so it is only half mapped.
        assert_eq!(mappings.count_overlap(&(page(2)..page(6))), 2 * PAGE_SIZE);
        assert!(!mappings.is_fully_mapped(&(page(2)..page(6))));
        assert!(mappings.is_fully_mapped(&(page(1)..page(3))));
        // The two-page gap between the first two records is what makes anything
        // spanning it unmapped.
        assert!(!mappings.is_fully_mapped(&(page(1)..page(5))));
        assert!(!mappings.is_fully_mapped(&(page(1)..page(7))));
        // All three records together cover six of the twelve pages asked for.
        assert_eq!(mappings.count_overlap(&(page(0)..page(12))), 6 * PAGE_SIZE);
    }

    #[ktest]
    fn insert_merges_contiguous_compatible_records() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 1));
        mappings.insert(record(page(2), 1));
        mappings.insert(record(page(3), 1));
        // All three are contiguous and indistinguishable, so they are one.
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings.get(page(1)).unwrap().range(), page(1)..page(4));

        // A record with a gap stays separate.
        mappings.insert(record(page(6), 1));
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings.get(page(6)).unwrap().range(), page(6)..page(7));
    }

    #[ktest]
    fn insert_does_not_merge_incompatible_records() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 1));
        mappings.insert(writable(page(2), 1));
        mappings.insert(record(page(3), 1));
        // The middle record differs from both of its neighbours.
        assert_eq!(mappings.len(), 3);
    }

    #[ktest]
    fn carve_splits_and_then_merges_back() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 4));

        let outcome = mappings.carve(&(page(2)..page(4)), |middle, sub| {
            assert_eq!(sub.start, page(2));
            assert_eq!(sub.len(), 2 * PAGE_SIZE);
            Some(middle)
        });

        assert_eq!(outcome.mapped, 2 * PAGE_SIZE);
        assert_eq!(outcome.dropped, 0);
        // All three pieces are contiguous and identical, so the set is
        // indistinguishable from before the carve.
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings.get(page(1)).unwrap().range(), page(1)..page(5));
    }

    #[ktest]
    fn carve_keeps_the_pieces_distinct_when_the_middle_differs() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 4));

        mappings.carve(&(page(2)..page(4)), |middle, _| Some(writable_at(middle, page(2))));

        // The unchanged sides stay put and cannot merge with the new middle.
        assert_eq!(starts(&mappings, &(page(1)..page(5))), [
            page(1),
            page(2),
            page(4)
        ]);
    }

    /// Returns `mapping` moved to `at`, with a different protection so that it
    /// does not merge with its neighbours.
    fn writable_at(mapping: VmMapping, at: Vaddr) -> VmMapping {
        let mut moved = mapping.relocate(at);
        moved.set_perms((VmPerms::READ | VmPerms::WRITE).with_ceiling());
        moved
    }

    #[ktest]
    fn carve_can_discard_the_middle() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 3));
        mappings.insert(record(page(10), 1));

        let outcome = mappings.carve(&(page(2)..page(3)), |_, _| None);

        assert_eq!(outcome.mapped, PAGE_SIZE);
        assert_eq!(outcome.dropped, PAGE_SIZE);
        // The hole is gone, and the untouched record is still there. The two
        // sides of the hole cannot merge, because they are not adjacent.
        assert!(mappings.get(page(2)).is_none());
        assert_eq!(starts(&mappings, &(page(0)..page(20))), [page(1), page(3), page(10)]);
    }

    #[ktest]
    fn carve_over_a_hole_maps_nothing() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 1));
        let outcome = mappings.carve(&(page(5)..page(7)), |_, _| {
            panic!("no record intersects the range")
        });
        assert_eq!(outcome, CarveOutcome::default());
        assert_eq!(mappings.len(), 1);
    }

    #[ktest]
    fn carve_covering_a_whole_record_keeps_it_intact() {
        let mut mappings = Mappings::new();
        mappings.insert(record(page(1), 1));
        mappings.insert(record(page(3), 1));
        let outcome = mappings.carve(&(page(1)..page(4)), |_, _| None);
        assert_eq!(outcome.mapped, 2 * PAGE_SIZE);
        assert_eq!(outcome.dropped, 2 * PAGE_SIZE);
        assert_eq!(mappings.len(), 0);
    }

    #[ktest]
    fn take_in_removes_exactly_what_it_reports() {
        let mut mappings = Mappings::new();
        for index in [1, 5, 9] {
            mappings.insert(record(page(index), 2));
        }
        // `page(6)..page(9)` catches the tail of the second record only: the
        // third starts exactly at `page(9)`, which is outside the range.
        let taken = mappings.take_in(&(page(6)..page(9)));
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].start(), page(5));
        assert_eq!(taken[0].range(), page(5)..page(7));
        assert_eq!(starts(&mappings, &(page(0)..page(20))), [page(1), page(9)]);
    }

    #[ktest]
    fn randomized_inserts_never_overlap() {
        let mut mappings = Mappings::new();
        // A deterministic pseudo-random walk over a small address space, so the
        // test is reproducible.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _ in 0..512 {
            let pages = (next() % 4 + 1) as usize;
            let index = next() % 32;
            let mapping = record(page(index as usize), pages);

            // Make room first, the way a `MAP_FIXED` mapping does, then insert.
            mappings.carve(&(mapping.start..mapping.end()), |_, _| None);
            mappings.insert(mapping);

            // The invariant: ascending, and every record starts at or after the
            // end of its predecessor.
            let mut previous_end = 0;
            for record in mappings.iter() {
                assert!(
                    record.start() >= previous_end,
                    "record {:#x}..{:#x} overlaps the record ending at {previous_end:#x}",
                    record.start(),
                    record.end()
                );
                previous_end = record.end();
            }
            // And the set still answers every question about the same points.
            for probe in 0..32u64 {
                let addr = page(probe as usize);
                let found = mappings.get(addr);
                assert_eq!(found.is_some(), found.is_some_and(|m| m.end() > addr));
            }
        }
    }
}
