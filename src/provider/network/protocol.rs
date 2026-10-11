use serde::{Deserialize, Serialize};


/// sent by carplayd when first connecting to socket
#[derive(Serialize, Deserialize, Debug)]
pub struct InitMessage {
	pub display_width: u32,
	pub display_height: u32,
	pub display_fps: u32,
}

/// sent by carplayd when requesting a screen stream
/// response: connect to the port
#[derive(Serialize, Deserialize, Debug)]
pub struct DisplayRequestMessage {
	pub port: u16,
}
