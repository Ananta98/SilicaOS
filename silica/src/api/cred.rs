// SPDX-License-Identifier: GPL-2.0

//! User credentials.
//!
//! Credentials represent the user and group identity of a process. In FreeBSD,
//! credentials are fully immutable objects (`struct ucred`) wrapped in an `Arc`.
//! Any modification creates a new copy-on-write `Arc<Ucred>`, which guarantees
//! lockless read access and atomic credential switching.

use alloc::vec::Vec;

/// User identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Uid(pub u32);

impl Uid {
    pub const ROOT: Self = Self(0);

    pub const fn is_root(&self) -> bool {
        self.0 == 0
    }

    pub const fn as_u32(&self) -> u32 {
        self.0
    }
}

/// Group identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Gid(pub u32);

impl Gid {
    pub const ROOT: Self = Self(0);

    pub const fn is_root(&self) -> bool {
        self.0 == 0
    }

    pub const fn as_u32(&self) -> u32 {
        self.0
    }
}

/// Immutable user credential struct mirroring FreeBSD `struct ucred`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ucred {
    /// Effective user ID.
    pub cr_uid: Uid,
    /// Real user ID.
    pub cr_ruid: Uid,
    /// Saved set-user-ID.
    pub cr_svuid: Uid,
    /// Effective group ID.
    pub cr_gid: Gid,
    /// Real group ID.
    pub cr_rgid: Gid,
    /// Saved set-group-ID.
    pub cr_svgid: Gid,
    /// Supplementary group IDs.
    pub cr_groups: Vec<Gid>,
}

impl Ucred {
    /// Creates a default root credential (UID 0, GID 0).
    pub const fn root() -> Self {
        Self {
            cr_uid: Uid::ROOT,
            cr_ruid: Uid::ROOT,
            cr_svuid: Uid::ROOT,
            cr_gid: Gid::ROOT,
            cr_rgid: Gid::ROOT,
            cr_svgid: Gid::ROOT,
            cr_groups: Vec::new(),
        }
    }

    /// Creates a credential for a given UID and GID.
    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            cr_uid: Uid(uid),
            cr_ruid: Uid(uid),
            cr_svuid: Uid(uid),
            cr_gid: Gid(gid),
            cr_rgid: Gid(gid),
            cr_svgid: Gid(gid),
            cr_groups: Vec::new(),
        }
    }

    /// Returns whether the effective user is superuser (root).
    pub fn is_superuser(&self) -> bool {
        self.cr_uid.is_root()
    }
}

impl Default for Ucred {
    fn default() -> Self {
        Self::root()
    }
}
