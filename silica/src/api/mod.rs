// SPDX-License-Identifier: GPL-2.0

//! The interface user space sees.
//!
//! Every type here is part of an ABI: either a shape user space passes to or
//! receives from a syscall, or a set of constants both sides agree on. They are
//! therefore layout-sensitive (`#[repr(C)]` where a C struct is implied), and the
//! comments about them describe what the *other* side of the boundary expects
//! rather than how the kernel uses them internally.
//!
//! | Module | ABI |
//! |---|---|
//! | [`errno`] | `errno` numbers, and the negated value a failed syscall returns |
//! | [`cred`] | `uid_t`, `gid_t` and `struct ucred` |
//! | [`limit`] | `struct rlimit` and the `RLIMIT_*` resource identifiers |
//! | [`signal`] | `sigset_t`, `struct sigaction` and the signal numbers |
//! | [`termios`] | `struct termios`, `struct winsize`, `tcflag_t` |
//! | [`ioctl`] | `ioctl(2)` request numbers |
//!
//! Nothing here acts on the data. The mechanisms that do live next to their
//! subject: signal delivery and process state in [`crate::proc`], file
//! descriptors in [`crate::fs`].

pub mod cred;
pub mod errno;
pub mod ioctl;
pub mod limit;
pub mod signal;
pub mod termios;
