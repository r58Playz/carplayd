//! vibecoded
//! The wall clock, and the NTP server the accessory syncs to (§30.3.2.8.1).
//!
//! We are the device, so we are the *reference*: the accessory is the NTP client and we
//! never discipline our clock to anything. That makes the server stateless — answer each
//! request with an accurate receive and transmit timestamp and the accessory does all the
//! rate and offset estimation on its side.
//!
//! The packet is Apple's `RTPTimeSyncPacket`, a 32-byte RTP-wrapped NTP exchange rather
//! than a 48-byte RFC 5905 packet. See `PROTOCOL.md` for where the layout came from; the
//! short version is that a shipping CarPlay device validates two bytes of the request,
//! copies one field straight back out, and fills three timestamps.
//!
//! # One clock
//!
//! [`Clock`] is the single timebase for the whole session: NTP responses, screen stream
//! presentation timestamps, and the `/feedback` message all have to agree. It is `Copy`
//! and carries its own origin, so pass one around rather than calling [`Clock::new`]
//! twice — two clocks would drift apart by however long elapsed between them.
//!
//! It is deliberately monotonic. `CLOCK_REALTIME` would let an NTP step or an RTC
//! correction mid-session shear the accessory's rate estimate, which is miserable to
//! debug. The reference implementation does the same thing, offsetting monotonic uptime
//! by the NTP epoch so the seconds field merely *looks* like a 1900-based timestamp; head
//! units accept a wall clock that reads as 1 Jan 1970 without complaint.

use std::{
	io, mem,
	net::{Ipv6Addr, SocketAddrV6},
	os::fd::{AsRawFd, RawFd},
	ptr,
	time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use log::{debug, trace, warn};
use tokio::{io::Interest, net::UdpSocket};

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch, in nanoseconds.
///
/// The literal `0x1ea7e79c20d10000` in the reference implementation, which adds it to
/// `CLOCK_MONOTONIC` before converting to NTP form.
pub const NTP_EPOCH_OFFSET_NS: u64 = 2_208_988_800 * NANOS_PER_SEC;

/// Convert nanoseconds to the 64-bit NTP fixed-point form: seconds in the upper 32 bits,
/// fraction of a second in the lower 32.
pub fn to_ntp64(nanos: u64) -> u64 {
	let seconds = nanos / NANOS_PER_SEC;
	// The remainder is below 2^30, so shifting it up by 32 stays inside a u64.
	let fraction = ((nanos % NANOS_PER_SEC) << 32) / NANOS_PER_SEC;

	(seconds << 32) | fraction
}

/// The device's wall clock.
///
/// Monotonic, with an origin fixed when the clock is created. See the module docs for why
/// there should be exactly one of these per session.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
	origin: Instant,
}

impl Default for Clock {
	fn default() -> Self {
		Self::new()
	}
}

impl Clock {
	pub fn new() -> Self {
		Self {
			origin: Instant::now(),
		}
	}

	/// Nanoseconds since the NTP epoch, as this clock reckons it.
	pub fn now(&self) -> u64 {
		NTP_EPOCH_OFFSET_NS + self.origin.elapsed().as_nanos() as u64
	}

	/// [`Clock::now`] in NTP fixed-point form, ready for the wire.
	pub fn now_ntp(&self) -> u64 {
		to_ntp64(self.now())
	}

	pub fn instant_ntp(&self, instant: Instant) -> u64 {
		to_ntp64(NTP_EPOCH_OFFSET_NS + instant.saturating_duration_since(self.origin).as_nanos() as u64)
	}

	/// Move a kernel packet timestamp onto this clock.
	///
	/// `SO_TIMESTAMPNS` reports against `CLOCK_REALTIME`, so we sample both clocks back to
	/// back and shift the kernel value by the difference. Only the gap between those two
	/// samples is error, which is far smaller than the scheduler delay this exists to
	/// remove in the first place.
	pub fn received_at(&self, timestamp: libc::timespec) -> u64 {
		let kernel = timestamp.tv_sec as i128 * NANOS_PER_SEC as i128 + timestamp.tv_nsec as i128;

		let realtime = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|since| since.as_nanos() as i128)
			.unwrap_or(0);
		let wall = self.now() as i128;

		// The packet arrived `realtime - kernel` ago.
		(wall - (realtime - kernel)).max(0) as u64
	}
}

/// Every timing packet is exactly this long, in both directions.
pub const PACKET_LEN: usize = 32;

/// RTP payload type 82 with the marker bit: a request from the accessory.
const PAYLOAD_TYPE_REQUEST: u8 = 0xD2;
/// RTP payload type 83 with the marker bit: our response.
const PAYLOAD_TYPE_RESPONSE: u8 = 0xD3;

/// The sequence number the reference implementation writes into every response. It is a
/// constant there, not a counter.
const SEQUENCE: u16 = 7;

/// Build the response to one timing request, or `None` if it is not one.
///
/// Validation is deliberately as thin as the reference: RTP version 2 in the top two bits
/// of the first byte, and the request payload type in the second. Every other field of the
/// request is ignored except bytes 24..32, the client's transmit timestamp, which is
/// copied back out byte for byte without reinterpretation.
pub fn build_response(request: &[u8], received: u64, clock: &Clock) -> Option<[u8; PACKET_LEN]> {
	if request.len() < PACKET_LEN {
		return None;
	}
	if request[0] >> 6 != 2 || request[1] != PAYLOAD_TYPE_REQUEST {
		return None;
	}

	let mut response = [0u8; PACKET_LEN];

	response[0] = 0x80;
	response[1] = PAYLOAD_TYPE_RESPONSE;
	response[2..4].copy_from_slice(&SEQUENCE.to_be_bytes());

	// Originate: the request's transmit timestamp, verbatim.
	response[8..16].copy_from_slice(&request[24..32]);
	response[16..24].copy_from_slice(&to_ntp64(received).to_be_bytes());

	// Sampled as late as possible, so the accessory's round-trip estimate excludes the
	// time we spent getting here.
	let transmit = clock.now_ntp();
	response[24..32].copy_from_slice(&transmit.to_be_bytes());

	// Bytes 4..8 carry the integer-seconds half of the transmit timestamp.
	response[4..8].copy_from_slice(&((transmit >> 32) as u32).to_be_bytes());

	Some(response)
}

/// A control buffer for `recvmsg`, aligned for `cmsghdr`.
///
/// 256 bytes is what the reference implementation uses; a single timestamp control message
/// needs a small fraction of that.
#[repr(C, align(8))]
struct ControlBuffer([u8; 256]);

/// One datagram, with the kernel's receive timestamp and the sender's address kept in the
/// raw form `sendto` wants back.
struct Datagram {
	len: usize,
	from: libc::sockaddr_storage,
	from_len: libc::socklen_t,
	timestamp: Option<libc::timespec>,
}

/// `recvmsg` with a control buffer, pulling out `SCM_TIMESTAMPNS` if the kernel attached it.
///
/// # Safety
///
/// `fd` must be a valid, open socket for the duration of the call.
unsafe fn recv_with_timestamp(fd: RawFd, buf: &mut [u8]) -> io::Result<Datagram> {
	// SAFETY: every structure below is either zeroed (valid for all these POD types) or
	// filled in before use, and all pointers are into locals that outlive the syscall.
	unsafe {
		let mut from: libc::sockaddr_storage = mem::zeroed();
		let mut control = ControlBuffer([0u8; 256]);

		let mut iov = libc::iovec {
			iov_base: buf.as_mut_ptr().cast(),
			iov_len: buf.len(),
		};

		let mut msg: libc::msghdr = mem::zeroed();
		msg.msg_name = (&raw mut from).cast();
		msg.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
		msg.msg_iov = &raw mut iov;
		msg.msg_iovlen = 1;
		msg.msg_control = control.0.as_mut_ptr().cast();
		msg.msg_controllen = control.0.len() as _;

		let len = libc::recvmsg(fd, &raw mut msg, 0);
		if len < 0 {
			return Err(io::Error::last_os_error());
		}

		let mut timestamp = None;
		let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
		while !cmsg.is_null() {
			if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_TIMESTAMPNS
			{
				let mut value: libc::timespec = mem::zeroed();
				ptr::copy_nonoverlapping(
					libc::CMSG_DATA(cmsg),
					(&raw mut value).cast::<u8>(),
					mem::size_of::<libc::timespec>(),
				);
				timestamp = Some(value);
				break;
			}
			cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
		}

		Ok(Datagram {
			len: len as usize,
			from,
			from_len: msg.msg_namelen,
			timestamp,
		})
	}
}

/// `sendto` the address a datagram came from, without round-tripping it through
/// `SocketAddr` — that conversion is where an IPv6 scope id gets quietly lost.
///
/// # Safety
///
/// `fd` must be a valid, open socket, and `from`/`from_len` must be an address the kernel
/// handed us.
unsafe fn send_back(
	fd: RawFd,
	buf: &[u8],
	from: &libc::sockaddr_storage,
	from_len: libc::socklen_t,
) -> io::Result<usize> {
	// SAFETY: the buffer and address are borrowed for the whole call, and `from_len` came
	// from the kernel alongside `from`.
	let sent = unsafe {
		libc::sendto(
			fd,
			buf.as_ptr().cast(),
			buf.len(),
			0,
			ptr::from_ref(from).cast(),
			from_len,
		)
	};

	if sent < 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(sent as usize)
	}
}

fn set_sockopt(
	fd: RawFd,
	level: libc::c_int,
	name: libc::c_int,
	value: libc::c_int,
) -> io::Result<()> {
	// SAFETY: `value` is a live `c_int` and its length is passed correctly.
	let result = unsafe {
		libc::setsockopt(
			fd,
			level,
			name,
			ptr::from_ref(&value).cast(),
			mem::size_of::<libc::c_int>() as libc::socklen_t,
		)
	};

	if result < 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(())
	}
}

/// The NTP server the accessory syncs against.
///
/// Bind before sending the initial SETUP: [`TimingServer::port`] is what goes in its
/// `timingPort` key, and the accessory may start asking the moment it has answered.
///
/// The loop is allocation-free and does one `recvmsg` and one `sendto` per request. On a
/// loaded Pi Zero 2 W it would be worth moving to a dedicated `SCHED_FIFO` thread with its
/// own current-thread runtime; the kernel timestamp removes most of the reason to, since
/// scheduling delay no longer lands in the measurement.
pub struct TimingServer {
	socket: UdpSocket,
	clock: Clock,
}

impl TimingServer {
	pub async fn bind(ip: Ipv6Addr, scope_id: u32, port: u16, clock: Clock) -> Result<Self> {
		let socket = UdpSocket::bind(SocketAddrV6::new(ip, port, 0, scope_id))
			.await
			.context("failed to bind the timing socket")?;

		let fd = socket.as_raw_fd();

		set_sockopt(fd, libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, 1)
			.context("failed to enable SO_TIMESTAMPNS")?;

		// Table 30-55 puts time sync in the voice class. Best effort: it changes nothing
		// over NCM, and matters on the wireless transport.
		const DSCP_EXPEDITED_FORWARDING: libc::c_int = 46 << 2;
		if let Err(err) = set_sockopt(
			fd,
			libc::IPPROTO_IPV6,
			libc::IPV6_TCLASS,
			DSCP_EXPEDITED_FORWARDING,
		) {
			debug!("could not set IPV6_TCLASS on the timing socket: {err}");
		}

		debug!(
			"timing server listening on port {}",
			socket.local_addr()?.port()
		);

		Ok(Self { socket, clock })
	}

	/// The port to advertise as `timingPort` in the initial SETUP request.
	pub fn port(&self) -> Result<u16> {
		Ok(self
			.socket
			.local_addr()
			.context("failed to read the timing socket address")?
			.port())
	}

	pub async fn run(self) -> Result<()> {
		let fd = self.socket.as_raw_fd();
		let mut buf = [0u8; PACKET_LEN];

		loop {
			self.socket
				.readable()
				.await
				.context("failed to wait on the timing socket")?;

			// SAFETY: the socket owns `fd` and outlives every call here.
			let datagram = match self.socket.try_io(Interest::READABLE, || unsafe {
				recv_with_timestamp(fd, &mut buf)
			}) {
				Ok(datagram) => datagram,
				// A spurious readiness notification; wait for the next one.
				Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
				Err(err) => return Err(err).context("failed to read from the timing socket"),
			};

			// Taken as close to the syscall as we can get. Falling back to sampling now
			// costs accuracy but not correctness.
			let received = datagram
				.timestamp
				.map_or_else(|| self.clock.now(), |ts| self.clock.received_at(ts));

			let Some(response) = build_response(&buf[..datagram.len], received, &self.clock) else {
				warn!(
					"ignoring a {} byte datagram on the timing socket",
					datagram.len
				);
				continue;
			};

			// SAFETY: the address came from the kernel in the matching `recvmsg`.
			match unsafe { send_back(fd, &response, &datagram.from, datagram.from_len) } {
				Ok(PACKET_LEN) => trace!("answered a timing request"),
				Ok(short) => warn!("timing response was truncated to {short} bytes"),
				Err(err) => warn!("failed to send a timing response: {err}"),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	/// A well-formed request carrying `transmit` as the client's transmit timestamp.
	fn request(transmit: u64) -> [u8; PACKET_LEN] {
		let mut packet = [0u8; PACKET_LEN];
		packet[0] = 0x80;
		packet[1] = PAYLOAD_TYPE_REQUEST;
		packet[2..4].copy_from_slice(&1u16.to_be_bytes());
		packet[24..32].copy_from_slice(&transmit.to_be_bytes());
		packet
	}

	#[test]
	fn ntp_conversion_splits_seconds_and_fraction() {
		assert_eq!(to_ntp64(0), 0);
		assert_eq!(to_ntp64(NANOS_PER_SEC), 1 << 32);
		assert_eq!(to_ntp64(NANOS_PER_SEC / 2), 1 << 31, "half a second");

		// A frame at 60 fps is 2^32/60 fraction units apart, which is what proves the
		// screen stream timestamps are NTP and not nanoseconds. The input truncates to
		// whole nanoseconds, so allow a few units of slack.
		let step = to_ntp64(NANOS_PER_SEC / 60);
		let expected = (1u64 << 32) / 60;
		assert!(expected.abs_diff(step) < 8, "{step} vs {expected}");
	}

	#[test]
	fn ntp_conversion_never_overflows_the_fraction() {
		// One nanosecond short of a second is the worst case for the shift.
		let ntp = to_ntp64(2 * NANOS_PER_SEC - 1);
		assert_eq!(ntp >> 32, 1);
		assert_eq!(ntp as u32, 0xFFFF_FFFB);
	}

	#[test]
	fn the_epoch_offset_is_the_reference_constant() {
		assert_eq!(NTP_EPOCH_OFFSET_NS, 0x1ea7_e79c_20d1_0000);
	}

	#[test]
	fn the_clock_only_moves_forward() {
		let clock = Clock::new();
		let first = clock.now();
		let second = clock.now();

		assert!(second >= first);
		assert!(first >= NTP_EPOCH_OFFSET_NS, "the epoch offset is applied");
		// A freshly created clock reads as just after the Unix epoch, which is exactly
		// what the reference implementation presents to head units.
		assert!(first - NTP_EPOCH_OFFSET_NS < NANOS_PER_SEC);
	}

	#[test]
	fn a_response_has_the_documented_layout() {
		let clock = Clock::new();
		let transmit = 0x1122_3344_5566_7788;

		let response = build_response(&request(transmit), clock.now(), &clock).unwrap();

		assert_eq!(response[0], 0x80);
		assert_eq!(response[1], PAYLOAD_TYPE_RESPONSE);
		assert_eq!(u16::from_be_bytes([response[2], response[3]]), SEQUENCE);

		assert_eq!(
			&response[8..16],
			&transmit.to_be_bytes(),
			"the originate timestamp is copied out verbatim"
		);

		let receive = u64::from_be_bytes(response[16..24].try_into().unwrap());
		let transmit_ts = u64::from_be_bytes(response[24..32].try_into().unwrap());
		assert!(receive <= transmit_ts, "we receive before we transmit");

		assert_eq!(
			u32::from_be_bytes(response[4..8].try_into().unwrap()),
			(transmit_ts >> 32) as u32,
			"bytes 4..8 mirror the transmit seconds"
		);
	}

	/// The originate field is copied as bytes, so a request whose timestamp is not valid
	/// NTP still round-trips unchanged.
	#[test]
	fn the_originate_field_is_never_reinterpreted() {
		let clock = Clock::new();
		let garbage = 0xDEAD_BEEF_CAFE_F00Du64;

		let response = build_response(&request(garbage), clock.now(), &clock).unwrap();
		assert_eq!(&response[8..16], &garbage.to_be_bytes());
	}

	#[test]
	fn non_timing_datagrams_are_rejected() {
		let clock = Clock::new();
		let now = clock.now();

		let mut wrong_version = request(0);
		wrong_version[0] = 0x40;
		assert!(build_response(&wrong_version, now, &clock).is_none());

		let mut wrong_payload_type = request(0);
		wrong_payload_type[1] = PAYLOAD_TYPE_RESPONSE;
		assert!(build_response(&wrong_payload_type, now, &clock).is_none());

		assert!(build_response(&request(0)[..31], now, &clock).is_none());
		assert!(build_response(&[], now, &clock).is_none());
	}

	/// The whole point of `SO_TIMESTAMPNS` is that the receive timestamp predates our
	/// handling of the packet, so a delay between arrival and processing must not appear
	/// in the receive field.
	#[tokio::test]
	async fn the_server_timestamps_at_arrival_not_at_processing() {
		let clock = Clock::new();
		let server = TimingServer::bind(Ipv6Addr::LOCALHOST, 0, 0, clock)
			.await
			.unwrap();
		let port = server.port().unwrap();

		let client = UdpSocket::bind("[::1]:0").await.unwrap();
		client
			.connect(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
			.await
			.unwrap();

		client.send(&request(0x0102_0304_0506_0708)).await.unwrap();

		// Stall the server's task so arrival and processing are far apart.
		tokio::time::sleep(Duration::from_millis(50)).await;
		let task = tokio::spawn(server.run());

		let mut buf = [0u8; PACKET_LEN];
		let len = tokio::time::timeout(Duration::from_secs(5), client.recv(&mut buf))
			.await
			.expect("the server should answer")
			.unwrap();
		assert_eq!(len, PACKET_LEN);

		assert_eq!(&buf[8..16], &0x0102_0304_0506_0708u64.to_be_bytes());

		let receive = u64::from_be_bytes(buf[16..24].try_into().unwrap());
		let transmit = u64::from_be_bytes(buf[24..32].try_into().unwrap());

		// If the receive timestamp were sampled when the task finally ran, the two would
		// be microseconds apart. Coming from the kernel, they straddle the whole stall.
		let gap_ns = ((transmit - receive) as u128 * NANOS_PER_SEC as u128) >> 32;
		assert!(
			gap_ns > 25_000_000,
			"receive and transmit are only {gap_ns} ns apart, so the receive timestamp did \
			 not come from the kernel"
		);

		task.abort();
	}
}
