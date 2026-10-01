// SPDX-License-Identifier: GPL-2.0

//! Common PS/2 Device Protocol Definitions and Commands.
//!
//! Provides protocol constants and command abstractions for PS/2 keyboards,
//! mice, and auxiliary input devices connected to the i8042 microcontroller.

use crate::errno::Result;
use super::controller::I8042Controller;

// ----------------------------------------------------------------------------
// PS/2 Device Standard Commands
// ----------------------------------------------------------------------------

/// Reset the device and perform a power-on self test (BAT).
pub const CMD_RESET: u8 = 0xFF;

/// Request device to resend the previous byte.
pub const CMD_RESEND: u8 = 0xFE;

/// Reset parameters to default settings.
pub const CMD_SET_DEFAULTS: u8 = 0xF6;

/// Disable data streaming / scanning.
pub const CMD_DISABLE_SCANNING: u8 = 0xF5;

/// Enable data streaming / scanning.
pub const CMD_ENABLE_SCANNING: u8 = 0xF4;

/// Set device sampling rate (mouse).
pub const CMD_SET_SAMPLE_RATE: u8 = 0xF3;

/// Query the 1 or 2-byte device identifier.
pub const CMD_GET_DEVICE_ID: u8 = 0xF2;

/// Set keyboard scancode set.
pub const CMD_SET_SCANCODE_SET: u8 = 0xF0;

/// Set keyboard indicator LEDs (ScrollLock, NumLock, CapsLock).
pub const CMD_SET_LEDS: u8 = 0xED;

/// Set mouse resolution.
pub const CMD_SET_RESOLUTION: u8 = 0xE8;

/// Set mouse scaling to 1:1.
pub const CMD_SET_SCALING_1_1: u8 = 0xE6;

/// Set mouse scaling to 2:1.
pub const CMD_SET_SCALING_2_1: u8 = 0xE7;

/// Request device status.
pub const CMD_STATUS_REQUEST: u8 = 0xE9;

// ----------------------------------------------------------------------------
// PS/2 Device Responses
// ----------------------------------------------------------------------------

/// Command acknowledged.
pub const RESP_ACK: u8 = 0xFA;

/// Self-test / Basic Assurance Test passed.
pub const RESP_BAT_OK: u8 = 0xAA;

/// Resend request.
pub const RESP_RESEND: u8 = 0xFE;

/// Device error.
pub const RESP_ERROR: u8 = 0xFC;

// ----------------------------------------------------------------------------
// Known Device Identifiers
// ----------------------------------------------------------------------------

/// Standard 3-button PS/2 mouse.
pub const DEV_ID_MOUSE_STANDARD: u8 = 0x00;

/// Microsoft IntelliMouse with scroll wheel.
pub const DEV_ID_MOUSE_INTELLIMOUSE: u8 = 0x03;

/// 5-button mouse with wheel.
pub const DEV_ID_MOUSE_5BUTTON: u8 = 0x04;

/// Standard PS/2 keyboard first ID byte.
pub const DEV_ID_KEYBOARD: u8 = 0xAB;

// ----------------------------------------------------------------------------
// High-Level PS/2 Command Helpers
// ----------------------------------------------------------------------------

const MAX_COMMAND_RETRIES: usize = 3;

/// Sends a command byte to the primary PS/2 port (Keyboard) and checks for ACK.
pub fn send_keyboard_cmd(controller: &I8042Controller, cmd: u8) -> Result<()> {
    for _ in 0..MAX_COMMAND_RETRIES {
        controller.wait_and_send_data(cmd)?;
        let resp = controller.wait_and_read_data()?;
        if resp == RESP_ACK {
            return Ok(());
        }
        if resp != RESP_RESEND {
            crate::return_errno!(EIO, "keyboard command rejected: unexpected response");
        }
    }
    crate::return_errno!(EIO, "keyboard command retries exhausted");
}

/// Sends a command byte to the secondary PS/2 port (Mouse) and checks for ACK.
pub fn send_mouse_cmd(controller: &I8042Controller, cmd: u8) -> Result<()> {
    for _ in 0..MAX_COMMAND_RETRIES {
        controller.send_to_port2(cmd)?;
        let resp = controller.wait_and_read_data()?;
        if resp == RESP_ACK {
            return Ok(());
        }
        if resp != RESP_RESEND {
            crate::return_errno!(EIO, "mouse command rejected: unexpected response");
        }
    }
    crate::return_errno!(EIO, "mouse command retries exhausted");
}

/// Sends a command byte followed by an argument byte to the secondary PS/2 port.
pub fn send_mouse_cmd_arg(controller: &I8042Controller, cmd: u8, arg: u8) -> Result<()> {
    send_mouse_cmd(controller, cmd)?;
    send_mouse_cmd(controller, arg)?;
    Ok(())
}

/// Resets the device on the secondary PS/2 port (Mouse) and returns the device ID byte.
pub fn reset_mouse(controller: &I8042Controller) -> Result<u8> {
    send_mouse_cmd(controller, CMD_RESET)?;
    let bat_resp = controller.wait_and_read_data()?;
    if bat_resp != RESP_BAT_OK {
        crate::return_errno!(EIO, "mouse BAT failed");
    }
    // Read the mouse ID byte returned after BAT
    let dev_id = controller.wait_and_read_data()?;
    Ok(dev_id)
}
