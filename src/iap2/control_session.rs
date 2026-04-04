use std::collections::BTreeSet;

use super::error::Iap2Error;

pub const CONTROL_MESSAGE_START: [u8; 2] = [0x40, 0x40];

const CONTROL_HEADER_LEN: usize = 6;
const CONTROL_PARAMETER_HEADER_LEN: usize = 4;

pub const MSG_START_IDENTIFICATION: u16 = 0x1d00;
pub const MSG_IDENTIFICATION_INFORMATION: u16 = 0x1d01;
pub const MSG_IDENTIFICATION_ACCEPTED: u16 = 0x1d02;
pub const MSG_IDENTIFICATION_REJECTED: u16 = 0x1d03;
pub const MSG_CANCEL_IDENTIFICATION: u16 = 0x1d05;
pub const MSG_IDENTIFICATION_INFORMATION_UPDATE: u16 = 0x1d06;

pub const MSG_REQUEST_AUTHENTICATION_CERTIFICATE: u16 = 0xaa00;
pub const MSG_AUTHENTICATION_CERTIFICATE: u16 = 0xaa01;
pub const MSG_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE: u16 = 0xaa02;
pub const MSG_AUTHENTICATION_RESPONSE: u16 = 0xaa03;
pub const MSG_AUTHENTICATION_FAILED: u16 = 0xaa04;
pub const MSG_AUTHENTICATION_SUCCEEDED: u16 = 0xaa05;
pub const MSG_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER: u16 = 0xaa06;

pub const PARAM_REQUEST_AUTH_CERTIFICATE_SERIAL_NUMBER: u16 = 0;
pub const PARAM_AUTHENTICATION_CERTIFICATE: u16 = 0;
pub const PARAM_AUTHENTICATION_CHALLENGE: u16 = 0;
pub const PARAM_AUTHENTICATION_RESPONSE: u16 = 0;
pub const PARAM_AUTHENTICATION_SERIAL_NUMBER: u16 = 0;

pub const IDENT_PARAM_NAME: u16 = 0;
pub const IDENT_PARAM_MODEL_IDENTIFIER: u16 = 1;
pub const IDENT_PARAM_MANUFACTURER: u16 = 2;
pub const IDENT_PARAM_SERIAL_NUMBER: u16 = 3;
pub const IDENT_PARAM_FIRMWARE_VERSION: u16 = 4;
pub const IDENT_PARAM_HARDWARE_VERSION: u16 = 5;
pub const IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY: u16 = 6;
pub const IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE: u16 = 7;
pub const IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT: u16 = 16;
pub const IDENT_PARAM_VEHICLE_INFORMATION_COMPONENT: u16 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlParameter {
	pub id: u16,
	pub data: Vec<u8>,
}

impl ControlParameter {
	pub fn none(id: u16) -> Self {
		Self {
			id,
			data: Vec::new(),
		}
	}

	pub fn blob(id: u16, data: Vec<u8>) -> Self {
		Self { id, data }
	}

	pub fn uint8(id: u16, value: u8) -> Self {
		Self {
			id,
			data: vec![value],
		}
	}

	pub fn uint16(id: u16, value: u16) -> Self {
		Self {
			id,
			data: value.to_be_bytes().to_vec(),
		}
	}

	pub fn uint16_array(id: u16, values: &[u16]) -> Self {
		let mut data = Vec::with_capacity(values.len() * 2);
		for value in values {
			data.extend_from_slice(&value.to_be_bytes());
		}

		Self { id, data }
	}

	pub fn utf8(id: u16, value: &str) -> Self {
		Self {
			id,
			data: value.as_bytes().to_vec(),
		}
	}

	pub fn as_utf8(&self) -> Result<String, Iap2Error> {
		String::from_utf8(self.data.clone()).map_err(|_| Iap2Error::InvalidControlParameterValue {
			parameter_id: self.id,
			reason: "invalid utf8",
		})
	}

	pub fn as_u8(&self) -> Result<u8, Iap2Error> {
		if self.data.len() != 1 {
			return Err(Iap2Error::InvalidControlParameterValue {
				parameter_id: self.id,
				reason: "expected uint8 value",
			});
		}

		Ok(self.data[0])
	}

	pub fn as_u16(&self) -> Result<u16, Iap2Error> {
		if self.data.len() != 2 {
			return Err(Iap2Error::InvalidControlParameterValue {
				parameter_id: self.id,
				reason: "expected uint16 value",
			});
		}

		Ok(u16::from_be_bytes([self.data[0], self.data[1]]))
	}

	pub fn as_u16_array(&self) -> Result<Vec<u16>, Iap2Error> {
		if self.data.len() % 2 != 0 {
			return Err(Iap2Error::InvalidControlParameterValue {
				parameter_id: self.id,
				reason: "expected packed uint16 array",
			});
		}

		let mut out = Vec::with_capacity(self.data.len() / 2);
		for chunk in self.data.chunks_exact(2) {
			out.push(u16::from_be_bytes([chunk[0], chunk[1]]));
		}

		Ok(out)
	}

	fn encode(&self) -> Result<Vec<u8>, Iap2Error> {
		let length = CONTROL_PARAMETER_HEADER_LEN + self.data.len();
		if length > usize::from(u16::MAX) {
			return Err(Iap2Error::InvalidControlMessage("parameter too long"));
		}

		let mut out = Vec::with_capacity(length);
		out.extend_from_slice(&(length as u16).to_be_bytes());
		out.extend_from_slice(&self.id.to_be_bytes());
		out.extend_from_slice(&self.data);
		Ok(out)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlMessage {
	pub id: u16,
	pub parameters: Vec<ControlParameter>,
}

impl ControlMessage {
	pub fn new(id: u16, parameters: Vec<ControlParameter>) -> Self {
		Self { id, parameters }
	}

	pub fn encode(&self) -> Result<Vec<u8>, Iap2Error> {
		let mut body = Vec::new();
		for parameter in &self.parameters {
			body.extend_from_slice(&parameter.encode()?);
		}

		let length = CONTROL_HEADER_LEN + body.len();
		if length > usize::from(u16::MAX) {
			return Err(Iap2Error::InvalidControlMessage("control message too long"));
		}

		let mut out = Vec::with_capacity(length);
		out.extend_from_slice(&CONTROL_MESSAGE_START);
		out.extend_from_slice(&(length as u16).to_be_bytes());
		out.extend_from_slice(&self.id.to_be_bytes());
		out.extend_from_slice(&body);

		Ok(out)
	}

	pub fn decode(bytes: &[u8]) -> Result<Self, Iap2Error> {
		if bytes.len() < CONTROL_HEADER_LEN {
			return Err(Iap2Error::BufferTooShort {
				expected_at_least: CONTROL_HEADER_LEN,
				actual: bytes.len(),
			});
		}

		if bytes[0] != CONTROL_MESSAGE_START[0] || bytes[1] != CONTROL_MESSAGE_START[1] {
			return Err(Iap2Error::InvalidControlMessageStart {
				first: bytes[0],
				second: bytes[1],
			});
		}

		let advertised = u16::from_be_bytes([bytes[2], bytes[3]]);
		if usize::from(advertised) != bytes.len() {
			return Err(Iap2Error::InvalidControlMessageLength {
				advertised,
				actual: bytes.len(),
			});
		}

		let id = u16::from_be_bytes([bytes[4], bytes[5]]);
		let parameters = decode_parameter_stream(&bytes[CONTROL_HEADER_LEN..])?;

		Ok(Self { id, parameters })
	}

	pub fn first_parameter(&self, id: u16) -> Option<&ControlParameter> {
		self.parameters.iter().find(|parameter| parameter.id == id)
	}

	pub fn parameters_by_id(&self, id: u16) -> impl Iterator<Item = &ControlParameter> {
		self.parameters
			.iter()
			.filter(move |parameter| parameter.id == id)
	}

	pub fn has_parameter(&self, id: u16) -> bool {
		self.first_parameter(id).is_some()
	}
}

fn decode_parameter_stream(bytes: &[u8]) -> Result<Vec<ControlParameter>, Iap2Error> {
	let mut cursor = 0usize;
	let mut parameters = Vec::new();

	while cursor < bytes.len() {
		if bytes.len() - cursor < CONTROL_PARAMETER_HEADER_LEN {
			return Err(Iap2Error::BufferTooShort {
				expected_at_least: CONTROL_PARAMETER_HEADER_LEN,
				actual: bytes.len() - cursor,
			});
		}

		let advertised = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]);
		let advertised = usize::from(advertised);
		if advertised < CONTROL_PARAMETER_HEADER_LEN || cursor + advertised > bytes.len() {
			return Err(Iap2Error::InvalidControlParameterLength {
				advertised: advertised as u16,
				remaining: bytes.len() - cursor,
			});
		}

		let id = u16::from_be_bytes([bytes[cursor + 2], bytes[cursor + 3]]);
		let data_start = cursor + CONTROL_PARAMETER_HEADER_LEN;
		let data_end = cursor + advertised;
		parameters.push(ControlParameter {
			id,
			data: bytes[data_start..data_end].to_vec(),
		});

		cursor += advertised;
	}

	Ok(parameters)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsbHostTransportComponent {
	pub identifier: Option<u16>,
	pub name: Option<String>,
	pub supports_iap2_connection: bool,
	pub carplay_interface_number: Option<u8>,
	pub supports_carplay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VehicleInformationComponent {
	pub identifier: Option<u16>,
	pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentificationInformation {
	pub name: Option<String>,
	pub model_identifier: Option<String>,
	pub manufacturer: Option<String>,
	pub serial_number: Option<String>,
	pub firmware_version: Option<String>,
	pub hardware_version: Option<String>,
	pub messages_sent_by_accessory: Vec<u16>,
	pub messages_received_from_device: Vec<u16>,
	pub usb_host_transports: Vec<UsbHostTransportComponent>,
	pub vehicle_information_components: Vec<VehicleInformationComponent>,
}

impl IdentificationInformation {
	pub fn has_minimum_manufacturing_fields(&self) -> bool {
		self.name.as_ref().is_some_and(|s| !s.is_empty())
			&& self
				.model_identifier
				.as_ref()
				.is_some_and(|s| !s.is_empty())
			&& self.manufacturer.as_ref().is_some_and(|s| !s.is_empty())
			&& self.serial_number.as_ref().is_some_and(|s| !s.is_empty())
			&& self
				.firmware_version
				.as_ref()
				.is_some_and(|s| !s.is_empty())
			&& self
				.hardware_version
				.as_ref()
				.is_some_and(|s| !s.is_empty())
	}

	pub fn supports_wired_carplay(&self) -> bool {
		self.usb_host_transports.iter().any(|component| {
			component.supports_iap2_connection
				&& component.supports_carplay
				&& component.carplay_interface_number.is_some()
		})
	}

	pub fn has_vehicle_information_component(&self) -> bool {
		!self.vehicle_information_components.is_empty()
	}

	pub fn encode_parameters(&self) -> Result<Vec<ControlParameter>, Iap2Error> {
		let mut parameters = Vec::new();

		if let Some(value) = &self.name {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_NAME, value));
		}
		if let Some(value) = &self.model_identifier {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_MODEL_IDENTIFIER, value));
		}
		if let Some(value) = &self.manufacturer {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_MANUFACTURER, value));
		}
		if let Some(value) = &self.serial_number {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_SERIAL_NUMBER, value));
		}
		if let Some(value) = &self.firmware_version {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_FIRMWARE_VERSION, value));
		}
		if let Some(value) = &self.hardware_version {
			parameters.push(ControlParameter::utf8(IDENT_PARAM_HARDWARE_VERSION, value));
		}

		if !self.messages_sent_by_accessory.is_empty() {
			parameters.push(ControlParameter::uint16_array(
				IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY,
				&self.messages_sent_by_accessory,
			));
		}

		if !self.messages_received_from_device.is_empty() {
			parameters.push(ControlParameter::uint16_array(
				IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE,
				&self.messages_received_from_device,
			));
		}

		for component in &self.usb_host_transports {
			parameters.push(encode_group(
				IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT,
				component.encode_group_parameters(),
			)?);
		}

		for component in &self.vehicle_information_components {
			parameters.push(encode_group(
				IDENT_PARAM_VEHICLE_INFORMATION_COMPONENT,
				component.encode_group_parameters(),
			)?);
		}

		Ok(parameters)
	}
}

impl UsbHostTransportComponent {
	fn decode_group(group: &[ControlParameter]) -> Result<Self, Iap2Error> {
		Ok(Self {
			identifier: first_parameter(group, 0)
				.map(ControlParameter::as_u16)
				.transpose()?,
			name: first_parameter(group, 1)
				.map(ControlParameter::as_utf8)
				.transpose()?,
			supports_iap2_connection: has_flag_parameter(group, 2),
			carplay_interface_number: first_parameter(group, 3)
				.map(ControlParameter::as_u8)
				.transpose()?,
			supports_carplay: has_flag_parameter(group, 4),
		})
	}

	fn encode_group_parameters(&self) -> Vec<ControlParameter> {
		let mut out = Vec::new();
		if let Some(value) = self.identifier {
			out.push(ControlParameter::uint16(0, value));
		}
		if let Some(value) = &self.name {
			out.push(ControlParameter::utf8(1, value));
		}
		if self.supports_iap2_connection {
			out.push(ControlParameter::none(2));
		}
		if let Some(value) = self.carplay_interface_number {
			out.push(ControlParameter::uint8(3, value));
		}
		if self.supports_carplay {
			out.push(ControlParameter::none(4));
		}

		out
	}
}

impl VehicleInformationComponent {
	fn decode_group(group: &[ControlParameter]) -> Result<Self, Iap2Error> {
		Ok(Self {
			identifier: first_parameter(group, 0)
				.map(ControlParameter::as_u16)
				.transpose()?,
			name: first_parameter(group, 1)
				.map(ControlParameter::as_utf8)
				.transpose()?,
		})
	}

	fn encode_group_parameters(&self) -> Vec<ControlParameter> {
		let mut out = Vec::new();
		if let Some(value) = self.identifier {
			out.push(ControlParameter::uint16(0, value));
		}
		if let Some(value) = &self.name {
			out.push(ControlParameter::utf8(1, value));
		}

		out
	}
}

fn encode_group(
	group_parameter_id: u16,
	fields: Vec<ControlParameter>,
) -> Result<ControlParameter, Iap2Error> {
	let mut data = Vec::new();
	for field in fields {
		let encoded = field.encode()?;
		data.extend_from_slice(&encoded);
	}

	Ok(ControlParameter {
		id: group_parameter_id,
		data,
	})
}

fn has_flag_parameter(parameters: &[ControlParameter], id: u16) -> bool {
	parameters.iter().any(|parameter| parameter.id == id)
}

fn first_parameter(parameters: &[ControlParameter], id: u16) -> Option<&ControlParameter> {
	parameters.iter().find(|parameter| parameter.id == id)
}

fn parse_group_components<T>(
	message: &ControlMessage,
	group_parameter_id: u16,
	decoder: impl Fn(&[ControlParameter]) -> Result<T, Iap2Error>,
) -> Result<Vec<T>, Iap2Error> {
	let mut out = Vec::new();
	for group in message.parameters_by_id(group_parameter_id) {
		let decoded = decode_parameter_stream(&group.data)?;
		out.push(decoder(&decoded)?);
	}

	Ok(out)
}

fn parse_identification_information(
	message: &ControlMessage,
) -> Result<IdentificationInformation, Iap2Error> {
	let messages_sent_by_accessory = message
		.first_parameter(IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY)
		.map(ControlParameter::as_u16_array)
		.transpose()?
		.unwrap_or_default();

	let messages_received_from_device = message
		.first_parameter(IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE)
		.map(ControlParameter::as_u16_array)
		.transpose()?
		.unwrap_or_default();

	Ok(IdentificationInformation {
		name: message
			.first_parameter(IDENT_PARAM_NAME)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		model_identifier: message
			.first_parameter(IDENT_PARAM_MODEL_IDENTIFIER)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		manufacturer: message
			.first_parameter(IDENT_PARAM_MANUFACTURER)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		serial_number: message
			.first_parameter(IDENT_PARAM_SERIAL_NUMBER)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		firmware_version: message
			.first_parameter(IDENT_PARAM_FIRMWARE_VERSION)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		hardware_version: message
			.first_parameter(IDENT_PARAM_HARDWARE_VERSION)
			.map(ControlParameter::as_utf8)
			.transpose()?,
		messages_sent_by_accessory,
		messages_received_from_device,
		usb_host_transports: parse_group_components(
			message,
			IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT,
			UsbHostTransportComponent::decode_group,
		)?,
		vehicle_information_components: parse_group_components(
			message,
			IDENT_PARAM_VEHICLE_INFORMATION_COMPONENT,
			VehicleInformationComponent::decode_group,
		)?,
	})
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessageKind {
	RequestAuthenticationCertificate {
		request_serial_number: bool,
	},
	AuthenticationCertificate {
		certificate: Vec<u8>,
	},
	RequestAuthenticationChallengeResponse {
		challenge: Vec<u8>,
	},
	AuthenticationResponse {
		response: Vec<u8>,
	},
	AuthenticationFailed,
	AuthenticationSucceeded,
	AccessoryAuthenticationSerialNumber {
		serial_number: Vec<u8>,
	},
	StartIdentification,
	IdentificationInformation(IdentificationInformation),
	IdentificationAccepted,
	IdentificationRejected {
		rejected_parameter_ids: Vec<u16>,
		unsupported_messages_sent_by_accessory: Vec<u16>,
		unsupported_messages_received_from_device: Vec<u16>,
	},
	CancelIdentification,
	IdentificationInformationUpdate(IdentificationInformation),
	Unknown(ControlMessage),
}

impl ControlMessageKind {
	pub fn decode_stream(bytes: &[u8]) -> Result<Vec<Self>, Iap2Error> {
		let mut cursor = 0usize;
		let mut out = Vec::new();

		while cursor < bytes.len() {
			if bytes.len() - cursor < CONTROL_HEADER_LEN {
				return Err(Iap2Error::BufferTooShort {
					expected_at_least: CONTROL_HEADER_LEN,
					actual: bytes.len() - cursor,
				});
			}

			if bytes[cursor] != CONTROL_MESSAGE_START[0]
				|| bytes[cursor + 1] != CONTROL_MESSAGE_START[1]
			{
				return Err(Iap2Error::InvalidControlMessageStart {
					first: bytes[cursor],
					second: bytes[cursor + 1],
				});
			}

			let advertised = u16::from_be_bytes([bytes[cursor + 2], bytes[cursor + 3]]);
			let advertised = usize::from(advertised);
			if advertised < CONTROL_HEADER_LEN || cursor + advertised > bytes.len() {
				return Err(Iap2Error::InvalidControlMessageLength {
					advertised: advertised as u16,
					actual: bytes.len() - cursor,
				});
			}

			let message = Self::decode(&bytes[cursor..cursor + advertised])?;
			out.push(message);
			cursor += advertised;
		}

		Ok(out)
	}

	pub fn decode(bytes: &[u8]) -> Result<Self, Iap2Error> {
		let message = ControlMessage::decode(bytes)?;

		let kind = match message.id {
			MSG_REQUEST_AUTHENTICATION_CERTIFICATE => Self::RequestAuthenticationCertificate {
				request_serial_number: message
					.has_parameter(PARAM_REQUEST_AUTH_CERTIFICATE_SERIAL_NUMBER),
			},
			MSG_AUTHENTICATION_CERTIFICATE => Self::AuthenticationCertificate {
				certificate: required_blob(&message, PARAM_AUTHENTICATION_CERTIFICATE)?,
			},
			MSG_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
				Self::RequestAuthenticationChallengeResponse {
					challenge: required_blob(&message, PARAM_AUTHENTICATION_CHALLENGE)?,
				}
			}
			MSG_AUTHENTICATION_RESPONSE => Self::AuthenticationResponse {
				response: required_blob(&message, PARAM_AUTHENTICATION_RESPONSE)?,
			},
			MSG_AUTHENTICATION_FAILED => Self::AuthenticationFailed,
			MSG_AUTHENTICATION_SUCCEEDED => Self::AuthenticationSucceeded,
			MSG_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER => {
				Self::AccessoryAuthenticationSerialNumber {
					serial_number: required_blob(&message, PARAM_AUTHENTICATION_SERIAL_NUMBER)?,
				}
			}
			MSG_START_IDENTIFICATION => Self::StartIdentification,
			MSG_IDENTIFICATION_INFORMATION => {
				Self::IdentificationInformation(parse_identification_information(&message)?)
			}
			MSG_IDENTIFICATION_ACCEPTED => Self::IdentificationAccepted,
			MSG_IDENTIFICATION_REJECTED => {
				let unsupported_messages_sent_by_accessory = message
					.first_parameter(IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY)
					.map(ControlParameter::as_u16_array)
					.transpose()?
					.unwrap_or_default();

				let unsupported_messages_received_from_device = message
					.first_parameter(IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE)
					.map(ControlParameter::as_u16_array)
					.transpose()?
					.unwrap_or_default();

				let mut rejected_parameter_ids = BTreeSet::new();
				for parameter in &message.parameters {
					if parameter.id == IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY
						|| parameter.id == IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE
					{
						continue;
					}
					rejected_parameter_ids.insert(parameter.id);
				}

				Self::IdentificationRejected {
					rejected_parameter_ids: rejected_parameter_ids.into_iter().collect(),
					unsupported_messages_sent_by_accessory,
					unsupported_messages_received_from_device,
				}
			}
			MSG_CANCEL_IDENTIFICATION => Self::CancelIdentification,
			MSG_IDENTIFICATION_INFORMATION_UPDATE => {
				Self::IdentificationInformationUpdate(parse_identification_information(&message)?)
			}
			_ => Self::Unknown(message),
		};

		Ok(kind)
	}

	pub fn to_message(&self) -> Result<ControlMessage, Iap2Error> {
		let message = match self {
			Self::RequestAuthenticationCertificate {
				request_serial_number,
			} => {
				let mut parameters = Vec::new();
				if *request_serial_number {
					parameters.push(ControlParameter::none(
						PARAM_REQUEST_AUTH_CERTIFICATE_SERIAL_NUMBER,
					));
				}

				ControlMessage::new(MSG_REQUEST_AUTHENTICATION_CERTIFICATE, parameters)
			}
			Self::AuthenticationCertificate { certificate } => ControlMessage::new(
				MSG_AUTHENTICATION_CERTIFICATE,
				vec![ControlParameter::blob(
					PARAM_AUTHENTICATION_CERTIFICATE,
					certificate.clone(),
				)],
			),
			Self::RequestAuthenticationChallengeResponse { challenge } => ControlMessage::new(
				MSG_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
				vec![ControlParameter::blob(
					PARAM_AUTHENTICATION_CHALLENGE,
					challenge.clone(),
				)],
			),
			Self::AuthenticationResponse { response } => ControlMessage::new(
				MSG_AUTHENTICATION_RESPONSE,
				vec![ControlParameter::blob(
					PARAM_AUTHENTICATION_RESPONSE,
					response.clone(),
				)],
			),
			Self::AuthenticationFailed => {
				ControlMessage::new(MSG_AUTHENTICATION_FAILED, Vec::new())
			}
			Self::AuthenticationSucceeded => {
				ControlMessage::new(MSG_AUTHENTICATION_SUCCEEDED, Vec::new())
			}
			Self::AccessoryAuthenticationSerialNumber { serial_number } => ControlMessage::new(
				MSG_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER,
				vec![ControlParameter::blob(
					PARAM_AUTHENTICATION_SERIAL_NUMBER,
					serial_number.clone(),
				)],
			),
			Self::StartIdentification => ControlMessage::new(MSG_START_IDENTIFICATION, Vec::new()),
			Self::IdentificationInformation(info) => {
				ControlMessage::new(MSG_IDENTIFICATION_INFORMATION, info.encode_parameters()?)
			}
			Self::IdentificationAccepted => {
				ControlMessage::new(MSG_IDENTIFICATION_ACCEPTED, Vec::new())
			}
			Self::IdentificationRejected {
				rejected_parameter_ids,
				unsupported_messages_sent_by_accessory,
				unsupported_messages_received_from_device,
			} => {
				let mut parameters = Vec::new();
				for rejected in rejected_parameter_ids {
					parameters.push(ControlParameter::none(*rejected));
				}
				if !unsupported_messages_sent_by_accessory.is_empty() {
					parameters.push(ControlParameter::uint16_array(
						IDENT_PARAM_MESSAGES_SENT_BY_ACCESSORY,
						unsupported_messages_sent_by_accessory,
					));
				}
				if !unsupported_messages_received_from_device.is_empty() {
					parameters.push(ControlParameter::uint16_array(
						IDENT_PARAM_MESSAGES_RECEIVED_FROM_DEVICE,
						unsupported_messages_received_from_device,
					));
				}

				ControlMessage::new(MSG_IDENTIFICATION_REJECTED, parameters)
			}
			Self::CancelIdentification => {
				ControlMessage::new(MSG_CANCEL_IDENTIFICATION, Vec::new())
			}
			Self::IdentificationInformationUpdate(info) => ControlMessage::new(
				MSG_IDENTIFICATION_INFORMATION_UPDATE,
				info.encode_parameters()?,
			),
			Self::Unknown(message) => message.clone(),
		};

		Ok(message)
	}

	pub fn encode(&self) -> Result<Vec<u8>, Iap2Error> {
		self.to_message()?.encode()
	}
}

fn required_parameter<'a>(
	message: &'a ControlMessage,
	parameter_id: u16,
) -> Result<&'a ControlParameter, Iap2Error> {
	message
		.first_parameter(parameter_id)
		.ok_or(Iap2Error::MissingControlParameter(parameter_id))
}

fn required_blob(message: &ControlMessage, parameter_id: u16) -> Result<Vec<u8>, Iap2Error> {
	Ok(required_parameter(message, parameter_id)?.data.clone())
}

#[cfg(test)]
mod tests {
	use super::{
		ControlMessageKind, IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT, IdentificationInformation,
		UsbHostTransportComponent,
	};

	#[test]
	fn authentication_certificate_round_trip() {
		let message = ControlMessageKind::AuthenticationCertificate {
			certificate: vec![0xde, 0xad, 0xbe, 0xef],
		};

		let encoded = message.encode().expect("encode");
		let decoded = ControlMessageKind::decode(&encoded).expect("decode");
		assert_eq!(decoded, message);
	}

	#[test]
	fn identification_information_round_trip_with_usb_host_group() {
		let info = IdentificationInformation {
			name: Some("Head Unit".to_string()),
			model_identifier: Some("HU-1".to_string()),
			manufacturer: Some("Example".to_string()),
			serial_number: Some("ABC123".to_string()),
			firmware_version: Some("1.0".to_string()),
			hardware_version: Some("A".to_string()),
			usb_host_transports: vec![UsbHostTransportComponent {
				identifier: Some(1),
				name: Some("USB iAP2".to_string()),
				supports_iap2_connection: true,
				carplay_interface_number: Some(3),
				supports_carplay: true,
			}],
			..Default::default()
		};

		let message = ControlMessageKind::IdentificationInformation(info.clone());
		let encoded = message.encode().expect("encode");
		let decoded = ControlMessageKind::decode(&encoded).expect("decode");

		match decoded {
			ControlMessageKind::IdentificationInformation(decoded_info) => {
				assert_eq!(decoded_info.name, info.name);
				assert_eq!(decoded_info.usb_host_transports.len(), 1);
				assert!(decoded_info.usb_host_transports[0].supports_carplay);
			}
			_ => panic!("unexpected message kind"),
		}
	}

	#[test]
	fn identification_rejected_contains_reported_ids() {
		let message = ControlMessageKind::IdentificationRejected {
			rejected_parameter_ids: vec![0, IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT],
			unsupported_messages_sent_by_accessory: vec![0x1234],
			unsupported_messages_received_from_device: vec![],
		};

		let encoded = message.encode().expect("encode");
		let decoded = ControlMessageKind::decode(&encoded).expect("decode");
		match decoded {
			ControlMessageKind::IdentificationRejected {
				rejected_parameter_ids,
				unsupported_messages_sent_by_accessory,
				..
			} => {
				assert!(rejected_parameter_ids.contains(&0));
				assert!(rejected_parameter_ids.contains(&IDENT_PARAM_USB_HOST_TRANSPORT_COMPONENT));
				assert_eq!(unsupported_messages_sent_by_accessory, vec![0x1234]);
			}
			_ => panic!("unexpected message kind"),
		}
	}
}
