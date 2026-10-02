// SPDX-License-Identifier: GPL-2.0

pub mod console;
pub mod ldisc;

use alloc::sync::Arc;
use spin::Once;
use crate::drivers::register_chrdev;
use console::Console;

static CONSOLE: Once<Arc<Console>> = Once::new();

/// Initialize the TTY subsystem
pub fn init() {
    let console_dev = Arc::new(Console::new());
    let _ = CONSOLE.call_once(|| Arc::clone(&console_dev));
    register_chrdev(console_dev);
    ostd::info!("TTY subsystem initialized");
}

/// Returns the primary system console device.
pub fn get_console() -> Option<Arc<Console>> {
    CONSOLE.get().cloned()
}

/// Dispatches an incoming character from the keyboard into the TTY line discipline.
pub fn handle_keyboard_char(ch: u8) {
    if let Some(console) = CONSOLE.get() {
        console.handle_input(ch);
    }
}
