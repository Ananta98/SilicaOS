// SPDX-License-Identifier: GPL-2.0

//! User-space arguments of the virtual memory system calls.
//!
//! Each type here corresponds to one system call's flag word. Parsing is strict:
//! an unknown bit is [`Errno::EINVAL`], which is what Linux does for the flag
//! words that these types mirror.

use bitflags::bitflags;

use crate::errno::Result;

bitflags! {
    /// The `flags` argument of `mmap(2)`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct MmapFlags: u32 {
        /// `MAP_SHARED`: writes are carried through to the backing object and
        /// are visible to every other mapping of it.
        const SHARED          = 0x0000_0001;
        /// `MAP_PRIVATE`: writes are private to this mapping.
        const PRIVATE         = 0x0000_0002;
        /// `MAP_FIXED`: place the mapping at the given address, replacing
        /// whatever was there.
        const FIXED           = 0x0000_0010;
        /// `MAP_ANONYMOUS`: the mapping is not backed by a file.
        const ANONYMOUS       = 0x0000_0020;
        /// `MAP_FIXED_NOREPLACE`: like `MAP_FIXED`, but fail rather than
        /// replace an existing mapping.
        const FIXED_NOREPLACE = 0x0010_0000;
        /// `MAP_POPULATE`: fault the whole mapping in before returning.
        const POPULATE = 0x0000_8000;
    }
}

impl MmapFlags {
    /// `MAP_32BIT`: place the mapping in the first 2 GiB of the address space.
    ///
    /// The bit has this value only on x86-64; on other architectures bit 6 is
    /// not assigned to any flag.
    #[cfg(target_arch = "x86_64")]
    pub const MAP_32BIT: Self = Self::from_bits_retain(0x0000_0040);

    const KNOWN_BASE: Self = Self::SHARED
        .union(Self::PRIVATE)
        .union(Self::FIXED)
        .union(Self::ANONYMOUS)
        .union(Self::FIXED_NOREPLACE)
        .union(Self::POPULATE);

    /// Every bit this architecture assigns, which is what `from_raw` checks the
    /// argument against.
    #[cfg(target_arch = "x86_64")]
    const KNOWN: Self = Self::KNOWN_BASE.union(Self::MAP_32BIT);
    #[cfg(not(target_arch = "x86_64"))]
    const KNOWN: Self = Self::KNOWN_BASE;

    /// Parses the `flags` argument of `mmap(2)`.
    pub fn from_raw(raw: u32) -> Result<Self> {
        if raw & !Self::KNOWN.bits() != 0 {
            crate::return_errno!(
                EINVAL,
                "the mmap flags {raw:#x} have bits outside {:#x}",
                Self::KNOWN.bits()
            );
        }
        // Every remaining bit is assigned, including the architecture-specific
        // ones that are not part of the `bitflags` declaration.
        let flags = Self::from_bits_retain(raw);
        flags.validate()?;
        Ok(flags)
    }

    /// Rejects flag combinations that Linux also rejects.
    ///
    /// This runs on every `mmap`, not just on a parsed argument word, because a
    /// caller that assembles the flags itself must not be able to slip past it.
    pub(crate) fn validate(self) -> Result<()> {
        if self.contains(Self::SHARED) && self.contains(Self::PRIVATE) {
            crate::return_errno!(EINVAL, "MAP_SHARED and MAP_PRIVATE are mutually exclusive");
        }
        if self.contains(Self::FIXED) && self.contains(Self::FIXED_NOREPLACE) {
            crate::return_errno!(
                EINVAL,
                "MAP_FIXED and MAP_FIXED_NOREPLACE are mutually exclusive"
            );
        }
        Ok(())
    }

    /// Returns whether the mapping is shared with other mappings of the same
    /// backing object.
    pub fn is_shared(self) -> bool {
        self.contains(Self::SHARED)
    }

    /// Returns whether the mapping was requested as anonymous.
    pub fn is_anonymous(self) -> bool {
        self.contains(Self::ANONYMOUS)
    }
}

/// The `advice` argument of `madvise(2)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum MadviseAdvice {
    /// `MADV_NORMAL`: no special treatment.
    Normal = 0,
    /// `MADV_RANDOM`: expect page-readahead-disabled access. No effect here.
    Random = 1,
    /// `MADV_SEQUENTIAL`: expect aggressive read-ahead. No effect here.
    Sequential = 2,
    /// `MADV_WILLNEED`: fault the range in ahead of time.
    WillNeed = 3,
    /// `MADV_DONTNEED`: drop the pages; a later access gets a fresh page.
    DontNeed = 4,
    /// `MADV_FREE`: drop the pages but keep their contents available until the
    /// memory is needed elsewhere. Treated as [`Self::DontNeed`].
    Free = 8,
    /// `MADV_DONTNEED_COLD`: like [`Self::DontNeed`], but more eager to
    /// reclaim under pressure. Treated as [`Self::DontNeed`].
    DontNeedCold = 16,
}

impl MadviseAdvice {
    /// Parses the `advice` argument of `madvise(2)`.
    pub fn from_raw(raw: u32) -> Result<Self> {
        match raw {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Random),
            2 => Ok(Self::Sequential),
            3 => Ok(Self::WillNeed),
            4 => Ok(Self::DontNeed),
            8 => Ok(Self::Free),
            16 => Ok(Self::DontNeedCold),
            _ => crate::return_errno!(EINVAL, "unsupported madvise advice {raw}"),
        }
    }

    /// Returns whether the advice asks for the pages to be dropped.
    pub fn drops_pages(self) -> bool {
        matches!(self, Self::DontNeed | Self::Free | Self::DontNeedCold)
    }
}

bitflags! {
    /// The `flags` argument of `mremap(2)`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct MremapFlags: u32 {
        /// `MREMAP_MAYMOVE`: the kernel may move the mapping elsewhere.
        const MAYMOVE   = 0x01;
        /// `MREMAP_FIXED`: move the mapping to the address passed as `new_address`.
        const FIXED     = 0x02;
        /// `MREMAP_DONTUNMAP`: keep the old address range mapped, sharing the
        /// same physical pages as the new range.
        const DONTUNMAP = 0x04;
    }
}

impl MremapFlags {
    const KNOWN: Self = Self::MAYMOVE.union(Self::FIXED).union(Self::DONTUNMAP);

    /// Parses the `flags` argument of `mremap(2)`.
    pub fn from_raw(raw: u32) -> Result<Self> {
        if raw & !Self::KNOWN.bits() != 0 {
            crate::return_errno!(EINVAL, "the mremap flags {raw:#x} are not all known");
        }
        let flags = Self::from_bits_retain(raw);
        if flags.contains(Self::FIXED) && flags.contains(Self::DONTUNMAP) {
            // Linux rejects this because the two describe incompatible
            // dispositions of the old range.
            crate::return_errno!(EINVAL, "MREMAP_FIXED and MREMAP_DONTUNMAP conflict");
        }
        Ok(flags)
    }
}

bitflags! {
    /// The `flags` argument of `msync(2)`.
    ///
    /// Exactly one of these may be given, because each says something different
    /// about what should happen to the pages.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct MsyncFlags: u32 {
        /// `MS_ASYNC`: start the writeback and return without waiting for it.
        const ASYNC = 0x01;
        /// `MS_INVALIDATE`: drop the mapping's cached view, so that a later
        /// access re-reads the object.
        const INVALIDATE = 0x02;
        /// `MS_SYNC`: wait for the writeback to finish.
        const SYNC = 0x04;
    }
}

impl MsyncFlags {
    const KNOWN: Self = Self::ASYNC.union(Self::INVALIDATE).union(Self::SYNC);

    /// Parses the `flags` argument of `msync(2)`.
    pub fn from_raw(raw: u32) -> Result<Self> {
        if raw & !Self::KNOWN.bits() != 0 {
            crate::return_errno!(EINVAL, "unknown msync flags {raw:#x}");
        }
        let flags = Self::from_bits_retain(raw);
        flags.validate()?;
        Ok(flags)
    }

    /// Rejects anything other than exactly one of the three flags.
    ///
    /// This runs on every `msync`, not just on a parsed argument word, because a
    /// caller that assembles the flags itself must not be able to slip past it.
    pub(crate) fn validate(self) -> Result<()> {
        if self.is_empty() {
            crate::return_errno!(
                EINVAL,
                "msync needs one of MS_ASYNC, MS_SYNC or MS_INVALIDATE"
            );
        }
        if self != Self::ASYNC && self != Self::INVALIDATE && self != Self::SYNC {
            crate::return_errno!(
                EINVAL,
                "MS_ASYNC, MS_SYNC and MS_INVALIDATE are mutually exclusive, got {:#x}",
                self.bits()
            );
        }
        Ok(())
    }
}
