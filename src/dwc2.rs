//! Direct DWC2 mode control.
//!
//! The kernel exposes `/sys/class/usb_role/<udc>-role-switch/role`, but on this board
//! writing to it is a **silent no-op**. `dwc2_drd_role_sw_set()` (drivers/usb/dwc2/drd.c)
//! bails out before it ever calls `dwc2_force_mode()`:
//!
//! ```text
//! if (role == USB_ROLE_HOST)
//!         already = dwc2_ovr_avalid(hsotg, true);   // -EALREADY if GOTGCTL.ASesVld is set
//! ...
//! if (!already && hsotg->dr_mode == USB_DR_MODE_OTG)
//!         dwc2_force_mode(hsotg, role == USB_ROLE_HOST);
//! ```
//!
//! `dwc2_ovr_avalid()`/`dwc2_ovr_bvalid()` return `-EALREADY` when the session-valid
//! status bit already reads 1, on the assumption that the session bits track the
//! override bits the function is about to write. On a Pi Zero the OTG receptacle's VBUS
//! is tied to the board's 5 V rail (that is why the board can be powered from either
//! micro-USB port), so `GOTGCTL.ASesVld` *and* `GOTGCTL.BSesVld` are stuck high the
//! whole time the Pi is powered. Every role write therefore takes the `-EALREADY` path,
//! `dwc2_force_mode()` is never reached, and the core stays in whatever mode it was
//! forced into at probe — while the sysfs file cheerfully reports the role we asked
//! for. Observed on 6.18.34+rpt-rpi-v8: `GOTGCTL = 0x000d0000` (ASesVld|BSesVld|ConIDSts)
//! and `GUSBCFG = 0x40002700` (ForceDevMode) before *and* after `echo host > role`.
//!
//! So we do what the kernel would have done and set `GUSBCFG.ForceHstMode` /
//! `GUSBCFG.ForceDevMode` ourselves through `/dev/mem`. Flipping those bits makes the
//! core drive the connector-ID status change interrupt, and the driver's
//! `dwc2_conn_id_status_change()` work handler then re-initialises the core for the new
//! mode and starts the HCD (or the UDC) as usual — the register poke is the only thing
//! missing, everything downstream of it is stock kernel behaviour.

use std::{
	fs::OpenOptions,
	os::fd::AsRawFd,
	time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, trace};

// Register offsets from the core's base address (DWC2 databook §"Core Global Registers").
const GUSBCFG: usize = 0x0c;
const GINTSTS: usize = 0x14;
const GOTGCTL: usize = 0x00;

const GUSBCFG_FORCEHOSTMODE: u32 = 1 << 29;
const GUSBCFG_FORCEDEVMODE: u32 = 1 << 30;
/// GINTSTS bit 0, `CurMod`: 1 = host mode, 0 = device mode.
const GINTSTS_CURMODE_HOST: u32 = 1 << 0;

const MAP_LEN: usize = 0x1000;

/// How long to wait for `GINTSTS.CurMod` to follow the force bits. The kernel's
/// `dwc2_wait_for_mode()` uses the same 110 ms budget.
const MODE_TIMEOUT: Duration = Duration::from_millis(110);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
	Host,
	Device,
}

impl Mode {
	fn as_role(self) -> &'static str {
		match self {
			Self::Host => "host",
			Self::Device => "device",
		}
	}
}

/// A memory mapping of the DWC2 core's register block.
pub struct Dwc2 {
	regs: *mut u32,
	role_path: String,
}

// The mapping is just a pointer into our own address space; moving it between tokio
// worker threads is fine.
unsafe impl Send for Dwc2 {}
unsafe impl Sync for Dwc2 {}

impl Dwc2 {
	/// Map the core whose UDC is named `udc_name` (e.g. `3f980000.usb`). The physical
	/// base address is the hex prefix of that name — it is the `reg` property of the
	/// controller's device tree node, which is also how the platform device is named.
	pub fn open(udc_name: &str) -> Result<Self> {
		let base = udc_name
			.split('.')
			.next()
			.and_then(|x| u64::from_str_radix(x, 16).ok())
			.ok_or_else(|| anyhow!("cannot derive a base address from udc name {udc_name:?}"))?;

		if base % MAP_LEN as u64 != 0 {
			bail!("udc base address {base:#x} is not page aligned");
		}

		let mem = OpenOptions::new()
			.read(true)
			.write(true)
			.open("/dev/mem")
			.context("failed to open /dev/mem (run as root)")?;

		// SAFETY: mapping MAP_LEN bytes of a page-aligned physical address from
		// /dev/mem; the resulting pointer is only ever used for volatile 32-bit
		// accesses inside this range.
		let regs = unsafe {
			libc::mmap(
				std::ptr::null_mut(),
				MAP_LEN,
				libc::PROT_READ | libc::PROT_WRITE,
				libc::MAP_SHARED,
				mem.as_raw_fd(),
				base as libc::off_t,
			)
		};
		if regs == libc::MAP_FAILED {
			return Err(std::io::Error::last_os_error())
				.with_context(|| format!("failed to mmap dwc2 registers at {base:#x}"));
		}

		let this = Self {
			regs: regs.cast(),
			role_path: format!("/sys/class/usb_role/{udc_name}-role-switch/role"),
		};

		debug!(
			"mapped dwc2 at {base:#x}: GOTGCTL={:#010x} GUSBCFG={:#010x} GINTSTS={:#010x} (currently in {} mode)",
			this.read(GOTGCTL),
			this.read(GUSBCFG),
			this.read(GINTSTS),
			this.current_mode_str(),
		);

		Ok(this)
	}

	fn read(&self, offset: usize) -> u32 {
		// SAFETY: offset is one of the constants above, well inside the mapping.
		unsafe { self.regs.byte_add(offset).read_volatile() }
	}

	fn write(&self, offset: usize, value: u32) {
		// SAFETY: as above.
		unsafe { self.regs.byte_add(offset).write_volatile(value) }
	}

	/// The mode the core is *actually* in right now, straight from `GINTSTS.CurMod`.
	pub fn current_mode(&self) -> Mode {
		if self.read(GINTSTS) & GINTSTS_CURMODE_HOST != 0 {
			Mode::Host
		} else {
			Mode::Device
		}
	}

	fn current_mode_str(&self) -> &'static str {
		self.current_mode().as_role()
	}

	/// Force the core into `mode`, first asking the kernel nicely so its role-switch
	/// bookkeeping stays in sync, then setting the `GUSBCFG` force bits ourselves
	/// because on this board that request does nothing (see the module docs).
	///
	/// Returns once `GINTSTS.CurMod` reports the requested mode. The driver's
	/// connector-ID work handler runs asynchronously after that, so the HCD/UDC comes
	/// up a few milliseconds later.
	pub fn force_mode(&self, mode: Mode) -> Result<()> {
		// Best-effort: on a board where the role switch works this does the whole job
		// and the register write below becomes a no-op.
		if let Err(err) = std::fs::write(&self.role_path, mode.as_role()) {
			debug!(
				"could not write {} ({err}); forcing directly",
				self.role_path
			);
		}

		let (set, clear) = match mode {
			Mode::Host => (GUSBCFG_FORCEHOSTMODE, GUSBCFG_FORCEDEVMODE),
			Mode::Device => (GUSBCFG_FORCEDEVMODE, GUSBCFG_FORCEHOSTMODE),
		};

		let gusbcfg = self.read(GUSBCFG);
		if gusbcfg & set != 0 && gusbcfg & clear == 0 {
			debug!("dwc2 already forced to {mode:?} (GUSBCFG={gusbcfg:#010x})");
		} else {
			let new = (gusbcfg & !clear) | set;
			debug!("forcing dwc2 to {mode:?}: GUSBCFG {gusbcfg:#010x} -> {new:#010x}");
			self.write(GUSBCFG, new);
		}

		let deadline = Instant::now() + MODE_TIMEOUT;
		loop {
			let current = self.current_mode();
			if current == mode {
				trace!(
					"dwc2 in {mode:?} mode (GOTGCTL={:#010x} GINTSTS={:#010x})",
					self.read(GOTGCTL),
					self.read(GINTSTS)
				);
				return Ok(());
			}
			if Instant::now() >= deadline {
				bail!(
					"dwc2 did not enter {mode:?} mode within {MODE_TIMEOUT:?} (GUSBCFG={:#010x} GINTSTS={:#010x})",
					self.read(GUSBCFG),
					self.read(GINTSTS)
				);
			}
			std::thread::sleep(Duration::from_millis(1));
		}
	}
}

impl Drop for Dwc2 {
	fn drop(&mut self) {
		// SAFETY: unmapping exactly what open() mapped.
		unsafe {
			libc::munmap(self.regs.cast(), MAP_LEN);
		}
	}
}
