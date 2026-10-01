// SPDX-License-Identifier: GPL-2.0

//! NVM Express (NVMe) PCIe Storage Controller Driver.
//!
//! Provides support for NVMe storage devices, initialized via the kernel module
//! system ([`KernelModule`]).

pub mod commands;
pub mod identify;
pub mod namespace;
pub mod queue;
pub mod regs;

use alloc::{format, sync::Arc, vec::Vec};
use spin::Mutex;

use ostd::{
    io::IoMem,
    mm::{HasPaddr, VmIoOnce, VmWriter, dma::DmaCoherent, io::util::HasVmReaderWriter},
};

use self::{
    commands::NvmeSqe,
    namespace::NvmeNamespace,
    queue::{DEFAULT_QUEUE_SIZE, NvmeQueuePair},
    regs::*,
};
use crate::{
    drivers::{
        block::register_block_device,
        bus::pci::{self, PciDevice},
    },
    errno::{Errno, Result},
};

/// High-level NVMe Controller handle.
pub struct NvmeController {
    pub mmio: IoMem,
    pub admin_queue: Arc<Mutex<NvmeQueuePair>>,
    pub io_queue: Arc<Mutex<NvmeQueuePair>>,
    pub namespaces: Vec<Arc<NvmeNamespace>>,
}

/// Initializes an NVMe controller discovered on the PCI bus.
pub fn probe_controller(dev: &PciDevice) -> Result<Arc<NvmeController>> {
    ostd::info!("NVMe: Initializing controller at PCI {}", dev.address);

    // 1. Enable Bus Mastering (DMA) and Memory Mapped I/O
    dev.enable_bus_mastering();
    dev.enable_memory_space();

    // 2. Locate BAR0 (Memory BAR for NVMe Registers)
    let bar0 = dev.bars[0].ok_or_else(|| {
        ostd::error!("NVMe: Controller has no valid BAR0");
        Errno::ENODEV
    })?;

    let mmio_range = (bar0.address as usize)..(bar0.address as usize + bar0.size as usize);
    let mmio = IoMem::acquire(mmio_range).map_err(|_| {
        ostd::error!("NVMe: Failed to map BAR0 MMIO region");
        Errno::EIO
    })?;

    // 3. Read Controller Capabilities (CAP)
    let _cap_low: u32 = mmio.read_once(NVME_REG_CAP).map_err(|_| Errno::EIO)?;
    let cap_high: u32 = mmio.read_once(NVME_REG_CAP + 4).map_err(|_| Errno::EIO)?;
    let dstrd = ((cap_high >> 0) & 0x0F) as usize;
    let doorbell_stride = 4 << dstrd;

    // 4. Reset Controller: Clear CC.EN, wait for CSTS.RDY == 0
    let cc_val: u32 = mmio.read_once(NVME_REG_CC).map_err(|_| Errno::EIO)?;
    if (cc_val & NVME_CC_EN) != 0 {
        mmio.write_once(NVME_REG_CC, &(cc_val & !NVME_CC_EN))
            .map_err(|_| Errno::EIO)?;
    }

    let mut timeout = 1_000_000;
    while timeout > 0 {
        let csts: u32 = mmio.read_once(NVME_REG_CSTS).map_err(|_| Errno::EIO)?;
        if (csts & NVME_CSTS_RDY) == 0 {
            break;
        }
        core::hint::spin_loop();
        timeout -= 1;
    }

    // 5. Initialize Admin Queue Pair (QID 0)
    let admin_queue = Arc::new(Mutex::new(NvmeQueuePair::new(
        0,
        DEFAULT_QUEUE_SIZE,
        doorbell_stride,
    )?));

    // 6. Write Admin Queue Configuration to Controller
    let aqa = ((DEFAULT_QUEUE_SIZE - 1) as u32) | (((DEFAULT_QUEUE_SIZE - 1) as u32) << 16);
    mmio.write_once(NVME_REG_AQA, &aqa)
        .map_err(|_| Errno::EIO)?;

    let (asq_pa, acq_pa) = {
        let q = admin_queue.lock();
        (q.sq_paddr, q.cq_paddr)
    };
    mmio.write_once(NVME_REG_ASQ, &(asq_pa as u32))
        .map_err(|_| Errno::EIO)?;
    mmio.write_once(NVME_REG_ASQ + 4, &((asq_pa >> 32) as u32))
        .map_err(|_| Errno::EIO)?;
    mmio.write_once(NVME_REG_ACQ, &(acq_pa as u32))
        .map_err(|_| Errno::EIO)?;
    mmio.write_once(NVME_REG_ACQ + 4, &((acq_pa >> 32) as u32))
        .map_err(|_| Errno::EIO)?;

    // 7. Enable Controller: Set CC.EN with standard 4K page size, 64B SQE, 16B CQE
    let new_cc =
        NVME_CC_EN | NVME_CC_CSS_NVM | NVME_CC_MPS_4K | NVME_CC_IOSQES_64 | NVME_CC_IOCQES_16;
    mmio.write_once(NVME_REG_CC, &new_cc)
        .map_err(|_| Errno::EIO)?;

    // Wait for CSTS.RDY == 1
    timeout = 1_000_000;
    while timeout > 0 {
        let csts: u32 = mmio.read_once(NVME_REG_CSTS).map_err(|_| Errno::EIO)?;
        if (csts & NVME_CSTS_RDY) != 0 {
            break;
        }
        core::hint::spin_loop();
        timeout -= 1;
    }

    // 8. Identify Controller via Admin Queue
    let id_dma = DmaCoherent::alloc(1, true).map_err(|_| Errno::ENOMEM)?;
    let id_sqe = NvmeSqe::identify(0, 1, id_dma.paddr() as u64, 0);
    admin_queue.lock().submit_and_wait(id_sqe, &mmio)?;

    let mut id_buf = [0u8; 4096];
    let mut reader = id_dma.reader();
    let mut id_writer = VmWriter::from(&mut id_buf[..]);
    let _ = reader.read(&mut id_writer);

    let model = core::str::from_utf8(&id_buf[24..64])
        .unwrap_or("NVMe SSD")
        .trim();
    let num_namespaces =
        u32::from_le_bytes(id_buf[516..520].try_into().unwrap_or([1, 0, 0, 0])).max(1);

    ostd::info!(
        "NVMe: Discovered controller \"{}\" with {} namespace(s)",
        model,
        num_namespaces
    );

    // 9. Create I/O Queue Pair (QID 1)
    let io_queue = Arc::new(Mutex::new(NvmeQueuePair::new(
        1,
        DEFAULT_QUEUE_SIZE,
        doorbell_stride,
    )?));
    let (io_sq_pa, io_cq_pa) = {
        let q = io_queue.lock();
        (q.sq_paddr, q.cq_paddr)
    };

    // Create I/O CQ
    let create_cq_sqe = NvmeSqe::create_io_cq(1, DEFAULT_QUEUE_SIZE, io_cq_pa, 0);
    admin_queue.lock().submit_and_wait(create_cq_sqe, &mmio)?;

    // Create I/O SQ
    let create_sq_sqe = NvmeSqe::create_io_sq(1, 1, DEFAULT_QUEUE_SIZE, io_sq_pa, 0);
    admin_queue.lock().submit_and_wait(create_sq_sqe, &mmio)?;

    // 10. Identify and register active namespaces
    let mut namespaces = Vec::new();
    for nsid in 1..=num_namespaces.min(4) {
        let ns_dma = DmaCoherent::alloc(1, true).map_err(|_| Errno::ENOMEM)?;
        let ns_sqe = NvmeSqe::identify(nsid, 0, ns_dma.paddr() as u64, 0);

        if admin_queue.lock().submit_and_wait(ns_sqe, &mmio).is_ok() {
            let mut ns_buf = [0u8; 4096];
            let mut ns_reader = ns_dma.reader();
            let mut ns_writer = VmWriter::from(&mut ns_buf[..]);
            let _ = ns_reader.read(&mut ns_writer);

            let block_count = u64::from_le_bytes(ns_buf[0..8].try_into().unwrap_or([0; 8]));
            let flbas = ns_buf[26];
            let ds = ns_buf[128 + ((flbas & 0x0F) as usize) * 4 + 2];
            let block_size = if ds >= 9 && ds <= 16 { 1 << ds } else { 512 };

            if block_count > 0 {
                let name = format!("nvme0n{}", nsid);
                let ns = Arc::new(NvmeNamespace {
                    nsid,
                    name: name.clone(),
                    block_size,
                    block_count,
                    io_queue: Arc::clone(&io_queue),
                    mmio: mmio.clone(),
                });

                register_block_device(&name, Arc::clone(&ns) as _);
                namespaces.push(ns);
            }
        }
    }

    Ok(Arc::new(NvmeController {
        mmio,
        admin_queue,
        io_queue,
        namespaces,
    }))
}

// ----------------------------------------------------------------------------
// Kernel Module Declaration via module! macro
// ----------------------------------------------------------------------------

fn init() -> Result<()> {
    if let Some(dev) = pci::find_nvme_device() {
        probe_controller(&dev)?;
        Ok(())
    } else {
        ostd::info!("NVMe: No NVMe controller found on PCI bus");
        Ok(())
    }
}

crate::module!("NVMe PCIe Storage Driver", "Ananta98", init);
