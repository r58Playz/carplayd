use super::{
	control_session::{
		ControlMessageKind, IDENT_PARAM_FIRMWARE_VERSION, IDENT_PARAM_HARDWARE_VERSION,
		IDENT_PARAM_MANUFACTURER, IDENT_PARAM_MODEL_IDENTIFIER, IDENT_PARAM_NAME,
		IDENT_PARAM_SERIAL_NUMBER, IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT,
		IDENT_PARAM_VEHICLE_INFORMATION_COMPONENT, IdentificationInformation,
	},
	decoder::PacketStreamParser,
	error::Iap2Error,
	link::{LinkRecord, LinkTransmitWindow},
	packet::{LinkPacket, PacketKind},
	sync::{LinkSynchronizationPayload, SessionDescriptor},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessoryProfile {
	pub expected_control_session_id: u8,
	pub expected_control_session_version: u8,
	pub expected_authentication_certificate: Option<Vec<u8>>,
	pub expected_authentication_serial_number: Option<Vec<u8>>,
	pub expected_authentication_response: Option<Vec<u8>>,
	pub identification: Option<IdentificationInformation>,
}

impl Default for AccessoryProfile {
	fn default() -> Self {
		Self {
			expected_control_session_id: 1,
			expected_control_session_version: 2,
			expected_authentication_certificate: None,
			expected_authentication_serial_number: None,
			expected_authentication_response: None,
			identification: None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationPolicy {
	Strict,
	AlwaysAccept,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentificationOutcome {
	Accepted,
	Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineState {
	WaitingForSyn,
	WaitingForLinkAck,
	WaitingForAuthenticationMaterial,
	WaitingForAuthenticationChallengeResponse,
	WaitingForIdentificationInformation,
	WaitingForIdentificationRetry,
	ReadyForCarPlay,
	Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineOutput {
	Transmit(Vec<u8>),
	AuthenticationOutcome { accepted: bool, fake: bool },
	IdentificationOutcome { outcome: IdentificationOutcome },
	CarPlayReady,
	LinkResetRequested,
}

#[derive(Debug, Clone)]
pub struct MachineSnapshot {
	pub state: MachineState,
	pub authenticated: bool,
	pub identified: bool,
	pub control_session_id: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct Iap2MachineConfig {
	pub local_sync_payload: LinkSynchronizationPayload,
	pub local_initial_psn: u8,
	pub accessory_profile: AccessoryProfile,
	pub authentication_policy: AuthenticationPolicy,
}

impl Iap2MachineConfig {
	pub fn iphone_default() -> Self {
		let sessions = vec![SessionDescriptor {
			identifier: 1,
			session_type: 0,
			session_version: 2,
		}];

		Self {
			local_sync_payload: LinkSynchronizationPayload::recommended_for_transport(
				super::sync::TransportProfile::UsbHostFullSpeed,
				sessions,
			),
			local_initial_psn: 1,
			accessory_profile: AccessoryProfile::default(),
			authentication_policy: AuthenticationPolicy::AlwaysAccept,
		}
	}
}

#[derive(Debug)]
pub struct Iap2Machine {
	state: MachineState,
	local_sync_payload: LinkSynchronizationPayload,
	link_record: LinkRecord,
	parser: PacketStreamParser,
	transmit_window: Option<LinkTransmitWindow>,
	control_session_id: Option<u8>,
	authenticated: bool,
	identified: bool,
	pending_auth_challenge: Option<Vec<u8>>,
	last_identification: Option<IdentificationInformation>,
	accessory_profile: AccessoryProfile,
	authentication_policy: AuthenticationPolicy,
	auth_challenge_counter: u32,
}

impl Iap2Machine {
	pub fn new(config: Iap2MachineConfig) -> Self {
		Self {
			state: MachineState::WaitingForSyn,
			local_sync_payload: config.local_sync_payload,
			link_record: LinkRecord::new(config.local_initial_psn),
			parser: PacketStreamParser::default(),
			transmit_window: None,
			control_session_id: None,
			authenticated: false,
			identified: false,
			pending_auth_challenge: None,
			last_identification: None,
			accessory_profile: config.accessory_profile,
			authentication_policy: config.authentication_policy,
			auth_challenge_counter: 0,
		}
	}

	pub fn snapshot(&self) -> MachineSnapshot {
		MachineSnapshot {
			state: self.state.clone(),
			authenticated: self.authenticated,
			identified: self.identified,
			control_session_id: self.control_session_id,
		}
	}

	pub fn state(&self) -> &MachineState {
		&self.state
	}

	pub fn is_ready_for_carplay(&self) -> bool {
		matches!(self.state, MachineState::ReadyForCarPlay)
	}

	pub fn start(&mut self) -> Result<Vec<MachineOutput>, Iap2Error> {
		Ok(vec![self.start_link_synchronization()?])
	}

	pub fn start_link_synchronization(&mut self) -> Result<MachineOutput, Iap2Error> {
		let sequence = self.link_record.next_sequence_for_payload();
		let packet = LinkPacket::syn(sequence, self.local_sync_payload.clone())?;
		self.state = MachineState::WaitingForSyn;
		Self::emit_packet(packet)
	}

	pub fn on_transport_bytes(&mut self, bytes: &[u8]) -> Result<Vec<MachineOutput>, Iap2Error> {
		self.parser.push_bytes(bytes);
		let mut outputs = Vec::new();
		while let Some(packet) = self.parser.next_packet() {
			outputs.extend(self.on_link_packet(packet)?);
		}

		Ok(outputs)
	}

	pub fn on_link_packet(&mut self, packet: LinkPacket) -> Result<Vec<MachineOutput>, Iap2Error> {
		self.handle_incoming_link_packet(packet)
	}

	fn handle_incoming_link_packet(
		&mut self,
		packet: LinkPacket,
	) -> Result<Vec<MachineOutput>, Iap2Error> {
		let mut outputs = Vec::new();

		match packet.packet_kind() {
			PacketKind::Reset => {
				self.reset();
				outputs.push(MachineOutput::LinkResetRequested);
				return Ok(outputs);
			}
			PacketKind::Sleep => return Ok(outputs),
			_ => {}
		}

		if let Some(window) = &mut self.transmit_window {
			if packet.control.is_ack() {
				window.acknowledge(packet.acknowledgement_number);
			}
		}

		match self.state {
			MachineState::WaitingForSyn => {
				self.handle_waiting_for_syn(packet, &mut outputs)?;
			}
			MachineState::WaitingForLinkAck => {
				self.handle_waiting_for_link_ack(packet, &mut outputs)?;
			}
			MachineState::WaitingForAuthenticationMaterial
			| MachineState::WaitingForAuthenticationChallengeResponse
			| MachineState::WaitingForIdentificationInformation
			| MachineState::WaitingForIdentificationRetry
			| MachineState::ReadyForCarPlay => {
				self.handle_control_phase(packet, &mut outputs)?;
			}
			MachineState::Failed => {}
		}

		Ok(outputs)
	}

	fn handle_waiting_for_syn(
		&mut self,
		packet: LinkPacket,
		outputs: &mut Vec<MachineOutput>,
	) -> Result<(), Iap2Error> {
		if !matches!(packet.packet_kind(), PacketKind::Syn | PacketKind::SynAck) {
			return Ok(());
		}

		let peer_sync = packet.sync_payload()?;
		let local_sync = self.local_sync_payload.align_to_peer(&peer_sync)?;
		self.ensure_control_session_v2(&local_sync)?;
		self.local_sync_payload = local_sync.clone();

		let transmit_window = LinkTransmitWindow::new(
			peer_sync.max_outstanding_packets,
			std::time::Duration::from_millis(u64::from(peer_sync.retransmission_timeout_ms)),
			peer_sync.max_retransmissions,
		)?;
		self.transmit_window = Some(transmit_window);

		self.control_session_id = find_control_session_id(
			&local_sync,
			self.accessory_profile.expected_control_session_id,
			self.accessory_profile.expected_control_session_version,
		);

		if self.control_session_id.is_none() {
			return Err(Iap2Error::MissingControlSessionV2);
		}

		self.link_record
			.set_last_received_in_sequence(packet.sequence_number);

		let response_sequence = self.link_record.next_sequence_for_payload();
		let response = LinkPacket::syn_ack(response_sequence, packet.sequence_number, local_sync)?;
		outputs.push(Self::emit_packet(response)?);

		self.state = MachineState::WaitingForLinkAck;
		Ok(())
	}

	fn handle_waiting_for_link_ack(
		&mut self,
		packet: LinkPacket,
		outputs: &mut Vec<MachineOutput>,
	) -> Result<(), Iap2Error> {
		match packet.packet_kind() {
			PacketKind::Ack => {
				let start = ControlMessageKind::StartIdentification;
				outputs.push(self.wrap_control_message(start)?);
				self.state = MachineState::WaitingForAuthenticationMaterial;
			}
			PacketKind::Syn | PacketKind::SynAck => {
				self.handle_waiting_for_syn(packet, outputs)?;
			}
			_ => {}
		}

		Ok(())
	}

	fn handle_control_phase(
		&mut self,
		packet: LinkPacket,
		outputs: &mut Vec<MachineOutput>,
	) -> Result<(), Iap2Error> {
		if !matches!(packet.packet_kind(), PacketKind::Session) {
			return Ok(());
		}

		let Some(control_session_id) = self.control_session_id else {
			return Err(Iap2Error::UnexpectedMachineState(
				"control session id missing after link negotiation",
			));
		};

		let (session_id, payload) = packet.session_payload()?;
		if session_id != control_session_id {
			return Ok(());
		}

		for message in ControlMessageKind::decode_stream(payload)? {
			self.handle_control_message(message, outputs)?;
		}

		if self.authenticated && self.identified && !matches!(self.state, MachineState::Failed) {
			self.state = MachineState::ReadyForCarPlay;
			outputs.push(MachineOutput::CarPlayReady);
		}

		Ok(())
	}

	fn handle_control_message(
		&mut self,
		message: ControlMessageKind,
		outputs: &mut Vec<MachineOutput>,
	) -> Result<(), Iap2Error> {
		match message {
			ControlMessageKind::RequestAuthenticationCertificate {
				request_serial_number,
			} => {
				if request_serial_number {
					if let Some(serial) =
						&self.accessory_profile.expected_authentication_serial_number
					{
						outputs.push(self.wrap_control_message(
							ControlMessageKind::AccessoryAuthenticationSerialNumber {
								serial_number: serial.clone(),
							},
						)?);
					}
				}

				let certificate = self
					.accessory_profile
					.expected_authentication_certificate
					.clone()
					.unwrap_or_else(|| vec![0x01, 0x02, 0x03]);
				outputs.push(self.wrap_control_message(
					ControlMessageKind::AuthenticationCertificate { certificate },
				)?);
				self.state = MachineState::WaitingForAuthenticationChallengeResponse;
			}
			ControlMessageKind::AuthenticationCertificate { certificate } => {
				let certificate_verified = self.verify_authentication_certificate(&certificate);
				if !certificate_verified {
					outputs
						.push(self.wrap_control_message(ControlMessageKind::AuthenticationFailed)?);
					outputs.push(MachineOutput::AuthenticationOutcome {
						accepted: false,
						fake: false,
					});
					self.state = MachineState::Failed;
					return Ok(());
				}

				let challenge = self.next_authentication_challenge();
				self.pending_auth_challenge = Some(challenge.clone());
				outputs.push(self.wrap_control_message(
					ControlMessageKind::RequestAuthenticationChallengeResponse { challenge },
				)?);
				self.state = MachineState::WaitingForAuthenticationChallengeResponse;
			}
			ControlMessageKind::RequestAuthenticationChallengeResponse { challenge } => {
				self.pending_auth_challenge = Some(challenge.clone());
				let response = self
					.accessory_profile
					.expected_authentication_response
					.clone()
					.unwrap_or_else(|| fake_sign_challenge(&challenge));

				outputs.push(self.wrap_control_message(
					ControlMessageKind::AuthenticationResponse { response },
				)?);
			}
			ControlMessageKind::AuthenticationResponse { response } => {
				let verified = self.verify_authentication_response(&response);
				if verified {
					self.authenticated = true;
					outputs.push(
						self.wrap_control_message(ControlMessageKind::AuthenticationSucceeded)?,
					);
					outputs.push(MachineOutput::AuthenticationOutcome {
						accepted: true,
						fake: matches!(
							self.authentication_policy,
							AuthenticationPolicy::AlwaysAccept
						),
					});
					if !self.identified {
						self.state = MachineState::WaitingForIdentificationInformation;
					}
				} else {
					outputs
						.push(self.wrap_control_message(ControlMessageKind::AuthenticationFailed)?);
					outputs.push(MachineOutput::AuthenticationOutcome {
						accepted: false,
						fake: false,
					});
					self.state = MachineState::Failed;
				}
			}
			ControlMessageKind::AccessoryAuthenticationSerialNumber { .. } => {}
			ControlMessageKind::IdentificationInformation(info) => {
				self.last_identification = Some(info.clone());
				let accepted = self.validate_identification_for_carplay(&info);
				if accepted {
					self.identified = true;
					outputs.push(
						self.wrap_control_message(ControlMessageKind::IdentificationAccepted)?,
					);
					outputs.push(MachineOutput::IdentificationOutcome {
						outcome: IdentificationOutcome::Accepted,
					});
					if !self.authenticated {
						outputs.push(self.wrap_control_message(
							ControlMessageKind::RequestAuthenticationCertificate {
								request_serial_number: true,
							},
						)?);
						self.state = MachineState::WaitingForAuthenticationMaterial;
					}
				} else {
					let rejected = identify_rejected_parameter_ids(&info);
					outputs.push(self.wrap_control_message(
						ControlMessageKind::IdentificationRejected {
							rejected_parameter_ids: rejected,
							unsupported_messages_sent_by_accessory: Vec::new(),
							unsupported_messages_received_from_device: Vec::new(),
						},
					)?);
					outputs.push(MachineOutput::IdentificationOutcome {
						outcome: IdentificationOutcome::Rejected,
					});
					self.state = MachineState::WaitingForIdentificationRetry;
				}
			}
			ControlMessageKind::CancelIdentification => {
				self.state = MachineState::Failed;
			}
			ControlMessageKind::Unknown(_) => {}
			_ => {}
		}

		Ok(())
	}

	fn verify_authentication_response(&self, response: &[u8]) -> bool {
		match self.authentication_policy {
			AuthenticationPolicy::AlwaysAccept => true,
			AuthenticationPolicy::Strict => {
				if let Some(expected) = &self.accessory_profile.expected_authentication_response {
					return expected.as_slice() == response;
				}

				if let Some(challenge) = &self.pending_auth_challenge {
					let expected = fake_sign_challenge(challenge);
					return expected.as_slice() == response;
				}

				false
			}
		}
	}

	fn verify_authentication_certificate(&self, certificate: &[u8]) -> bool {
		match self.authentication_policy {
			AuthenticationPolicy::AlwaysAccept => true,
			AuthenticationPolicy::Strict => {
				if let Some(expected) = &self.accessory_profile.expected_authentication_certificate
				{
					return expected.as_slice() == certificate;
				}

				!certificate.is_empty()
			}
		}
	}

	fn next_authentication_challenge(&mut self) -> Vec<u8> {
		self.auth_challenge_counter = self.auth_challenge_counter.wrapping_add(1);
		let mut challenge = vec![0u8; 16];
		let counter = self.auth_challenge_counter.to_be_bytes();
		challenge[0..4].copy_from_slice(&counter);
		challenge[4] = self.link_record.current_sequence_without_increment();
		challenge[5] = self.link_record.last_received_in_sequence();
		challenge[6] = self.control_session_id.unwrap_or(0);
		challenge[7] = 0;
		challenge[8..12].copy_from_slice(&counter);
		challenge[12..16].copy_from_slice(&counter);
		challenge
	}

	fn validate_identification_for_carplay(&self, info: &IdentificationInformation) -> bool {
		if !info.has_minimum_manufacturing_fields() {
			return false;
		}

		if !info.has_vehicle_information_component() {
			return false;
		}

		if !self.matches_expected_identification(info) {
			return false;
		}

		info.supports_wired_carplay()
	}

	fn matches_expected_identification(&self, info: &IdentificationInformation) -> bool {
		let Some(expected) = &self.accessory_profile.identification else {
			return true;
		};

		if expected.name.is_some() && expected.name != info.name {
			return false;
		}
		if expected.model_identifier.is_some() && expected.model_identifier != info.model_identifier
		{
			return false;
		}
		if expected.manufacturer.is_some() && expected.manufacturer != info.manufacturer {
			return false;
		}
		if expected.serial_number.is_some() && expected.serial_number != info.serial_number {
			return false;
		}

		true
	}

	fn wrap_control_message(
		&mut self,
		message: ControlMessageKind,
	) -> Result<MachineOutput, Iap2Error> {
		let Some(control_session_id) = self.control_session_id else {
			return Err(Iap2Error::UnexpectedMachineState(
				"cannot send control session message before negotiation",
			));
		};

		let payload = message.encode()?;
		let sequence = self.link_record.next_sequence_for_payload();
		let ack = self.link_record.last_received_in_sequence();
		let packet = LinkPacket::session(sequence, ack, control_session_id, payload)?;
		Self::emit_packet(packet)
	}

	fn emit_packet(packet: LinkPacket) -> Result<MachineOutput, Iap2Error> {
		Ok(MachineOutput::Transmit(packet.encode()?))
	}

	fn ensure_control_session_v2(
		&self,
		payload: &LinkSynchronizationPayload,
	) -> Result<(), Iap2Error> {
		let has_v2 = payload.sessions.iter().any(|session| {
			session.session_type == 0
				&& session.session_version
					== self.accessory_profile.expected_control_session_version
		});

		if has_v2 {
			Ok(())
		} else {
			Err(Iap2Error::MissingControlSessionV2)
		}
	}

	fn reset(&mut self) {
		self.state = MachineState::WaitingForSyn;
		self.transmit_window = None;
		self.authenticated = false;
		self.identified = false;
		self.control_session_id = None;
		self.pending_auth_challenge = None;
		self.last_identification = None;
		self.auth_challenge_counter = 0;
		self.parser.clear();
	}
}

fn find_control_session_id(
	payload: &LinkSynchronizationPayload,
	expected_id: u8,
	required_version: u8,
) -> Option<u8> {
	if let Some(session) = payload.sessions.iter().find(|session| {
		session.identifier == expected_id
			&& session.session_type == 0
			&& session.session_version == required_version
	}) {
		return Some(session.identifier);
	}

	payload
		.sessions
		.iter()
		.find(|session| session.session_type == 0 && session.session_version == required_version)
		.map(|session| session.identifier)
}

fn identify_rejected_parameter_ids(info: &IdentificationInformation) -> Vec<u16> {
	let mut ids = Vec::new();
	if info.name.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_NAME);
	}
	if info.model_identifier.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_MODEL_IDENTIFIER);
	}
	if info.manufacturer.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_MANUFACTURER);
	}
	if info.serial_number.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_SERIAL_NUMBER);
	}
	if info.firmware_version.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_FIRMWARE_VERSION);
	}
	if info.hardware_version.as_ref().is_none_or(String::is_empty) {
		ids.push(IDENT_PARAM_HARDWARE_VERSION);
	}
	if info.vehicle_information_components.is_empty() {
		ids.push(IDENT_PARAM_VEHICLE_INFORMATION_COMPONENT);
	}
	if !info.supports_wired_carplay() {
		ids.push(IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT);
	}

	ids.sort_unstable();
	ids.dedup();
	ids
}

fn fake_sign_challenge(challenge: &[u8]) -> Vec<u8> {
	let mut signed = challenge.to_vec();
	signed.reverse();
	if signed.is_empty() {
		signed.push(0u8);
	}

	signed
}

#[cfg(test)]
mod tests {
	use super::{
		AccessoryProfile, AuthenticationPolicy, Iap2Machine, Iap2MachineConfig,
		IdentificationOutcome, MachineOutput,
	};
	use crate::iap2::{
		control_session::{
			ControlMessageKind, IdentificationInformation, UsbHostTransportComponent,
			VehicleInformationComponent,
		},
		packet::LinkPacket,
		sync::{LinkSynchronizationPayload, SessionDescriptor, TransportProfile},
	};

	fn machine_config() -> Iap2MachineConfig {
		Iap2MachineConfig {
			local_sync_payload: LinkSynchronizationPayload::recommended_for_transport(
				TransportProfile::UsbHostFullSpeed,
				vec![SessionDescriptor {
					identifier: 1,
					session_type: 0,
					session_version: 2,
				}],
			),
			local_initial_psn: 10,
			accessory_profile: AccessoryProfile {
				expected_control_session_id: 1,
				expected_control_session_version: 2,
				expected_authentication_certificate: Some(vec![0xaa]),
				expected_authentication_serial_number: Some(vec![0x01]),
				expected_authentication_response: Some(vec![0x55]),
				identification: None,
			},
			authentication_policy: AuthenticationPolicy::AlwaysAccept,
		}
	}

	fn complete_link_handshake(machine: &mut Iap2Machine) {
		let peer_sync = LinkSynchronizationPayload::recommended_for_transport(
			TransportProfile::UsbHostFullSpeed,
			vec![SessionDescriptor {
				identifier: 1,
				session_type: 0,
				session_version: 2,
			}],
		);
		let syn = LinkPacket::syn(3, peer_sync).expect("build syn");
		let out = machine.on_link_packet(syn).expect("handle syn");
		assert!(
			out.iter()
				.any(|output| matches!(output, MachineOutput::Transmit(_)))
		);

		let ack = LinkPacket::ack(4, 10).expect("build ack");
		let out = machine.on_link_packet(ack).expect("handle ack");
		assert!(
			out.iter()
				.any(|output| matches!(output, MachineOutput::Transmit(_)))
		);
	}

	fn feed_control_message(machine: &mut Iap2Machine, sequence: u8, message: ControlMessageKind) {
		let payload = message.encode().expect("encode control message");
		let packet = LinkPacket::session(sequence, sequence.wrapping_sub(1), 1, payload)
			.expect("build control session packet");
		let _ = machine
			.on_link_packet(packet)
			.expect("handle control message packet");
	}

	#[test]
	fn state_machine_reaches_ready_with_fake_auth() {
		let mut machine = Iap2Machine::new(machine_config());
		complete_link_handshake(&mut machine);

		let info = IdentificationInformation {
			name: Some("Head Unit".to_string()),
			model_identifier: Some("HU-01".to_string()),
			manufacturer: Some("Example".to_string()),
			serial_number: Some("123".to_string()),
			firmware_version: Some("1.0".to_string()),
			hardware_version: Some("A".to_string()),
			usb_host_transports: vec![UsbHostTransportComponent {
				identifier: Some(1),
				name: Some("USB".to_string()),
				supports_iap2_connection: true,
				carplay_interface_number: Some(3),
				supports_carplay: true,
			}],
			vehicle_information_components: vec![VehicleInformationComponent {
				identifier: Some(1),
				name: Some("Vehicle".to_string()),
			}],
			..Default::default()
		};

		feed_control_message(
			&mut machine,
			20,
			ControlMessageKind::IdentificationInformation(info),
		);
		feed_control_message(
			&mut machine,
			21,
			ControlMessageKind::AuthenticationResponse { response: vec![0] },
		);

		let snapshot = machine.snapshot();
		assert!(snapshot.authenticated);
		assert!(snapshot.identified);
		assert!(machine.is_ready_for_carplay());
	}

	#[test]
	fn missing_vehicle_info_is_rejected() {
		let mut machine = Iap2Machine::new(machine_config());
		complete_link_handshake(&mut machine);

		let info = IdentificationInformation {
			name: Some("Head Unit".to_string()),
			model_identifier: Some("HU-01".to_string()),
			manufacturer: Some("Example".to_string()),
			serial_number: Some("123".to_string()),
			firmware_version: Some("1.0".to_string()),
			hardware_version: Some("A".to_string()),
			usb_host_transports: vec![UsbHostTransportComponent {
				identifier: Some(1),
				name: Some("USB".to_string()),
				supports_iap2_connection: true,
				carplay_interface_number: Some(3),
				supports_carplay: true,
			}],
			..Default::default()
		};

		let payload = ControlMessageKind::IdentificationInformation(info)
			.encode()
			.expect("encode control message");
		let packet = LinkPacket::session(20, 19, 1, payload).expect("build packet");
		let outputs = machine.on_link_packet(packet).expect("handle packet");

		assert!(outputs.iter().any(|output| {
			matches!(
				output,
				MachineOutput::IdentificationOutcome {
					outcome: IdentificationOutcome::Rejected
				}
			)
		}));
	}
}
