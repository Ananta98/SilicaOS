// SPDX-License-Identifier: GPL-2.0

//! PS/2 Mouse Driver.
//!
//! Provides mouse packet assembly (3-byte standard and 4-byte IntelliMouse),
//! relative motion tracking, button state decoding, ISA IRQ 12 interrupt handler,
//! and DevFS character device integration (`/dev/psaux`, `/dev/mouse`).

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
    ps2::{
        CMD_ENABLE_SCANNING, CMD_GET_DEVICE_ID, CMD_SET_SAMPLE_RATE, DEV_ID_MOUSE_INTELLIMOUSE,
        reset_mouse, send_mouse_cmd, send_mouse_cmd_arg,
    },
};
use crate::errno::Result;

const ISA_MOUSE_IRQ: u8 = 12;

/// Mapped hardware interrupt line for mouse.
static MOUSE_IRQ_LINE: Once<MappedIrqLine> = Once::new();

/// Global ring buffer storing raw mouse packets for `/dev/psaux` and `/dev/mouse`.
static MOUSE_BUFFER: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());

/// Global mouse packet parser state.
static MOUSE_PARSER: Mutex<MousePacketParser> = Mutex::new(MousePacketParser::new());

// ----------------------------------------------------------------------------
// PS/2 Mouse Packet Decoder
// ----------------------------------------------------------------------------

/// Represents a parsed PS/2 mouse motion and button event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MousePacket {
    pub left_button: bool,
    pub right_button: bool,
    pub middle_button: bool,
    pub dx: i16,
    pub dy: i16,
    pub dz: i8,
}

/// State machine to assemble multi-byte PS/2 mouse packets.
pub struct MousePacketParser {
    raw: [u8; 4],
    idx: usize,
    intellimouse: bool,
}

impl MousePacketParser {
    pub const fn new() -> Self {
        Self {
            raw: [0; 4],
            idx: 0,
            intellimouse: false,
        }
    }

    pub fn set_intellimouse(&mut self, enabled: bool) {
        self.intellimouse = enabled;
    }

    /// Feeds a single byte from port 0x60 into the parser.
    ///
    /// Returns `Some(MousePacket)` when a complete, synchronized packet is assembled.
    pub fn feed(&mut self, byte: u8) -> Option<MousePacket> {
        // First byte must have bit 3 set to 1. If not, discard to resynchronize stream.
        if self.idx == 0 && (byte & 0x08) == 0 {
            return None;
        }

        self.raw[self.idx] = byte;
        self.idx += 1;

        let packet_len = if self.intellimouse { 4 } else { 3 };

        if self.idx >= packet_len {
            self.idx = 0;
            let b0 = self.raw[0];
            let b1 = self.raw[1];
            let b2 = self.raw[2];

            let left = (b0 & 0x01) != 0;
            let right = (b0 & 0x02) != 0;
            let middle = (b0 & 0x04) != 0;

            // X and Y sign extension (9-bit signed values)
            let mut dx = b1 as i16;
            if (b0 & 0x10) != 0 {
                dx |= !0xFF;
            }

            let mut dy = b2 as i16;
            if (b0 & 0x20) != 0 {
                dy |= !0xFF;
            }

            let dz = if self.intellimouse {
                self.raw[3] as i8
            } else {
                0
            };

            Some(MousePacket {
                left_button: left,
                right_button: right,
                middle_button: middle,
                dx,
                dy,
                dz,
            })
        } else {
            None
        }
    }

    pub fn current_raw_packet(&self) -> &[u8] {
        let len = if self.intellimouse { 4 } else { 3 };
        &self.raw[..len]
    }
}

// ----------------------------------------------------------------------------
// Interrupt Handler & Mouse Input Processing
// ----------------------------------------------------------------------------

fn handle_mouse_irq(_frame: &TrapFrame) {
    if let Some(ctrl_lock) = I8042_CONTROLLER.get() {
        let ctrl = ctrl_lock.lock();
        let status = ctrl.read_status();

        // Verify output buffer is full and data is for mouse (AUX buffer full)
        if status.contains(StatusFlags::OUTPUT_FULL) && status.contains(StatusFlags::AUX_FULL) {
            let byte = ctrl.read_data();
            drop(ctrl); // release controller lock

            let maybe_packet = {
                let mut parser = MOUSE_PARSER.lock();
                let res = parser.feed(byte);
                if res.is_some() {
                    // Push raw packet bytes to queue for /dev/psaux
                    let mut buf = MOUSE_BUFFER.lock();
                    for &b in parser.current_raw_packet() {
                        if buf.len() < 2048 {
                            buf.push_back(b);
                        }
                    }
                }
                res
            };

            if let Some(pkt) = maybe_packet {
                // If button clicked or major movement occurred, log info
                if pkt.left_button || pkt.right_button || pkt.middle_button {
                    ostd::info!(
                        "[MOUSE] Click: Left={}, Right={}, Middle={}, dx={}, dy={}",
                        pkt.left_button,
                        pkt.right_button,
                        pkt.middle_button,
                        pkt.dx,
                        pkt.dy
                    );
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Unified Device Model Trait Implementation
// ----------------------------------------------------------------------------

pub struct MouseDevice;

impl CharacterDevice for MouseDevice {
    fn name(&self) -> String {
        "mouse".into()
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut mbuf = MOUSE_BUFFER.lock();
        let mut count = 0;
        while count < buf.len() {
            if let Some(byte) = mbuf.pop_front() {
                buf[count] = byte;
                count += 1;
            } else {
                break;
            }
        }
        Ok(count)
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        Ok(buf.len())
    }
}

// ----------------------------------------------------------------------------
// Initialization & Detection
// ----------------------------------------------------------------------------

/// Probes and initializes the PS/2 mouse, binds ISA IRQ 12,
/// and registers the driver.
pub fn init(controller: &I8042Controller) -> Result<()> {
    ostd::info!("PS/2 Mouse: Initializing...");

    // 1. Reset mouse and query initial device ID
    let _initial_id = match reset_mouse(controller) {
        Ok(id) => {
            ostd::info!(
                "PS/2 Mouse: Reset successful, initial device ID: 0x{:02X}",
                id
            );
            id
        }
        Err(err) => {
            ostd::warn!(
                "PS/2 Mouse: Reset error (proceeding with standard): {:?}",
                err
            );
            0x00
        }
    };
    controller.flush_buffer();

    // 2. Attempt IntelliMouse detection sequence (sample rates: 200, 100, 80)
    let mut is_intellimouse = false;
    let _ = send_mouse_cmd_arg(controller, CMD_SET_SAMPLE_RATE, 200);
    let _ = send_mouse_cmd_arg(controller, CMD_SET_SAMPLE_RATE, 100);
    let _ = send_mouse_cmd_arg(controller, CMD_SET_SAMPLE_RATE, 80);

    if send_mouse_cmd(controller, CMD_GET_DEVICE_ID).is_ok() {
        if let Ok(new_id) = controller.wait_and_read_data() {
            if new_id == DEV_ID_MOUSE_INTELLIMOUSE {
                is_intellimouse = true;
                ostd::info!("PS/2 Mouse: IntelliMouse detected with scroll wheel (ID: 0x03)");
            } else {
                ostd::info!(
                    "PS/2 Mouse: Standard 3-button mouse detected (ID: 0x{:02X})",
                    new_id
                );
            }
        }
    }
    controller.flush_buffer();

    let mut parser = MOUSE_PARSER.lock();
    parser.set_intellimouse(is_intellimouse);
    drop(parser);

    // 3. Set default sample rate (100 Hz) and enable data streaming
    let _ = send_mouse_cmd_arg(controller, CMD_SET_SAMPLE_RATE, 100);
    if let Err(err) = send_mouse_cmd(controller, CMD_ENABLE_SCANNING) {
        ostd::warn!("PS/2 Mouse: Failed to enable data reporting: {:?}", err);
    }
    controller.flush_buffer();

    // 4. Allocate and map ISA IRQ 12
    if let Some(chip) = IRQ_CHIP.get() {
        if let Ok(irq_line) = IrqLine::alloc() {
            match chip.map_isa_pin_to(irq_line, ISA_MOUSE_IRQ) {
                Ok(mut mapped_line) => {
                    mapped_line.on_active(handle_mouse_irq);
                    MOUSE_IRQ_LINE.call_once(|| mapped_line);
                    ostd::info!("PS/2 Mouse: Mapped ISA IRQ {} successfully", ISA_MOUSE_IRQ);
                }
                Err(err) => {
                    ostd::warn!(
                        "PS/2 Mouse: Failed to map ISA IRQ {}: {:?}",
                        ISA_MOUSE_IRQ,
                        err
                    );
                }
            }
        }
    }

    // 5. Register into unified driver registry
    crate::drivers::register_chrdev(Arc::new(MouseDevice));

    ostd::info!("PS/2 Mouse: Driver registered successfully.");
    Ok(())
}
