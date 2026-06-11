//! The minimal device tree. Built with vm-fdt (the rust-vmm FDT writer, the same
//! one Firecracker uses) so we are not hand-rolling the binary format.
//!
//! For M0 the kernel only needs: a memory node so it knows where RAM is, a cpu,
//! psci so it can power down cleanly, and the pl011 plus a `chosen/bootargs` with
//! `earlycon` so it can print before any interrupt controller is alive. The gic
//! and timer nodes are included so the DTB is well formed, but interrupts are not
//! functional until the GIC is wired in (M0.5). The kernel will print its early
//! boot messages over earlycon regardless, which is the M0 success signal.

use crate::layout;
use crate::{Error, Result};
use vm_fdt::FdtWriter;

const GIC_PHANDLE: u32 = 1;

pub struct DtbParams<'a> {
    pub mem_size: u64,
    /// Guest CPUs; every node gets `enable-method = "psci"` (CPU_ON boots the
    /// secondaries).
    pub vcpus: usize,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    /// The backend's GIC, if it created one. Some -> a functional GICv3 node;
    /// None -> the non-backed GICv2 stub that only keeps the tree well-formed.
    pub gic: Option<crate::GicInfo>,
    /// virtio-mmio devices to advertise, as `(base, size, relative-SPI)`.
    pub virtio: &'a [(u64, u64, u32)],
    /// Random bytes for `/chosen/rng-seed`. The kernel credits these as entropy
    /// at early boot, so crng initializes immediately instead of stalling for
    /// seconds waiting for a source the microVM does not have.
    pub rng_seed: &'a [u8],
}

pub fn build(p: &DtbParams) -> Result<Vec<u8>> {
    let mut fdt = FdtWriter::new().map_err(fdt_err)?;

    let root = fdt.begin_node("").map_err(fdt_err)?;
    fdt.property_string("compatible", "linux,dummy-virt").map_err(fdt_err)?;
    fdt.property_u32("#address-cells", 2).map_err(fdt_err)?;
    fdt.property_u32("#size-cells", 2).map_err(fdt_err)?;
    fdt.property_string("model", "amber").map_err(fdt_err)?;

    // /chosen
    let chosen = fdt.begin_node("chosen").map_err(fdt_err)?;
    fdt.property_string("bootargs", p.cmdline).map_err(fdt_err)?;
    fdt.property_string("stdout-path", "/pl011@9000000").map_err(fdt_err)?;
    if let Some((start, end)) = p.initrd {
        fdt.property_u64("linux,initrd-start", start).map_err(fdt_err)?;
        fdt.property_u64("linux,initrd-end", end).map_err(fdt_err)?;
    }
    if !p.rng_seed.is_empty() {
        fdt.property("rng-seed", p.rng_seed).map_err(fdt_err)?;
    }
    fdt.end_node(chosen).map_err(fdt_err)?;

    // /memory
    let mem = fdt.begin_node("memory@40000000").map_err(fdt_err)?;
    fdt.property_string("device_type", "memory").map_err(fdt_err)?;
    fdt.property_array_u64("reg", &[layout::RAM_BASE, p.mem_size]).map_err(fdt_err)?;
    fdt.end_node(mem).map_err(fdt_err)?;

    // /cpus: one node per vcpu; `reg` is the MPIDR affinity the backend gives
    // each vcpu, which is how PSCI CPU_ON names its target.
    let cpus = fdt.begin_node("cpus").map_err(fdt_err)?;
    fdt.property_u32("#address-cells", 1).map_err(fdt_err)?;
    fdt.property_u32("#size-cells", 0).map_err(fdt_err)?;
    for i in 0..p.vcpus.max(1) as u32 {
        let cpu = fdt.begin_node(&format!("cpu@{i}")).map_err(fdt_err)?;
        fdt.property_string("device_type", "cpu").map_err(fdt_err)?;
        fdt.property_string("compatible", "arm,arm-v8").map_err(fdt_err)?;
        fdt.property_u32("reg", i).map_err(fdt_err)?;
        fdt.property_string("enable-method", "psci").map_err(fdt_err)?;
        fdt.end_node(cpu).map_err(fdt_err)?;
    }
    fdt.end_node(cpus).map_err(fdt_err)?;

    // /psci: method "hvc". Lets the guest request SYSTEM_OFF, which we trap.
    let psci = fdt.begin_node("psci").map_err(fdt_err)?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".into(), "arm,psci-0.2".into()],
    )
    .map_err(fdt_err)?;
    fdt.property_string("method", "hvc").map_err(fdt_err)?;
    fdt.end_node(psci).map_err(fdt_err)?;

    // /intc: the interrupt controller. GICv3 when the backend created one (reg
    // = distributor + one redistributor region, both sized by the host), else a
    // non-backed GICv2 stub that only keeps the tree well-formed (the M0 path).
    if let Some(g) = p.gic {
        let intc = fdt.begin_node(&format!("intc@{:x}", g.dist_base)).map_err(fdt_err)?;
        // GICv3: distributor + redistributor. GICv2 (amber's software GIC):
        // distributor + CPU interface (the `redist_*` region), as a
        // cortex-a15-gic the kernel's gic-v2 driver binds. GICv2 advertises 3
        // interrupt cells too (type, number, flags), matching the timer node.
        let compatible = match g.kind {
            crate::GicKind::V3 => "arm,gic-v3",
            crate::GicKind::V2 => "arm,cortex-a15-gic",
        };
        fdt.property_string("compatible", compatible).map_err(fdt_err)?;
        fdt.property_u32("#interrupt-cells", 3).map_err(fdt_err)?;
        fdt.property_null("interrupt-controller").map_err(fdt_err)?;
        fdt.property_array_u64(
            "reg",
            &[g.dist_base, g.dist_size, g.redist_base, g.redist_size],
        )
        .map_err(fdt_err)?;
        fdt.property_u32("phandle", GIC_PHANDLE).map_err(fdt_err)?;
        fdt.end_node(intc).map_err(fdt_err)?;
    } else {
        let intc = fdt.begin_node("intc@8000000").map_err(fdt_err)?;
        fdt.property_string("compatible", "arm,cortex-a15-gic").map_err(fdt_err)?;
        fdt.property_u32("#interrupt-cells", 3).map_err(fdt_err)?;
        fdt.property_null("interrupt-controller").map_err(fdt_err)?;
        fdt.property_array_u64(
            "reg",
            &[
                layout::GIC_DIST_BASE, layout::GIC_DIST_SIZE,
                layout::GIC_CPU_BASE, layout::GIC_CPU_SIZE,
            ],
        )
        .map_err(fdt_err)?;
        fdt.property_u32("phandle", GIC_PHANDLE).map_err(fdt_err)?;
        fdt.end_node(intc).map_err(fdt_err)?;
    }

    // /timer: the architected generic timer. PPIs 13..16, edge-triggered.
    let timer = fdt.begin_node("timer").map_err(fdt_err)?;
    fdt.property_string("compatible", "arm,armv8-timer").map_err(fdt_err)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE).map_err(fdt_err)?;
    // <type ppi flags> per cell triple; 1 = PPI, 0xf08 = level/active-low here.
    fdt.property_array_u32(
        "interrupts",
        &[1, 13, 0xf08, 1, 14, 0xf08, 1, 11, 0xf08, 1, 10, 0xf08],
    )
    .map_err(fdt_err)?;
    fdt.end_node(timer).map_err(fdt_err)?;

    // /apb-pclk: a fixed clock the PL011 needs. An AMBA PrimeCell will not probe
    // without an "apb_pclk", and the pl011 driver also wants "uartclk"; both
    // point here. 24 MHz to match the timer the guest already sees.
    const APB_PCLK_PHANDLE: u32 = 2;
    let clk = fdt.begin_node("apb-pclk").map_err(fdt_err)?;
    fdt.property_string("compatible", "fixed-clock").map_err(fdt_err)?;
    fdt.property_u32("#clock-cells", 0).map_err(fdt_err)?;
    fdt.property_u32("clock-frequency", 24_000_000).map_err(fdt_err)?;
    fdt.property_string("clock-output-names", "uartclk").map_err(fdt_err)?;
    fdt.property_u32("phandle", APB_PCLK_PHANDLE).map_err(fdt_err)?;
    fdt.end_node(clk).map_err(fdt_err)?;

    // /pl011: the console. earlycon uses DR/FR; the full driver binds via the
    // PrimeCell IDs and the clocks below, registering ttyAMA0 with RX on SPI 1.
    let uart = fdt.begin_node("pl011@9000000").map_err(fdt_err)?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".into(), "arm,primecell".into()],
    )
    .map_err(fdt_err)?;
    fdt.property_array_u64("reg", &[layout::PL011_BASE, layout::PL011_SIZE]).map_err(fdt_err)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE).map_err(fdt_err)?;
    fdt.property_array_u32("interrupts", &[0, 1, 0x04]).map_err(fdt_err)?;
    fdt.property_array_u32("clocks", &[APB_PCLK_PHANDLE, APB_PCLK_PHANDLE]).map_err(fdt_err)?;
    fdt.property_string_list(
        "clock-names",
        vec!["uartclk".into(), "apb_pclk".into()],
    )
    .map_err(fdt_err)?;
    fdt.end_node(uart).map_err(fdt_err)?;

    // /virtio_mmio: one node per device (block, entropy, ...), each level-high
    // on its SPI in the device hole below RAM.
    for &(base, size, spi) in p.virtio {
        let node = fdt
            .begin_node(&format!("virtio_mmio@{base:x}"))
            .map_err(fdt_err)?;
        fdt.property_string("compatible", "virtio,mmio").map_err(fdt_err)?;
        fdt.property_array_u64("reg", &[base, size]).map_err(fdt_err)?;
        fdt.property_u32("interrupt-parent", GIC_PHANDLE).map_err(fdt_err)?;
        fdt.property_array_u32("interrupts", &[0, spi, 0x04]).map_err(fdt_err)?;
        fdt.end_node(node).map_err(fdt_err)?;
    }

    fdt.end_node(root).map_err(fdt_err)?;
    fdt.finish().map_err(fdt_err)
}

fn fdt_err(e: vm_fdt::Error) -> Error {
    Error::Fdt(format!("{e:?}"))
}
