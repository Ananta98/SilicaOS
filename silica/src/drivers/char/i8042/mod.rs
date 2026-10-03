// SPDX-License-Identifier: GPL-2.0

//! i8042 PS/2 Controller, Keyboard, and Mouse Drivers.
//!
//! Provides the controller interface, PS/2 protocol handlers, Scancode Set 1
//! decoding, mouse packet assembly, interrupt handling, and DevFS nodes.

pub mod controller;
pub mod keyboard;
pub mod mouse;
pub mod ps2;

pub use controller::I8042Controller;
pub use keyboard::KeyboardDevice;
pub use mouse::MouseDevice;

use core::sync::atomic::{AtomicBool, Ordering};
use crate::api::errno::Result;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initializes the i8042 controller subsystem, probing keyboard and mouse.
pub fn init() -> Result<()> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    controller::init()
}

// ----------------------------------------------------------------------------
// Kernel Module Declaration via module! macro
// ----------------------------------------------------------------------------

crate::module!(
    "PS/2 i8042 Controller and Input Drivers",
    "SilicaOS Team",
    crate::modules::InitcallLevel::Device,
    init
);
