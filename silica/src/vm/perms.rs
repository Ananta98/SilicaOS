// SPDX-License-Identifier: GPL-2.0

//! Memory access permissions of mappings.
//!
//! A mapping carries two independent sets of bits, mirroring Linux's split
//! between a VMA's current protection and the protection it may be raised to:
//!
//! - `READ | WRITE | EXEC` is the protection in force right now. This is what
//!   `mprotect(2)` sets and what the page table entries are derived from.
//! - `MAY_READ | MAY_WRITE | MAY_EXEC` is the ceiling fixed at `mmap(2)` time.
//!
//! Keeping the ceiling is what makes `mprotect` faithful: a mapping created
//! with `PROT_READ` must fail a later `mprotect(PROT_READ | PROT_WRITE)` with
//! `EACCES` rather than silently granting write access. The `MAY_*` bits are
//! laid out so that shifting them down by three yields the corresponding
//! `PROT_*` bit.

use bitflags::bitflags;

use ostd::mm::PageFlags;

use crate::errno::Result;

bitflags! {
    /// The memory access permissions of a mapping.
    ///
    /// The low three bits are the permissions currently in force and match the
    /// `PROT_*` values that user space passes to `mmap(2)` and `mprotect(2)`.
    /// The next three bits are the permissions the mapping may ever be raised
    /// to.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct VmPerms: u8 {
        /// Readable.
        const READ    = 1 << 0;
        /// Writable.
        const WRITE   = 1 << 1;
        /// Executable.
        const EXEC    = 1 << 2;
        /// May be protected to readable.
        const MAY_READ  = 1 << 3;
        /// May be protected to writable.
        const MAY_WRITE = 1 << 4;
        /// May be protected to executable.
        const MAY_EXEC  = 1 << 5;
    }
}

impl VmPerms {
    /// The bits that represent permissions in force.
    pub const GRANTED: Self = Self::READ.union(Self::WRITE).union(Self::EXEC);

    /// The bits that represent the ceiling.
    pub const ALL_MAY: Self = Self::MAY_READ.union(Self::MAY_WRITE).union(Self::MAY_EXEC);

    /// Parses a `PROT_*` bitmask coming from user space.
    ///
    /// Returns [`Errno::EINVAL`] if any bit outside `PROT_READ | PROT_WRITE |
    /// PROT_EXEC` is set. The `MAY_*` bits share the type, so they have to be
    /// rejected explicitly rather than left to [`Self::from_bits`].
    pub fn from_prot(bits: u32) -> Result<Self> {
        if bits as u8 & !Self::GRANTED.bits() != 0 {
            crate::return_errno!(EINVAL, "invalid protection bits {bits:#x}");
        }
        Ok(Self::from_bits_retain(bits as u8))
    }

    /// Returns the permissions currently in force.
    pub const fn granted(self) -> Self {
        // `intersect` is not const in `bitflags` 2, so mask the bits by hand.
        Self::from_bits_retain(self.bits() & Self::GRANTED.bits())
    }

    /// Returns `perms` with its ceiling raised to match.
    ///
    /// This is what `mmap(2)` does: the mapping gets exactly the `PROT_*`
    /// permissions that were requested, and `mprotect(2)` may later narrow them
    /// but never widen them.
    pub const fn with_ceiling(self) -> Self {
        Self::from_bits_retain(self.bits() | (self.granted().bits() << 3))
    }

    /// Returns `self` with the permissions in force replaced by `granted`, and
    /// the ceiling left untouched.
    ///
    /// This is what `mprotect(2)` does: it moves the low three bits and leaves
    /// the `MAY_*` ceiling exactly where `mmap(2)` put it.
    pub const fn with_granted(self, granted: Self) -> Self {
        Self::from_bits_retain((self.bits() & Self::ALL_MAY.bits()) | granted.granted().bits())
    }

    /// Returns the ceiling, i.e. the permissions that were requested at
    /// `mmap(2)` time.
    pub const fn may(self) -> Self {
        Self::from_bits_retain((self.bits() & Self::ALL_MAY.bits()) >> 3)
    }

    /// Returns whether `perms` is within the ceiling of `self`.
    pub fn allows(self, perms: Self) -> bool {
        self.may().contains(perms)
    }

    /// Checks that the permissions in force do not exceed the ceiling.
    pub fn check(self) -> Result<()> {
        if self.allows(self.granted()) {
            Ok(())
        } else {
            crate::return_errno!(EACCES, "permissions {:?} exceed the ceiling {:?}", self.granted(), self.may())
        }
    }
}

impl From<PageFlags> for VmPerms {
    fn from(flags: PageFlags) -> Self {
        let mut perms = Self::empty();
        if flags.contains(PageFlags::R) {
            perms |= Self::READ;
        }
        if flags.contains(PageFlags::W) {
            perms |= Self::WRITE;
        }
        if flags.contains(PageFlags::X) {
            perms |= Self::EXEC;
        }
        perms
    }
}

impl From<VmPerms> for PageFlags {
    fn from(perms: VmPerms) -> Self {
        let granted = perms.granted();
        let mut flags = PageFlags::empty();
        if granted.contains(VmPerms::READ) {
            flags |= PageFlags::R;
        }
        if granted.contains(VmPerms::WRITE) {
            flags |= PageFlags::W;
        }
        if granted.contains(VmPerms::EXEC) {
            flags |= PageFlags::X;
        }
        flags
    }
}

const _: () = {
    // `VmPerms` mirrors both the `PROT_*` values that user space passes and the
    // low bits of OSTD's `PageFlags`, so the conversions above are bit-for-bit
    // rather than a table lookup.
    const _: () = assert!(VmPerms::READ.bits() == 1 && VmPerms::WRITE.bits() == 2);
    const _: () = assert!(VmPerms::EXEC.bits() == 4);
    const _: () = assert!(VmPerms::READ.bits() == PageFlags::R.bits());
    const _: () = assert!(VmPerms::WRITE.bits() == PageFlags::W.bits());
    const _: () = assert!(VmPerms::EXEC.bits() == PageFlags::X.bits());
    // Shifting the `MAY_*` bits down by three must yield the granted bits.
    const _: () = assert!(VmPerms::ALL_MAY.bits() >> 3 == VmPerms::GRANTED.bits());
};

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use crate::errno::Errno;

    use super::*;

    #[ktest]
    fn from_prot_rejects_unknown_bits() {
        assert_eq!(VmPerms::from_prot(0b111), Ok(VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC));
        assert_eq!(VmPerms::from_prot(0b1000), Err(Errno::EINVAL));
        assert_eq!(VmPerms::from_prot(0), Ok(VmPerms::empty()));
    }

    #[ktest]
    fn may_is_granted_shifted_down() {
        let perms = (VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC).with_ceiling();
        assert_eq!(perms.may(), VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC);
        assert!(perms.allows(VmPerms::WRITE));
    }

    #[ktest]
    fn ceiling_is_enforced() {
        // `mmap(PROT_READ)` may never be widened to writable.
        let perms = VmPerms::READ.with_ceiling();
        assert!(!perms.allows(VmPerms::READ | VmPerms::WRITE));
        assert_eq!(perms.check(), Ok(()));

        // Adding a `MAY_WRITE` without the granted bit is itself inconsistent.
        let bogus = VmPerms::READ | VmPerms::MAY_WRITE;
        assert_eq!(bogus.check(), Err(Errno::EACCES));
    }

    #[ktest]
    fn page_flags_round_trip() {
        for granted in [
            VmPerms::empty(),
            VmPerms::READ,
            VmPerms::READ | VmPerms::WRITE,
            VmPerms::READ | VmPerms::EXEC,
            VmPerms::READ | VmPerms::WRITE | VmPerms::EXEC,
        ] {
            let flags = PageFlags::from(granted);
            assert_eq!(VmPerms::from(flags), granted);
        }
    }
}
