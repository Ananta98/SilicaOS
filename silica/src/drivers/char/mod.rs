// SPDX-License-Identifier: GPL-2.0

//! Character Device Drivers Subsystem.
//!
//! Provides drivers for character devices including the i8042 PS/2 controller,
//! keyboards, mice, serial ports, and terminal devices.

pub mod i8042;
pub mod tty;
