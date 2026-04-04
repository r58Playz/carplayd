use super::error::Iap2Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Control(u8);

impl Control {
	pub const SYN_BIT: u8 = 1 << 7;
	pub const ACK_BIT: u8 = 1 << 6;
	pub const EAK_BIT: u8 = 1 << 5;
	pub const RST_BIT: u8 = 1 << 4;
	pub const SLP_BIT: u8 = 1 << 3;

	const ASSIGNED_MASK: u8 =
		Self::SYN_BIT | Self::ACK_BIT | Self::EAK_BIT | Self::RST_BIT | Self::SLP_BIT;

	pub fn new(raw: u8) -> Result<Self, Iap2Error> {
		if raw & !Self::ASSIGNED_MASK != 0 {
			return Err(Iap2Error::InvalidControlByte(raw));
		}

		let control = Self(raw);
		control.validate_flag_relationships()?;
		Ok(control)
	}

	pub const fn syn_only() -> Self {
		Self(Self::SYN_BIT)
	}

	pub const fn ack_only() -> Self {
		Self(Self::ACK_BIT)
	}

	pub const fn syn_ack() -> Self {
		Self(Self::SYN_BIT | Self::ACK_BIT)
	}

	pub const fn eak() -> Self {
		Self(Self::ACK_BIT | Self::EAK_BIT)
	}

	pub const fn rst() -> Self {
		Self(Self::RST_BIT)
	}

	pub const fn sleep() -> Self {
		Self(Self::SLP_BIT)
	}

	pub const fn raw(self) -> u8 {
		self.0
	}

	pub const fn is_syn(self) -> bool {
		self.0 & Self::SYN_BIT != 0
	}

	pub const fn is_ack(self) -> bool {
		self.0 & Self::ACK_BIT != 0
	}

	pub const fn is_eak(self) -> bool {
		self.0 & Self::EAK_BIT != 0
	}

	pub const fn is_rst(self) -> bool {
		self.0 & Self::RST_BIT != 0
	}

	pub const fn is_sleep(self) -> bool {
		self.0 & Self::SLP_BIT != 0
	}

	pub fn validate_packet_fields(
		self,
		payload_present: bool,
		acknowledgement_number: u8,
		session_id: u8,
	) -> Result<(), Iap2Error> {
		self.validate_flag_relationships()?;

		if !self.is_ack() && acknowledgement_number != 0 {
			return Err(Iap2Error::AckNumberMustBeZero(acknowledgement_number));
		}

		if self.is_syn() || self.is_eak() {
			if !payload_present {
				return Err(Iap2Error::MissingPayload);
			}
			if session_id != 0 {
				return Err(Iap2Error::SessionIdMustBeZero(session_id));
			}
			return Ok(());
		}

		if self.is_rst() || self.is_sleep() {
			if payload_present {
				return Err(Iap2Error::UnexpectedPayload);
			}
			if session_id != 0 {
				return Err(Iap2Error::SessionIdMustBeZero(session_id));
			}
			return Ok(());
		}

		if payload_present {
			if !self.is_ack() {
				return Err(Iap2Error::SessionPayloadRequiresAck);
			}
			if session_id == 0 {
				return Err(Iap2Error::SessionIdRequired);
			}
		} else if session_id != 0 {
			return Err(Iap2Error::SessionIdMustBeZero(session_id));
		}

		Ok(())
	}

	fn validate_flag_relationships(self) -> Result<(), Iap2Error> {
		let mut exclusive_count = 0;
		if self.is_syn() {
			exclusive_count += 1;
		}
		if self.is_eak() {
			exclusive_count += 1;
		}
		if self.is_rst() {
			exclusive_count += 1;
		}

		if exclusive_count > 1 {
			return Err(Iap2Error::InvalidControlByte(self.0));
		}

		if self.is_eak() && !self.is_ack() {
			return Err(Iap2Error::InvalidControlByte(self.0));
		}

		if self.is_rst() && self.0 != Self::RST_BIT {
			return Err(Iap2Error::InvalidControlByte(self.0));
		}

		if self.is_sleep() && self.0 != Self::SLP_BIT {
			return Err(Iap2Error::InvalidControlByte(self.0));
		}

		Ok(())
	}
}

impl TryFrom<u8> for Control {
	type Error = Iap2Error;

	fn try_from(value: u8) -> Result<Self, Self::Error> {
		Self::new(value)
	}
}

impl From<Control> for u8 {
	fn from(value: Control) -> Self {
		value.raw()
	}
}

#[cfg(test)]
mod tests {
	use super::Control;

	#[test]
	fn eak_requires_ack() {
		assert!(Control::new(Control::EAK_BIT).is_err());
		assert!(Control::new(Control::ACK_BIT | Control::EAK_BIT).is_ok());
	}

	#[test]
	fn rst_cannot_be_combined() {
		assert!(Control::new(Control::RST_BIT | Control::ACK_BIT).is_err());
	}

	#[test]
	fn syn_requires_payload() {
		let control = Control::syn_only();
		assert!(control.validate_packet_fields(false, 0, 0).is_err());
		assert!(control.validate_packet_fields(true, 0, 0).is_ok());
	}

	#[test]
	fn session_payload_requires_ack_and_session_id() {
		let control = Control::ack_only();
		assert!(control.validate_packet_fields(true, 0, 0).is_err());
		assert!(control.validate_packet_fields(true, 0, 1).is_ok());
	}
}
