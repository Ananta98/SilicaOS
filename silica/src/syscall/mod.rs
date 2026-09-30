// SPDX-License-Identifier: GPL-2.0

//! System call dispatch and definitions.

#[macro_use]
pub mod macros;

pub mod arch;
pub mod fs;
pub mod mm;
pub mod proc;
pub mod sched;

pub use arch::dispatch;
