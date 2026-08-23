//! Control-session message identifiers (spec §97).
//!
//! Generated from the Accessory Interface Specification R29 text; each entry keeps the
//! spec's `Source` column, which says who is allowed to send it. We impersonate the
//! *device*, so [`Source::Device`] messages are ours to send and [`Source::Accessory`]
//! messages are what the head unit sends us.

/// Which side of the connection originates a message (§97).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
	/// The Apple device sends it — that is us.
	Device,
	/// The accessory sends it — that is the head unit.
	Accessory,
}

/// `StartOOBBTPairing` (sent by the device).
pub const ID_START_OOBBT_PAIRING: u16 = 0x00B0;
/// `OOBBTPairingAccessoryInformation` (sent by the accessory).
pub const ID_OOBBT_PAIRING_ACCESSORY_INFORMATION: u16 = 0x00B1;
/// `OOBBTPairingLinkKeyInformation` (sent by the device).
pub const ID_OOBBT_PAIRING_LINK_KEY_INFORMATION: u16 = 0x00B2;
/// `OOBBTPairingCompletionInformation` (sent by the accessory).
pub const ID_OOBBT_PAIRING_COMPLETION_INFORMATION: u16 = 0x00B3;
/// `StopOOBBTPairing` (sent by the device).
pub const ID_STOP_OOBBT_PAIRING: u16 = 0x00B4;
/// `StartDestinationInformation` (sent by the accessory).
pub const ID_START_DESTINATION_INFORMATION: u16 = 0x10DA;
/// `DestinationInformation` (sent by the device).
pub const ID_DESTINATION_INFORMATION: u16 = 0x10DB;
/// `StopDestinationInformation` (sent by the accessory).
pub const ID_STOP_DESTINATION_INFORMATION: u16 = 0x10DC;
/// `DestinationInformationStatus` (sent by the accessory).
pub const ID_DESTINATION_INFORMATION_STATUS: u16 = 0x10DD;
/// `StartIdentification` (sent by the device).
pub const ID_START_IDENTIFICATION: u16 = 0x1D00;
/// `IdentificationInformation` (sent by the accessory).
pub const ID_IDENTIFICATION_INFORMATION: u16 = 0x1D01;
/// `IdentificationAccepted` (sent by the device).
pub const ID_IDENTIFICATION_ACCEPTED: u16 = 0x1D02;
/// `IdentificationRejected` (sent by the device).
pub const ID_IDENTIFICATION_REJECTED: u16 = 0x1D03;
/// `CancelIdentification` (sent by the accessory).
pub const ID_CANCEL_IDENTIFICATION: u16 = 0x1D05;
/// `IdentificationInformationUpdate` (sent by the accessory).
pub const ID_IDENTIFICATION_INFORMATION_UPDATE: u16 = 0x1D06;
/// `StartCallStateUpdates` (sent by the accessory).
pub const ID_START_CALL_STATE_UPDATES: u16 = 0x4154;
/// `CallStateUpdate` (sent by the device).
pub const ID_CALL_STATE_UPDATE: u16 = 0x4155;
/// `StopCallStateUpdates` (sent by the accessory).
pub const ID_STOP_CALL_STATE_UPDATES: u16 = 0x4156;
/// `StartCommunicationsUpdates` (sent by the accessory).
pub const ID_START_COMMUNICATIONS_UPDATES: u16 = 0x4157;
/// `CommunicationsUpdate` (sent by the device).
pub const ID_COMMUNICATIONS_UPDATE: u16 = 0x4158;
/// `StopCommunicationsUpdates` (sent by the accessory).
pub const ID_STOP_COMMUNICATIONS_UPDATES: u16 = 0x4159;
/// `InitiateCall` (sent by the accessory).
pub const ID_INITIATE_CALL: u16 = 0x415A;
/// `AcceptCall` (sent by the accessory).
pub const ID_ACCEPT_CALL: u16 = 0x415B;
/// `EndCall` (sent by the accessory).
pub const ID_END_CALL: u16 = 0x415C;
/// `SwapCalls` (sent by the accessory).
pub const ID_SWAP_CALLS: u16 = 0x415D;
/// `MergeCalls` (sent by the accessory).
pub const ID_MERGE_CALLS: u16 = 0x415E;
/// `HoldStatusUpdate` (sent by the accessory).
pub const ID_HOLD_STATUS_UPDATE: u16 = 0x415F;
/// `MuteStatusUpdate` (sent by the accessory).
pub const ID_MUTE_STATUS_UPDATE: u16 = 0x4160;
/// `SendDTMF` (sent by the accessory).
pub const ID_SEND_DTMF: u16 = 0x4161;
/// `StartListUpdates` (sent by the accessory).
pub const ID_START_LIST_UPDATES: u16 = 0x4170;
/// `ListUpdate` (sent by the device).
pub const ID_LIST_UPDATE: u16 = 0x4171;
/// `StopListUpdates` (sent by the accessory).
pub const ID_STOP_LIST_UPDATES: u16 = 0x4172;
/// `StartMediaLibraryInformation` (sent by the accessory).
pub const ID_START_MEDIA_LIBRARY_INFORMATION: u16 = 0x4C00;
/// `MediaLibraryInformation` (sent by the device).
pub const ID_MEDIA_LIBRARY_INFORMATION: u16 = 0x4C01;
/// `StopMediaLibraryInformation` (sent by the accessory).
pub const ID_STOP_MEDIA_LIBRARY_INFORMATION: u16 = 0x4C02;
/// `StartMediaLibraryUpdates` (sent by the accessory).
pub const ID_START_MEDIA_LIBRARY_UPDATES: u16 = 0x4C03;
/// `MediaLibraryUpdate` (sent by the device).
pub const ID_MEDIA_LIBRARY_UPDATE: u16 = 0x4C04;
/// `StopMediaLibraryUpdates` (sent by the accessory).
pub const ID_STOP_MEDIA_LIBRARY_UPDATES: u16 = 0x4C05;
/// `PlayMediaLibraryCurrentSelection` (sent by the accessory).
pub const ID_PLAY_MEDIA_LIBRARY_CURRENT_SELECTION: u16 = 0x4C06;
/// `PlayMediaLibraryItems` (sent by the accessory).
pub const ID_PLAY_MEDIA_LIBRARY_ITEMS: u16 = 0x4C07;
/// `PlayMediaLibraryCollection` (sent by the accessory).
pub const ID_PLAY_MEDIA_LIBRARY_COLLECTION: u16 = 0x4C08;
/// `PlayMediaLibrarySpecial` (sent by the accessory).
pub const ID_PLAY_MEDIA_LIBRARY_SPECIAL: u16 = 0x4C09;
/// `BluetoothComponentInformation` (sent by the accessory).
pub const ID_BLUETOOTH_COMPONENT_INFORMATION: u16 = 0x4E01;
/// `StartBluetoothConnectionUpdates` (sent by the accessory).
pub const ID_START_BLUETOOTH_CONNECTION_UPDATES: u16 = 0x4E03;
/// `BluetoothConnectionUpdate` (sent by the device).
pub const ID_BLUETOOTH_CONNECTION_UPDATE: u16 = 0x4E04;
/// `StopBluetoothConnectionUpdates` (sent by the accessory).
pub const ID_STOP_BLUETOOTH_CONNECTION_UPDATES: u16 = 0x4E05;
/// `DeviceInformationUpdate` (sent by the device).
pub const ID_DEVICE_INFORMATION_UPDATE: u16 = 0x4E09;
/// `DeviceLanguageUpdate` (sent by the device).
pub const ID_DEVICE_LANGUAGE_UPDATE: u16 = 0x4E0A;
/// `DeviceTimeUpdate` (sent by the device).
pub const ID_DEVICE_TIME_UPDATE: u16 = 0x4E0B;
/// `DeviceUUIDUpdate` (sent by the device).
pub const ID_DEVICE_UUID_UPDATE: u16 = 0x4E0C;
/// `WirelessCarPlayUpdate` (sent by the device).
pub const ID_WIRELESS_CAR_PLAY_UPDATE: u16 = 0x4E0D;
/// `DeviceTransportIdentifierNotification` (sent by the device).
pub const ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION: u16 = 0x4E0E;
/// `StartNowPlayingUpdates` (sent by the accessory).
pub const ID_START_NOW_PLAYING_UPDATES: u16 = 0x5000;
/// `NowPlayingUpdate` (sent by the device).
pub const ID_NOW_PLAYING_UPDATE: u16 = 0x5001;
/// `StopNowPlayingUpdates` (sent by the accessory).
pub const ID_STOP_NOW_PLAYING_UPDATES: u16 = 0x5002;
/// `SetNowPlayingInformation` (sent by the accessory).
pub const ID_SET_NOW_PLAYING_INFORMATION: u16 = 0x5003;
/// `StartRouteGuidanceUpdates` (sent by the accessory).
pub const ID_START_ROUTE_GUIDANCE_UPDATES: u16 = 0x5200;
/// `RouteGuidanceUpdate` (sent by the device).
pub const ID_ROUTE_GUIDANCE_UPDATE: u16 = 0x5201;
/// `RouteGuidanceManeuverUpdate` (sent by the device).
pub const ID_ROUTE_GUIDANCE_MANEUVER_UPDATE: u16 = 0x5202;
/// `StopRouteGuidanceUpdates` (sent by the accessory).
pub const ID_STOP_ROUTE_GUIDANCE_UPDATES: u16 = 0x5203;
/// `StartAssistiveTouch` (sent by the accessory).
pub const ID_START_ASSISTIVE_TOUCH: u16 = 0x5400;
/// `StopAssistiveTouch` (sent by the accessory).
pub const ID_STOP_ASSISTIVE_TOUCH: u16 = 0x5401;
/// `StartAssistiveTouchInformation` (sent by the accessory).
pub const ID_START_ASSISTIVE_TOUCH_INFORMATION: u16 = 0x5402;
/// `AssistiveTouchInformation` (sent by the device).
pub const ID_ASSISTIVE_TOUCH_INFORMATION: u16 = 0x5403;
/// `StopAssistiveTouchInformation` (sent by the accessory).
pub const ID_STOP_ASSISTIVE_TOUCH_INFORMATION: u16 = 0x5404;
/// `RequestVoiceOverMoveCursor` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_MOVE_CURSOR: u16 = 0x5601;
/// `RequestVoiceOverActivateCursor` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_ACTIVATE_CURSOR: u16 = 0x5602;
/// `RequestVoiceOverScrollPage` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_SCROLL_PAGE: u16 = 0x5603;
/// `RequestVoiceOverSpeakText` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_SPEAK_TEXT: u16 = 0x5606;
/// `RequestVoiceOverPauseText` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_PAUSE_TEXT: u16 = 0x5608;
/// `RequestVoiceOverResumeText` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_RESUME_TEXT: u16 = 0x5609;
/// `StartVoiceOverUpdates` (sent by the accessory).
pub const ID_START_VOICE_OVER_UPDATES: u16 = 0x560B;
/// `VoiceOverUpdate` (sent by the device).
pub const ID_VOICE_OVER_UPDATE: u16 = 0x560C;
/// `StopVoiceOverUpdates` (sent by the accessory).
pub const ID_STOP_VOICE_OVER_UPDATES: u16 = 0x560D;
/// `RequestVoiceOverConfiguration` (sent by the accessory).
pub const ID_REQUEST_VOICE_OVER_CONFIGURATION: u16 = 0x560E;
/// `StartVoiceOverCursorUpdates` (sent by the accessory).
pub const ID_START_VOICE_OVER_CURSOR_UPDATES: u16 = 0x560F;
/// `VoiceOverCursorUpdate` (sent by the device).
pub const ID_VOICE_OVER_CURSOR_UPDATE: u16 = 0x5610;
/// `StopVoiceOverCursorUpdates` (sent by the accessory).
pub const ID_STOP_VOICE_OVER_CURSOR_UPDATES: u16 = 0x5611;
/// `StartVoiceOver` (sent by the accessory).
pub const ID_START_VOICE_OVER: u16 = 0x5612;
/// `StopVoiceOver` (sent by the accessory).
pub const ID_STOP_VOICE_OVER: u16 = 0x5613;
/// `RequestWiFiInformation` (sent by the accessory).
pub const ID_REQUEST_WI_FI_INFORMATION: u16 = 0x5700;
/// `WiFiInformation` (sent by the device).
pub const ID_WI_FI_INFORMATION: u16 = 0x5701;
/// `RequestAccessoryWiFiConfigurationInformation` (sent by the device).
pub const ID_REQUEST_ACCESSORY_WI_FI_CONFIGURATION_INFORMATION: u16 = 0x5702;
/// `AccessoryWiFiConfigurationInformation` (sent by the accessory).
pub const ID_ACCESSORY_WI_FI_CONFIGURATION_INFORMATION: u16 = 0x5703;
/// `StartHID` (sent by the accessory).
pub const ID_START_HID: u16 = 0x6800;
/// `DeviceHIDReport` (sent by the device).
pub const ID_DEVICE_HID_REPORT: u16 = 0x6801;
/// `AccessoryHIDReport` (sent by the accessory).
pub const ID_ACCESSORY_HID_REPORT: u16 = 0x6802;
/// `StopHID` (sent by the accessory).
pub const ID_STOP_HID: u16 = 0x6803;
/// `StartNativeHID` (sent by the device).
pub const ID_START_NATIVE_HID: u16 = 0x6806;
/// `StartVehicleStatusUpdates` (sent by the device).
pub const ID_START_VEHICLE_STATUS_UPDATES: u16 = 0xA100;
/// `VehicleStatusUpdate` (sent by the accessory).
pub const ID_VEHICLE_STATUS_UPDATE: u16 = 0xA101;
/// `StopVehicleStatusUpdates` (sent by the device).
pub const ID_STOP_VEHICLE_STATUS_UPDATES: u16 = 0xA102;
/// `RequestAuthenticationCertificate` (sent by the device).
pub const ID_REQUEST_AUTHENTICATION_CERTIFICATE: u16 = 0xAA00;
/// `AuthenticationCertificate` (sent by the accessory).
pub const ID_AUTHENTICATION_CERTIFICATE: u16 = 0xAA01;
/// `RequestAuthenticationChallengeResponse` (sent by the device).
pub const ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE: u16 = 0xAA02;
/// `AuthenticationResponse` (sent by the accessory).
pub const ID_AUTHENTICATION_RESPONSE: u16 = 0xAA03;
/// `AuthenticationFailed` (sent by the device).
pub const ID_AUTHENTICATION_FAILED: u16 = 0xAA04;
/// `AuthenticationSucceeded` (sent by the device).
pub const ID_AUTHENTICATION_SUCCEEDED: u16 = 0xAA05;
/// `AccessoryAuthenticationSerialNumber` (sent by the accessory).
pub const ID_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER: u16 = 0xAA06;
/// `StartPowerUpdates` (sent by the accessory).
pub const ID_START_POWER_UPDATES: u16 = 0xAE00;
/// `PowerUpdate` (sent by the device).
pub const ID_POWER_UPDATE: u16 = 0xAE01;
/// `StopPowerUpdates` (sent by the accessory).
pub const ID_STOP_POWER_UPDATES: u16 = 0xAE02;
/// `PowerSourceUpdate` (sent by the accessory).
pub const ID_POWER_SOURCE_UPDATE: u16 = 0xAE03;
/// `StartUSBDeviceModeAudio` (sent by the accessory).
pub const ID_START_USB_DEVICE_MODE_AUDIO: u16 = 0xDA00;
/// `USBDeviceModeAudioInformation` (sent by the device).
pub const ID_USB_DEVICE_MODE_AUDIO_INFORMATION: u16 = 0xDA01;
/// `StopUSBDeviceModeAudio` (sent by the accessory).
pub const ID_STOP_USB_DEVICE_MODE_AUDIO: u16 = 0xDA02;
/// `StartExternalAccessoryProtocolSession` (sent by the device).
pub const ID_START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION: u16 = 0xEA00;
/// `StopExternalAccessoryProtocolSession` (sent by the device).
pub const ID_STOP_EXTERNAL_ACCESSORY_PROTOCOL_SESSION: u16 = 0xEA01;
/// `RequestAppLaunch` (sent by the accessory).
pub const ID_REQUEST_APP_LAUNCH: u16 = 0xEA02;
/// `StatusExternalAccessoryProtocolSession` (sent by the accessory).
pub const ID_STATUS_EXTERNAL_ACCESSORY_PROTOCOL_SESSION: u16 = 0xEA03;
/// `GPRMCDataStatusValuesNotification` (sent by the device).
pub const ID_GPRMC_DATA_STATUS_VALUES_NOTIFICATION: u16 = 0xFFF0;
/// `StartLocationInformation` (sent by the device).
pub const ID_START_LOCATION_INFORMATION: u16 = 0xFFFA;
/// `LocationInformation` (sent by the accessory).
pub const ID_LOCATION_INFORMATION: u16 = 0xFFFB;
/// `StopLocationInformation` (sent by the device).
pub const ID_STOP_LOCATION_INFORMATION: u16 = 0xFFFC;

/// Every §97 control-session message: identifier, spec name, and originating side.
pub const MESSAGES: [(u16, &str, Source); 114] = [
	(ID_START_OOBBT_PAIRING, "StartOOBBTPairing", Source::Device),
	(
		ID_OOBBT_PAIRING_ACCESSORY_INFORMATION,
		"OOBBTPairingAccessoryInformation",
		Source::Accessory,
	),
	(
		ID_OOBBT_PAIRING_LINK_KEY_INFORMATION,
		"OOBBTPairingLinkKeyInformation",
		Source::Device,
	),
	(
		ID_OOBBT_PAIRING_COMPLETION_INFORMATION,
		"OOBBTPairingCompletionInformation",
		Source::Accessory,
	),
	(ID_STOP_OOBBT_PAIRING, "StopOOBBTPairing", Source::Device),
	(
		ID_START_DESTINATION_INFORMATION,
		"StartDestinationInformation",
		Source::Accessory,
	),
	(
		ID_DESTINATION_INFORMATION,
		"DestinationInformation",
		Source::Device,
	),
	(
		ID_STOP_DESTINATION_INFORMATION,
		"StopDestinationInformation",
		Source::Accessory,
	),
	(
		ID_DESTINATION_INFORMATION_STATUS,
		"DestinationInformationStatus",
		Source::Accessory,
	),
	(
		ID_START_IDENTIFICATION,
		"StartIdentification",
		Source::Device,
	),
	(
		ID_IDENTIFICATION_INFORMATION,
		"IdentificationInformation",
		Source::Accessory,
	),
	(
		ID_IDENTIFICATION_ACCEPTED,
		"IdentificationAccepted",
		Source::Device,
	),
	(
		ID_IDENTIFICATION_REJECTED,
		"IdentificationRejected",
		Source::Device,
	),
	(
		ID_CANCEL_IDENTIFICATION,
		"CancelIdentification",
		Source::Accessory,
	),
	(
		ID_IDENTIFICATION_INFORMATION_UPDATE,
		"IdentificationInformationUpdate",
		Source::Accessory,
	),
	(
		ID_START_CALL_STATE_UPDATES,
		"StartCallStateUpdates",
		Source::Accessory,
	),
	(ID_CALL_STATE_UPDATE, "CallStateUpdate", Source::Device),
	(
		ID_STOP_CALL_STATE_UPDATES,
		"StopCallStateUpdates",
		Source::Accessory,
	),
	(
		ID_START_COMMUNICATIONS_UPDATES,
		"StartCommunicationsUpdates",
		Source::Accessory,
	),
	(
		ID_COMMUNICATIONS_UPDATE,
		"CommunicationsUpdate",
		Source::Device,
	),
	(
		ID_STOP_COMMUNICATIONS_UPDATES,
		"StopCommunicationsUpdates",
		Source::Accessory,
	),
	(ID_INITIATE_CALL, "InitiateCall", Source::Accessory),
	(ID_ACCEPT_CALL, "AcceptCall", Source::Accessory),
	(ID_END_CALL, "EndCall", Source::Accessory),
	(ID_SWAP_CALLS, "SwapCalls", Source::Accessory),
	(ID_MERGE_CALLS, "MergeCalls", Source::Accessory),
	(ID_HOLD_STATUS_UPDATE, "HoldStatusUpdate", Source::Accessory),
	(ID_MUTE_STATUS_UPDATE, "MuteStatusUpdate", Source::Accessory),
	(ID_SEND_DTMF, "SendDTMF", Source::Accessory),
	(ID_START_LIST_UPDATES, "StartListUpdates", Source::Accessory),
	(ID_LIST_UPDATE, "ListUpdate", Source::Device),
	(ID_STOP_LIST_UPDATES, "StopListUpdates", Source::Accessory),
	(
		ID_START_MEDIA_LIBRARY_INFORMATION,
		"StartMediaLibraryInformation",
		Source::Accessory,
	),
	(
		ID_MEDIA_LIBRARY_INFORMATION,
		"MediaLibraryInformation",
		Source::Device,
	),
	(
		ID_STOP_MEDIA_LIBRARY_INFORMATION,
		"StopMediaLibraryInformation",
		Source::Accessory,
	),
	(
		ID_START_MEDIA_LIBRARY_UPDATES,
		"StartMediaLibraryUpdates",
		Source::Accessory,
	),
	(
		ID_MEDIA_LIBRARY_UPDATE,
		"MediaLibraryUpdate",
		Source::Device,
	),
	(
		ID_STOP_MEDIA_LIBRARY_UPDATES,
		"StopMediaLibraryUpdates",
		Source::Accessory,
	),
	(
		ID_PLAY_MEDIA_LIBRARY_CURRENT_SELECTION,
		"PlayMediaLibraryCurrentSelection",
		Source::Accessory,
	),
	(
		ID_PLAY_MEDIA_LIBRARY_ITEMS,
		"PlayMediaLibraryItems",
		Source::Accessory,
	),
	(
		ID_PLAY_MEDIA_LIBRARY_COLLECTION,
		"PlayMediaLibraryCollection",
		Source::Accessory,
	),
	(
		ID_PLAY_MEDIA_LIBRARY_SPECIAL,
		"PlayMediaLibrarySpecial",
		Source::Accessory,
	),
	(
		ID_BLUETOOTH_COMPONENT_INFORMATION,
		"BluetoothComponentInformation",
		Source::Accessory,
	),
	(
		ID_START_BLUETOOTH_CONNECTION_UPDATES,
		"StartBluetoothConnectionUpdates",
		Source::Accessory,
	),
	(
		ID_BLUETOOTH_CONNECTION_UPDATE,
		"BluetoothConnectionUpdate",
		Source::Device,
	),
	(
		ID_STOP_BLUETOOTH_CONNECTION_UPDATES,
		"StopBluetoothConnectionUpdates",
		Source::Accessory,
	),
	(
		ID_DEVICE_INFORMATION_UPDATE,
		"DeviceInformationUpdate",
		Source::Device,
	),
	(
		ID_DEVICE_LANGUAGE_UPDATE,
		"DeviceLanguageUpdate",
		Source::Device,
	),
	(ID_DEVICE_TIME_UPDATE, "DeviceTimeUpdate", Source::Device),
	(ID_DEVICE_UUID_UPDATE, "DeviceUUIDUpdate", Source::Device),
	(
		ID_WIRELESS_CAR_PLAY_UPDATE,
		"WirelessCarPlayUpdate",
		Source::Device,
	),
	(
		ID_DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
		"DeviceTransportIdentifierNotification",
		Source::Device,
	),
	(
		ID_START_NOW_PLAYING_UPDATES,
		"StartNowPlayingUpdates",
		Source::Accessory,
	),
	(ID_NOW_PLAYING_UPDATE, "NowPlayingUpdate", Source::Device),
	(
		ID_STOP_NOW_PLAYING_UPDATES,
		"StopNowPlayingUpdates",
		Source::Accessory,
	),
	(
		ID_SET_NOW_PLAYING_INFORMATION,
		"SetNowPlayingInformation",
		Source::Accessory,
	),
	(
		ID_START_ROUTE_GUIDANCE_UPDATES,
		"StartRouteGuidanceUpdates",
		Source::Accessory,
	),
	(
		ID_ROUTE_GUIDANCE_UPDATE,
		"RouteGuidanceUpdate",
		Source::Device,
	),
	(
		ID_ROUTE_GUIDANCE_MANEUVER_UPDATE,
		"RouteGuidanceManeuverUpdate",
		Source::Device,
	),
	(
		ID_STOP_ROUTE_GUIDANCE_UPDATES,
		"StopRouteGuidanceUpdates",
		Source::Accessory,
	),
	(
		ID_START_ASSISTIVE_TOUCH,
		"StartAssistiveTouch",
		Source::Accessory,
	),
	(
		ID_STOP_ASSISTIVE_TOUCH,
		"StopAssistiveTouch",
		Source::Accessory,
	),
	(
		ID_START_ASSISTIVE_TOUCH_INFORMATION,
		"StartAssistiveTouchInformation",
		Source::Accessory,
	),
	(
		ID_ASSISTIVE_TOUCH_INFORMATION,
		"AssistiveTouchInformation",
		Source::Device,
	),
	(
		ID_STOP_ASSISTIVE_TOUCH_INFORMATION,
		"StopAssistiveTouchInformation",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_MOVE_CURSOR,
		"RequestVoiceOverMoveCursor",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_ACTIVATE_CURSOR,
		"RequestVoiceOverActivateCursor",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_SCROLL_PAGE,
		"RequestVoiceOverScrollPage",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_SPEAK_TEXT,
		"RequestVoiceOverSpeakText",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_PAUSE_TEXT,
		"RequestVoiceOverPauseText",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_RESUME_TEXT,
		"RequestVoiceOverResumeText",
		Source::Accessory,
	),
	(
		ID_START_VOICE_OVER_UPDATES,
		"StartVoiceOverUpdates",
		Source::Accessory,
	),
	(ID_VOICE_OVER_UPDATE, "VoiceOverUpdate", Source::Device),
	(
		ID_STOP_VOICE_OVER_UPDATES,
		"StopVoiceOverUpdates",
		Source::Accessory,
	),
	(
		ID_REQUEST_VOICE_OVER_CONFIGURATION,
		"RequestVoiceOverConfiguration",
		Source::Accessory,
	),
	(
		ID_START_VOICE_OVER_CURSOR_UPDATES,
		"StartVoiceOverCursorUpdates",
		Source::Accessory,
	),
	(
		ID_VOICE_OVER_CURSOR_UPDATE,
		"VoiceOverCursorUpdate",
		Source::Device,
	),
	(
		ID_STOP_VOICE_OVER_CURSOR_UPDATES,
		"StopVoiceOverCursorUpdates",
		Source::Accessory,
	),
	(ID_START_VOICE_OVER, "StartVoiceOver", Source::Accessory),
	(ID_STOP_VOICE_OVER, "StopVoiceOver", Source::Accessory),
	(
		ID_REQUEST_WI_FI_INFORMATION,
		"RequestWiFiInformation",
		Source::Accessory,
	),
	(ID_WI_FI_INFORMATION, "WiFiInformation", Source::Device),
	(
		ID_REQUEST_ACCESSORY_WI_FI_CONFIGURATION_INFORMATION,
		"RequestAccessoryWiFiConfigurationInformation",
		Source::Device,
	),
	(
		ID_ACCESSORY_WI_FI_CONFIGURATION_INFORMATION,
		"AccessoryWiFiConfigurationInformation",
		Source::Accessory,
	),
	(ID_START_HID, "StartHID", Source::Accessory),
	(ID_DEVICE_HID_REPORT, "DeviceHIDReport", Source::Device),
	(
		ID_ACCESSORY_HID_REPORT,
		"AccessoryHIDReport",
		Source::Accessory,
	),
	(ID_STOP_HID, "StopHID", Source::Accessory),
	(ID_START_NATIVE_HID, "StartNativeHID", Source::Device),
	(
		ID_START_VEHICLE_STATUS_UPDATES,
		"StartVehicleStatusUpdates",
		Source::Device,
	),
	(
		ID_VEHICLE_STATUS_UPDATE,
		"VehicleStatusUpdate",
		Source::Accessory,
	),
	(
		ID_STOP_VEHICLE_STATUS_UPDATES,
		"StopVehicleStatusUpdates",
		Source::Device,
	),
	(
		ID_REQUEST_AUTHENTICATION_CERTIFICATE,
		"RequestAuthenticationCertificate",
		Source::Device,
	),
	(
		ID_AUTHENTICATION_CERTIFICATE,
		"AuthenticationCertificate",
		Source::Accessory,
	),
	(
		ID_REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
		"RequestAuthenticationChallengeResponse",
		Source::Device,
	),
	(
		ID_AUTHENTICATION_RESPONSE,
		"AuthenticationResponse",
		Source::Accessory,
	),
	(
		ID_AUTHENTICATION_FAILED,
		"AuthenticationFailed",
		Source::Device,
	),
	(
		ID_AUTHENTICATION_SUCCEEDED,
		"AuthenticationSucceeded",
		Source::Device,
	),
	(
		ID_ACCESSORY_AUTHENTICATION_SERIAL_NUMBER,
		"AccessoryAuthenticationSerialNumber",
		Source::Accessory,
	),
	(
		ID_START_POWER_UPDATES,
		"StartPowerUpdates",
		Source::Accessory,
	),
	(ID_POWER_UPDATE, "PowerUpdate", Source::Device),
	(ID_STOP_POWER_UPDATES, "StopPowerUpdates", Source::Accessory),
	(
		ID_POWER_SOURCE_UPDATE,
		"PowerSourceUpdate",
		Source::Accessory,
	),
	(
		ID_START_USB_DEVICE_MODE_AUDIO,
		"StartUSBDeviceModeAudio",
		Source::Accessory,
	),
	(
		ID_USB_DEVICE_MODE_AUDIO_INFORMATION,
		"USBDeviceModeAudioInformation",
		Source::Device,
	),
	(
		ID_STOP_USB_DEVICE_MODE_AUDIO,
		"StopUSBDeviceModeAudio",
		Source::Accessory,
	),
	(
		ID_START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION,
		"StartExternalAccessoryProtocolSession",
		Source::Device,
	),
	(
		ID_STOP_EXTERNAL_ACCESSORY_PROTOCOL_SESSION,
		"StopExternalAccessoryProtocolSession",
		Source::Device,
	),
	(ID_REQUEST_APP_LAUNCH, "RequestAppLaunch", Source::Accessory),
	(
		ID_STATUS_EXTERNAL_ACCESSORY_PROTOCOL_SESSION,
		"StatusExternalAccessoryProtocolSession",
		Source::Accessory,
	),
	(
		ID_GPRMC_DATA_STATUS_VALUES_NOTIFICATION,
		"GPRMCDataStatusValuesNotification",
		Source::Device,
	),
	(
		ID_START_LOCATION_INFORMATION,
		"StartLocationInformation",
		Source::Device,
	),
	(
		ID_LOCATION_INFORMATION,
		"LocationInformation",
		Source::Accessory,
	),
	(
		ID_STOP_LOCATION_INFORMATION,
		"StopLocationInformation",
		Source::Device,
	),
];

/// The spec name of a message identifier, for logging.
pub fn name(id: u16) -> Option<&'static str> {
	MESSAGES
		.iter()
		.find(|(i, _, _)| *i == id)
		.map(|(_, n, _)| *n)
}

/// Which side is allowed to send a message identifier.
pub fn source(id: u16) -> Option<Source> {
	MESSAGES
		.iter()
		.find(|(i, _, _)| *i == id)
		.map(|(_, _, s)| *s)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn table_is_sorted_and_unique() {
		for w in MESSAGES.windows(2) {
			assert!(w[0].0 < w[1].0, "{:#06x} !< {:#06x}", w[0].0, w[1].0);
		}
	}

	#[test]
	fn spot_checks_against_the_spec() {
		// §97.1.1 / §97.1.2: authentication is device-driven, accessory-answered.
		assert_eq!(
			source(ID_REQUEST_AUTHENTICATION_CERTIFICATE),
			Some(Source::Device)
		);
		assert_eq!(
			source(ID_AUTHENTICATION_CERTIFICATE),
			Some(Source::Accessory)
		);
		// §97.9.1: Source is Device despite "Accessory" appearing in the name.
		assert_eq!(ID_START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION, 0xEA00);
		assert_eq!(
			source(ID_START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION),
			Some(Source::Device)
		);
		// §97.15.1 / §97.15.2: the accessory asks, the device answers.
		assert_eq!(source(ID_START_POWER_UPDATES), Some(Source::Accessory));
		assert_eq!(source(ID_POWER_UPDATE), Some(Source::Device));
		assert_eq!(
			name(ID_IDENTIFICATION_INFORMATION),
			Some("IdentificationInformation")
		);
		assert_eq!(name(0x0000), None);
	}
}
