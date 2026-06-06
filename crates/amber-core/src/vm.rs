//! The backend-agnostic VM: load the kernel and DTB, wire the bus, create the
//! vcpu, and run the exit loop. This is the file that proves the seam works:
//! it names no hypervisor, only the `Hypervisor`/`Vcpu` traits.

use crate::bus::{MmioBus, Pl011};
use crate::hypervisor::{Hypervisor, MmioOp, Vcpu, VmExit};
use crate::{dtb, layout, loader, GuestMemory, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct VmConfig {
    pub mem_size: usize,
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Host image file to back the guest's virtio-blk root device (e.g. a
    /// squashfs base). None means no block device.
    pub disk: Option<PathBuf>,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            mem_size: 512 * 1024 * 1024,
            kernel: Vec::new(),
            initrd: None,
            // earlycon prints before the GIC exists; console=ttyAMA0 is the real
            // tty once the PL011 driver binds, giving an interactive console.
            cmdline: "earlycon=pl011,0x9000000 console=ttyAMA0".into(),
            disk: None,
        }
    }
}

pub struct Vm {
    mem: GuestMemory,
    bus: MmioBus,
    pl011: Pl011,
    blk: Option<crate::virtio::VirtioBlk>,
    entry: u64,
    dtb_addr: u64,
    // Kept so the device tree can be built in `run`, once the backend has
    // created its interrupt controller and we know the real GIC layout.
    mem_size: u64,
    cmdline: String,
    initrd: Option<(u64, u64)>,
}

impl Vm {
    /// Place the kernel and initramfs in guest RAM and wire the device bus. The
    /// DTB is deferred to `run`, because its interrupt-controller node depends on
    /// the GIC the backend creates.
    pub fn prepare(cfg: &VmConfig) -> Result<Self> {
        let mem = GuestMemory::new(layout::RAM_BASE, cfg.mem_size)?;

        let kernel = loader::load_kernel(&mem, &cfg.kernel)?;
        let initrd = loader::load_initramfs(&mem, cfg.initrd.as_deref())?;
        let dtb_addr = mem.base() + layout::DTB_OFFSET;

        let blk = match &cfg.disk {
            Some(path) => Some(crate::virtio::VirtioBlk::open(path)?),
            None => None,
        };

        Ok(Self {
            mem,
            bus: MmioBus::default(),
            pl011: Pl011::new(Box::new(std::io::stdout())),
            blk,
            entry: kernel.entry,
            dtb_addr,
            mem_size: cfg.mem_size as u64,
            cmdline: cfg.cmdline.clone(),
            initrd,
        })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.mem
    }

    /// Boot and run on backend `H` until shutdown or a fatal fault.
    ///
    /// Two threads. The vcpu runs synchronously on this one. A second thread
    /// reads host stdin and feeds the UART: with a GIC present the host
    /// hypervisor parks the vcpu inside `run` on a guest WFI and wakes it on an
    /// interrupt, so console input has to arrive as an interrupt. The reader
    /// pushes bytes into the PL011 and raises its GIC line, which wakes the vcpu;
    /// that is what makes the console interactive rather than output-only.
    pub fn run<H: Hypervisor + Sync>(self) -> Result<()> {
        let Vm {
            mem,
            mut bus,
            pl011,
            blk,
            entry,
            dtb_addr,
            mem_size,
            cmdline,
            initrd,
        } = self;

        let mut hv = H::create(&mem)?;

        // Give the block device its view of guest RAM (it reads rings/buffers).
        let mut blk = blk;
        if let Some(b) = &mut blk {
            b.attach(mem.ram());
        }

        // Seed the guest's RNG from the host so crng inits at once (no entropy
        // source in a microVM otherwise). Best effort: an unseeded boot just
        // stalls a little, it does not fail.
        let mut rng_seed = [0u8; 64];
        let _ = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut rng_seed));

        // Build the device tree now that the backend exists: its GIC node must
        // match the interrupt controller the backend just created.
        let blob = dtb::build(&dtb::DtbParams {
            mem_size,
            cmdline: &cmdline,
            initrd,
            gic: hv.gic_info(),
            virtio_blk: blk.is_some(),
            rng_seed: &rng_seed,
        })?;
        mem.write(dtb_addr, &blob)?;

        let mut vcpu = hv.create_vcpu(0)?;
        vcpu.set_boot_regs(entry, dtb_addr)?;

        let pl011_lo = layout::PL011_BASE;
        let pl011_hi = layout::PL011_BASE + layout::PL011_SIZE;
        let pl011_intid = layout::GIC_SPI_BASE + layout::PL011_SPI;
        let virtio_lo = layout::VIRTIO_BLK_BASE;
        let virtio_hi = layout::VIRTIO_BLK_BASE + layout::VIRTIO_BLK_SIZE;
        let virtio_intid = layout::GIC_SPI_BASE + layout::VIRTIO_BLK_SPI;

        let pl011 = Mutex::new(pl011);
        let running = AtomicBool::new(true);
        let hv = &hv;
        let pl011 = &pl011;
        let running = &running;

        std::thread::scope(|s| {
            // Console reader: host stdin -> UART RX -> GIC line -> wake the vcpu.
            s.spawn(move || console_reader(hv, pl011, pl011_intid, running));

            // vcpu loop on this thread.
            let mut step = || -> Result<bool> {
                match vcpu.run()? {
                    VmExit::Mmio { access } => {
                        let in_pl011 = access.ipa >= pl011_lo && access.ipa < pl011_hi;
                        let in_virtio = access.ipa >= virtio_lo && access.ipa < virtio_hi;
                        match access.op {
                            MmioOp::Write { value, .. } => {
                                if in_pl011 {
                                    pl011.lock().unwrap().write(access.ipa - pl011_lo, access.size, value);
                                } else if in_virtio {
                                    if let Some(b) = &mut blk {
                                        b.write(access.ipa - virtio_lo, access.size, value);
                                    }
                                } else {
                                    bus.write(access.ipa, access.size, value);
                                }
                            }
                            MmioOp::Read { reg } => {
                                let v = if in_pl011 {
                                    pl011.lock().unwrap().read(access.ipa - pl011_lo, access.size)
                                } else if in_virtio {
                                    blk.as_mut()
                                        .map(|b| b.read(access.ipa - virtio_lo, access.size))
                                        .unwrap_or(0)
                                } else {
                                    bus.read(access.ipa, access.size)
                                };
                                vcpu.set_x(reg, v)?;
                            }
                        }
                        // An access may have changed a device's interrupt line.
                        if in_pl011 {
                            let lvl = pl011.lock().unwrap().irq_level();
                            hv.set_irq(pl011_intid, lvl)?;
                        } else if in_virtio {
                            if let Some(b) = &blk {
                                hv.set_irq(virtio_intid, b.irq_level())?;
                            }
                        }
                        // Step past the faulting load/store: arm64 instructions
                        // are 4 bytes, and a syndrome-decodable access is one insn.
                        let pc = vcpu.pc()?;
                        vcpu.set_pc(pc + 4)?;
                        Ok(true)
                    }
                    // With a GIC the hypervisor handles WFI internally; an Idle
                    // exit only arrives from an explicit cancel. Nothing to do.
                    VmExit::Idle => Ok(true),
                    VmExit::Shutdown => {
                        log::info!("guest requested shutdown");
                        Ok(false)
                    }
                    VmExit::Fault { pc, esr, ipa } => Err(crate::Error::GuestFault { pc, esr, ipa }),
                }
            };

            let mut result = Ok(());
            loop {
                match step() {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            // Let the reader thread exit so the scope can join it.
            running.store(false, Ordering::Relaxed);
            result
        })
    }
}

/// Read host stdin and deliver bytes to the guest UART, raising its GIC line so a
/// parked vcpu wakes. Polls with a timeout so it can notice shutdown; stops
/// feeding on EOF (the timer still drives the guest). Reads fd 0 directly, which
/// couples amber-core to "stdin is the console" — true for `amber boot`, and the
/// control plane will replace it later.
fn console_reader<H: Hypervisor>(
    hv: &H,
    pl011: &Mutex<Pl011>,
    intid: u32,
    running: &AtomicBool,
) {
    let mut buf = [0u8; 64];
    while running.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
        if n <= 0 {
            continue;
        }
        let r = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if r <= 0 {
            break; // EOF or error: stop feeding input
        }
        let level = {
            let mut p = pl011.lock().unwrap();
            for b in &buf[..r as usize] {
                p.push_rx(*b);
            }
            p.irq_level()
        };
        let _ = hv.set_irq(intid, level);
    }
}
