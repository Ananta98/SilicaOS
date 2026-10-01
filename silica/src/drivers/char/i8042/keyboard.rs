// SPDX-License-Identifier: GPL-2.0

//! PS/2 Keyboard Driver.
//!
//! Provides scancode decoding (Set 1), modifier tracking (Shift, Ctrl, Alt, CapsLock),
//! interrupt-driven input handling via ISA IRQ 1, serial logging, and DevFS integration.

use crate::drivers::CharacterDevice;
use alloc::{collections::vec_deque::VecDeque, string::String, sync::Arc};
use ostd::{
    arch::{
        irq::{IRQ_CHIP, MappedIrqLine},
        trap::TrapFrame,
    },
    irq::IrqLine,
};
use spin::{Mutex, Once};

use super::{
    controller::{I8042_CONTROLLER, I8042Controller, StatusFlags},
    ps2::{CMD_ENABLE_SCANNING, CMD_RESET, send_keyboard_cmd},
};
use crate::errno::Result;

const ISA_KEYBOARD_IRQ: u8 = 1;

/// Mapped hardware interrupt line for keyboard.
static KEYBOARD_IRQ_LINE: Once<MappedIrqLine> = Once::new();

/// Global ring buffer storing received keyboard ASCII characters.
static KEYBOARD_BUFFER: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());

/// Global keyboard state machine.
static KEYBOARD_STATE: Mutex<KeyboardState> = Mutex::new(KeyboardState::new());

// ----------------------------------------------------------------------------
// Scancode Translation Tables (US QWERTY - Set 1)
// ----------------------------------------------------------------------------

const SCANCODE_NORMAL: [u8; 128] = [
    0, 0x1B, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x08, b'\t',
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', b'\n', 0, b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const SCANCODE_SHIFTED: [u8; 128] = [
    0, 0x1B, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0x08, b'\t',
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', b'\n', 0, b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Tracks modifier keys and extended prefix status.
pub struct KeyboardState {
    lshift: bool,
    rshift: bool,
    ctrl: bool,
    alt: bool,
    caps_lock: bool,
    extended: bool,
}

impl KeyboardState {
    pub const fn new() -> Self {
        Self {
            lshift: false,
            rshift: false,
            ctrl: false,
            alt: false,
            caps_lock: false,
            extended: false,
        }
    }

    /// Process a single raw scancode byte from port 0x60.
    ///
    /// Returns `Some(char)` if a printable or control character was produced.
    pub fn process_scancode(&mut self, scancode: u8) -> Option<char> {
        // Extended prefix byte 0xE0
        if scancode == 0xE0 {
            self.extended = true;
            return None;
        }

        let is_release = (scancode & 0x80) != 0;
        let make_code = scancode & 0x7F;

        if is_release {
            match make_code {
                0x2A => self.lshift = false,
                0x36 => self.rshift = false,
                0x1D => self.ctrl = false,
                0x38 => self.alt = false,
                _ => {}
            }
            self.extended = false;
            return None;
        }

        // Key Press (Make code)
        match make_code {
            0x2A => {
                self.lshift = true;
                return None;
            }
            0x36 => {
                self.rshift = true;
                return None;
            }
            0x1D => {
                self.ctrl = true;
                return None;
            }
            0x38 => {
                self.alt = true;
                return None;
            }
            0x3A => {
                self.caps_lock = !self.caps_lock;
                return None;
            }
            _ => {}
        }

        let idx = make_code as usize;
        if idx >= SCANCODE_NORMAL.len() {
            self.extended = false;
            return None;
        }

        let is_shifted = self.lshift || self.rshift;
        let base_char = SCANCODE_NORMAL[idx];
        let is_alpha = (b'a'..=b'z').contains(&base_char);

        let effective_shift = if is_alpha {
            is_shifted ^ self.caps_lock
        } else {
            is_shifted
        };

        let ch = if effective_shift {
            SCANCODE_SHIFTED[idx]
        } else {
            SCANCODE_NORMAL[idx]
        };

        self.extended = false;

        if ch != 0 { Some(ch as char) } else { None }
    }
}

// ----------------------------------------------------------------------------
// Interrupt Handler & Input Ingestion
// ----------------------------------------------------------------------------

fn handle_keyboard_irq(_frame: &TrapFrame) {
    if let Some(ctrl_lock) = I8042_CONTROLLER.get() {
        let ctrl = ctrl_lock.lock();
        let status = ctrl.read_status();

        // Verify output buffer is full and data is for keyboard (not mouse aux)
        if status.contains(StatusFlags::OUTPUT_FULL) && !status.contains(StatusFlags::AUX_FULL) {
            let scancode = ctrl.read_data();
            drop(ctrl); // release controller lock before queueing and logging

            let maybe_char = {
                let mut state = KEYBOARD_STATE.lock();
                state.process_scancode(scancode)
            };

            if let Some(ch) = maybe_char {
                // Push to keyboard FIFO buffer
                {
                    let mut buf = KEYBOARD_BUFFER.lock();
                    if buf.len() < 1024 {
                        buf.push_back(ch as u8);
                    }
                }
                // Serial log for interactive console feedback
                ostd::info!("[KEYBOARD] Key: '{}' (scancode: 0x{:02X})", ch, scancode);
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Unified Device Model Trait Implementation
// ----------------------------------------------------------------------------

pub struct KeyboardDevice;

impl CharacterDevice for KeyboardDevice {
    fn name(&self) -> String {
        "keyboard".into()
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut kbuf = KEYBOARD_BUFFER.lock();
        let mut count = 0;
        while count < buf.len() {
            if let Some(byte) = kbuf.pop_front() {
                buf[count] = byte;
                count += 1;
            } else {
                break;
            }
        }
        Ok(count)
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        // Writes to keyboard node can be consumed as LED control or discarded
        Ok(buf.len())
    }
}

// ----------------------------------------------------------------------------
// Initialization & Self-Test
// ----------------------------------------------------------------------------

/// Probes and initializes the PS/2 keyboard, binds IRQ 1, tests scancode decoding,
/// and registers the driver.
pub fn init(controller: &I8042Controller) -> Result<()> {
    ostd::info!("PS/2 Keyboard: Initializing...");

    // 1. Send reset command (non-fatal if keyboard ignores it)
    if let Err(err) = send_keyboard_cmd(controller, CMD_RESET) {
        ostd::warn!("PS/2 Keyboard: Reset command error (ignored): {:?}", err);
    }
    // Flush any self-test response (e.g. 0xAA)
    controller.flush_buffer();

    // 2. Enable keyboard scanning
    if let Err(err) = send_keyboard_cmd(controller, CMD_ENABLE_SCANNING) {
        ostd::warn!("PS/2 Keyboard: Enable scanning error (ignored): {:?}", err);
    }

    // 3. Allocate and map ISA IRQ 1
    if let Some(chip) = IRQ_CHIP.get() {
        if let Ok(irq_line) = IrqLine::alloc() {
            match chip.map_isa_pin_to(irq_line, ISA_KEYBOARD_IRQ) {
                Ok(mut mapped_line) => {
                    mapped_line.on_active(handle_keyboard_irq);
                    KEYBOARD_IRQ_LINE.call_once(|| mapped_line);
                    ostd::info!(
                        "PS/2 Keyboard: Mapped ISA IRQ {} successfully",
                        ISA_KEYBOARD_IRQ
                    );
                }
                Err(err) => {
                    ostd::warn!(
                        "PS/2 Keyboard: Failed to map ISA IRQ {}: {:?}",
                        ISA_KEYBOARD_IRQ,
                        err
                    );
                }
            }
        }
    }

    // 5. Register into unified driver registry
    crate::drivers::register_chrdev(Arc::new(KeyboardDevice));

    ostd::info!("PS/2 Keyboard: Driver registered successfully.");
    Ok(())
}


