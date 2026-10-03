// SPDX-License-Identifier: GPL-2.0

//! DRM and Framebuffer Display Subsystem.
//!
//! Provides a unified display architecture inspired by Linux fbdev/DRM and
//! FreeBSD vt, managing physical display surfaces, virtual terminal console
//! emulation, and `/dev/fb0` character devices.

pub mod framebuffer;
pub use framebuffer::get_console;
