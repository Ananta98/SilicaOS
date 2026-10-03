// SPDX-License-Identifier: GPL-2.0

//! Intel e1000 Network Device Implementation.

use alloc::{string::String, sync::Arc};
use core::mem::size_of;
use ostd::{
    io::IoMem,
    mm::{HasPaddr, VmIoOnce, dma::DmaCoherent, io::util::HasVmReaderWriter},
};
use spin::Mutex;

use super::{hw::*, regs::*};
use crate::{
    api::errno::{Errno, Result},
    drivers::net::NetDevice,
};

/// Internal mutable ring state for e1000.
struct E1000Rings {
    rx_dma: DmaCoherent,
    rx_buf_dma: DmaCoherent,
    rx_cur: usize,

    tx_dma: DmaCoherent,
    tx_buf_dma: DmaCoherent,
    tx_cur: usize,
}

/// Intel e1000 Gigabit Ethernet Network Device.
pub struct E1000Device {
    name: String,
    mac: [u8; 6],
    mmio: IoMem,
    rings: Mutex<E1000Rings>,
}

impl E1000Device {
    /// Probe, initialize, and construct a new [`E1000Device`].
    pub fn new(name: String, mmio: IoMem) -> Result<Arc<Self>> {
        // 1. Reset Controller
        let ctrl: u32 = mmio.read_once(REG_CTRL).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_CTRL, &(ctrl | CTRL_RST)).map_err(|_| Errno::EIO)?;

        // Wait a few cycles for reset completion
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }

        // 2. Set Link Up and Auto-Speed Detection
        let new_ctrl = CTRL_SLU | CTRL_ASDE | CTRL_FD;
        mmio.write_once(REG_CTRL, &new_ctrl).map_err(|_| Errno::EIO)?;

        // 3. Clear Multicast Table Array (MTA)
        for i in 0..128 {
            mmio.write_once(REG_MTA + i * 4, &0u32).map_err(|_| Errno::EIO)?;
        }

        // 4. Read MAC Address
        let mut mac = [0u8; 6];
        let ral: u32 = mmio.read_once(REG_RAL).unwrap_or(0);
        let rah: u32 = mmio.read_once(REG_RAH).unwrap_or(0);

        if ral != 0 || (rah & 0xFFFF) != 0 {
            mac[0] = (ral & 0xFF) as u8;
            mac[1] = ((ral >> 8) & 0xFF) as u8;
            mac[2] = ((ral >> 16) & 0xFF) as u8;
            mac[3] = ((ral >> 24) & 0xFF) as u8;
            mac[4] = (rah & 0xFF) as u8;
            mac[5] = ((rah >> 8) & 0xFF) as u8;
        } else {
            // Fallback default MAC if unconfigured in registers
            mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        }

        // 5. Initialize RX Ring and Buffers
        let rx_desc_bytes = NUM_RX_DESCRIPTORS * size_of::<RxDesc>();
        let rx_desc_frames = (rx_desc_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let rx_dma = DmaCoherent::alloc(rx_desc_frames, true).map_err(|_| Errno::ENOMEM)?;

        let rx_buf_bytes = NUM_RX_DESCRIPTORS * RX_BUFFER_SIZE;
        let rx_buf_frames = (rx_buf_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let rx_buf_dma = DmaCoherent::alloc(rx_buf_frames, true).map_err(|_| Errno::ENOMEM)?;

        let rx_paddr = rx_dma.paddr() as u64;
        let rx_buf_base = rx_buf_dma.paddr() as u64;

        // Initialize descriptors
        let mut rx_writer = rx_dma.writer();
        for i in 0..NUM_RX_DESCRIPTORS {
            let buf_addr = rx_buf_base + (i * RX_BUFFER_SIZE) as u64;
            let desc = RxDesc {
                addr: buf_addr,
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            };
            let _ = rx_writer.write_val(&desc.to_bytes());
        }

        // Program RX registers
        mmio.write_once(REG_RDBAL, &(rx_paddr as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_RDBAH, &((rx_paddr >> 32) as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_RDLEN, &(rx_desc_bytes as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_RDH, &0u32).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_RDT, &((NUM_RX_DESCRIPTORS - 1) as u32)).map_err(|_| Errno::EIO)?;

        // Enable Receiver
        let rctl = RCTL_EN | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC | RCTL_UPE | RCTL_MPE;
        mmio.write_once(REG_RCTL, &rctl).map_err(|_| Errno::EIO)?;

        // 6. Initialize TX Ring and Buffers
        let tx_desc_bytes = NUM_TX_DESCRIPTORS * size_of::<TxDesc>();
        let tx_desc_frames = (tx_desc_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let tx_dma = DmaCoherent::alloc(tx_desc_frames, true).map_err(|_| Errno::ENOMEM)?;

        let tx_buf_bytes = NUM_TX_DESCRIPTORS * TX_BUFFER_SIZE;
        let tx_buf_frames = (tx_buf_bytes + ostd::mm::PAGE_SIZE - 1) / ostd::mm::PAGE_SIZE;
        let tx_buf_dma = DmaCoherent::alloc(tx_buf_frames, true).map_err(|_| Errno::ENOMEM)?;

        let tx_paddr = tx_dma.paddr() as u64;
        let tx_buf_base = tx_buf_dma.paddr() as u64;

        // Initialize TX descriptors with buffer addresses
        let mut tx_writer = tx_dma.writer();
        for i in 0..NUM_TX_DESCRIPTORS {
            let buf_addr = tx_buf_base + (i * TX_BUFFER_SIZE) as u64;
            let desc = TxDesc {
                addr: buf_addr,
                length: 0,
                cso: 0,
                cmd: 0,
                status: TDESC_STAT_DD,
                css: 0,
                special: 0,
            };
            let _ = tx_writer.write_val(&desc.to_bytes());
        }

        // Program TX registers
        mmio.write_once(REG_TDBAL, &(tx_paddr as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_TDBAH, &((tx_paddr >> 32) as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_TDLEN, &(tx_desc_bytes as u32)).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_TDH, &0u32).map_err(|_| Errno::EIO)?;
        mmio.write_once(REG_TDT, &0u32).map_err(|_| Errno::EIO)?;

        // Configure Inter-Packet Gap (TIPG)
        let tipg: u32 = 10 | (10 << 10) | (10 << 20);
        mmio.write_once(REG_TIPG, &tipg).map_err(|_| Errno::EIO)?;

        // Enable Transmitter
        let tctl: u32 = TCTL_EN | TCTL_PSP | (15 << TCTL_CT_SHIFT) | (64 << TCTL_COLD_SHIFT) | TCTL_RTLC;
        mmio.write_once(REG_TCTL, &tctl).map_err(|_| Errno::EIO)?;

        // Disable interrupts for simplicity (polling mode driver)
        mmio.write_once(REG_IMC, &0xFFFFFFFFu32).map_err(|_| Errno::EIO)?;

        Ok(Arc::new(Self {
            name,
            mac,
            mmio,
            rings: Mutex::new(E1000Rings {
                rx_dma,
                rx_buf_dma,
                rx_cur: 0,
                tx_dma,
                tx_buf_dma,
                tx_cur: 0,
            }),
        }))
    }
}

impl NetDevice for E1000Device {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn can_transmit(&self) -> bool {
        let rings = self.rings.lock();
        let desc_offset = rings.tx_cur * size_of::<TxDesc>();
        let mut reader = rings.tx_dma.reader();
        reader.skip(desc_offset);
        if let Ok(bytes) = reader.read_val::<[u8; 16]>() {
            let desc = TxDesc::from_bytes(bytes);
            (desc.status & TDESC_STAT_DD) != 0
        } else {
            false
        }
    }

    fn transmit(&self, packet: &[u8]) -> Result<()> {
        if packet.len() > TX_BUFFER_SIZE {
            crate::return_errno!(EMSGSIZE, "packet exceeds TX buffer size");
        }

        let mut rings = self.rings.lock();
        let slot = rings.tx_cur;

        // Copy payload to DMA TX buffer
        let buf_offset = slot * TX_BUFFER_SIZE;
        let mut buf_writer = rings.tx_buf_dma.writer();
        buf_writer.skip(buf_offset);
        let mut reader = ostd::mm::VmReader::from(packet);
        buf_writer.write(&mut reader);

        // Write TX Descriptor
        let tx_buf_addr = (rings.tx_buf_dma.paddr() as u64) + buf_offset as u64;
        let desc = TxDesc {
            addr: tx_buf_addr,
            length: packet.len() as u16,
            cso: 0,
            cmd: TDESC_CMD_EOP | TDESC_CMD_IFCS | TDESC_CMD_RS,
            status: 0,
            css: 0,
            special: 0,
        };

        let desc_offset = slot * size_of::<TxDesc>();
        let mut desc_writer = rings.tx_dma.writer();
        desc_writer.skip(desc_offset);
        desc_writer.write_val(&desc.to_bytes()).map_err(|_| Errno::EIO)?;

        // Update tail register
        rings.tx_cur = (slot + 1) % NUM_TX_DESCRIPTORS;
        let next_tail = rings.tx_cur as u32;
        self.mmio.write_once(REG_TDT, &next_tail).map_err(|_| Errno::EIO)?;

        Ok(())
    }

    fn receive(&self, buf: &mut [u8]) -> Result<usize> {
        let mut rings = self.rings.lock();
        let cur = rings.rx_cur;

        // Read descriptor status
        let desc_offset = cur * size_of::<RxDesc>();
        let mut desc_reader = rings.rx_dma.reader();
        desc_reader.skip(desc_offset);
        let desc_bytes = desc_reader.read_val::<[u8; 16]>().map_err(|_| Errno::EIO)?;
        let desc = RxDesc::from_bytes(desc_bytes);

        // Check Descriptor Done (DD) bit
        if (desc.status & RDESC_STAT_DD) == 0 {
            return Ok(0); // No packet available
        }

        let pkt_len = desc.length as usize;
        let copy_len = pkt_len.min(buf.len());

        // Read packet from RX buffer
        let buf_offset = cur * RX_BUFFER_SIZE;
        let mut buf_reader = rings.rx_buf_dma.reader();
        buf_reader.skip(buf_offset);
        let mut writer = ostd::mm::VmWriter::from(&mut buf[..copy_len]);
        buf_reader.read(&mut writer);

        // Reset descriptor status
        let mut desc_writer = rings.rx_dma.writer();
        desc_writer.skip(desc_offset);
        let mut new_desc = desc;
        new_desc.status = 0;
        desc_writer.write_val(&new_desc.to_bytes()).map_err(|_| Errno::EIO)?;

        // Advance tail register to return descriptor to hardware
        self.mmio.write_once(REG_RDT, &(cur as u32)).map_err(|_| Errno::EIO)?;
        rings.rx_cur = (cur + 1) % NUM_RX_DESCRIPTORS;

        Ok(copy_len)
    }

    fn mtu(&self) -> usize {
        1500
    }
}
