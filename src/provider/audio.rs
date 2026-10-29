//! Provider-side PCM. Client formats and clocks are independent of CarPlay's wire format.
use std::{sync::Arc, time::Instant};

use anyhow::{Result, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SampleType {
	S16Le,
	F32Le,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PcmFormat {
	pub sample_rate: u32,
	pub channels: u8,
	pub sample_type: SampleType,
}
impl PcmFormat {
	pub fn s16(sample_rate: u32, channels: u8) -> Self {
		Self {
			sample_rate,
			channels,
			sample_type: SampleType::S16Le,
		}
	}
	pub fn validate(self) -> Result<()> {
		ensure!(
			(8_000..=96_000).contains(&self.sample_rate),
			"PCM sample rate must be 8000–96000 Hz"
		);
		ensure!(
			(1..=2).contains(&self.channels),
			"PCM must be mono or stereo"
		);
		Ok(())
	}
	pub fn frame_bytes(self) -> usize {
		self.channels as usize
			* match self.sample_type {
				SampleType::S16Le => 2,
				SampleType::F32Le => 4,
			}
	}
	pub fn frames(self, data: &[u8]) -> Result<usize> {
		ensure!(
			data.len().is_multiple_of(self.frame_bytes()),
			"partial PCM frame"
		);
		Ok(data.len() / self.frame_bytes())
	}
	pub fn block_frames(self) -> usize {
		self.sample_rate.div_ceil(200) as usize
	}
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioClockMode {
	#[default]
	Live,
	DemandDriven,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AudioKind {
	Main,
	Alternate,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AudioUse {
	Media,
	Alert,
	Telephony,
	SpeechRecognition,
	Default,
}
impl AudioUse {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Media => "media",
			Self::Alert => "alert",
			Self::Telephony => "telephony",
			Self::SpeechRecognition => "speechRecognition",
			Self::Default => "default",
		}
	}
	pub fn temporary(self) -> bool {
		!matches!(self, Self::Media)
	}
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AudioRequest {
	pub id: u64,
	pub kind: AudioKind,
	pub audio_type: AudioUse,
	pub playback: PcmFormat,
	#[serde(default)]
	pub microphone: Option<PcmFormat>,
	#[serde(default)]
	pub playback_clock: AudioClockMode,
	#[serde(default)]
	pub microphone_clock: AudioClockMode,
}
impl AudioRequest {
	pub fn validate(&self) -> Result<()> {
		self.playback.validate()?;
		if let Some(format) = self.microphone {
			format.validate()?;
		}
		ensure!(
			self.kind == AudioKind::Main || self.microphone.is_none(),
			"alternate audio cannot enable microphone input"
		);
		Ok(())
	}
}

#[derive(Clone, Debug)]
pub struct AudioChunk {
	/// Frame position in this endpoint's format, independent of RTP timestamps.
	pub position: u64,
	pub data: Bytes,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioProgress {
	pub consumed: u64,
	pub received: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioState {
	Active,
	Suspended,
	Stopped,
	Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioStatus {
	pub id: u64,
	pub state: AudioState,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub wire_format: Option<PcmFormat>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub detail: Option<String>,
}

#[async_trait]
pub trait CarplayProviderAudioOutput: Send {
	/// Cancel safe. Blocks are at most 20 ms; EOF drains before releasing ownership.
	async fn read(&mut self) -> Result<Option<AudioChunk>>;
	fn discard_buffered(&mut self) {}
}

#[async_trait]
pub trait CarplayProviderAudioInput: Send {
	async fn write(&mut self, chunk: AudioChunk) -> Result<()>;
	fn progress(&self) -> Option<AudioProgress>;
	fn next_position(&self) -> u64 {
		0
	}
}

#[derive(Clone)]
pub struct AudioEndpoints {
	pub output: Arc<Mutex<Box<dyn CarplayProviderAudioOutput>>>,
	pub input: Option<Arc<Mutex<Box<dyn CarplayProviderAudioInput>>>>,
}

pub enum AudioEvent {
	Start(AudioRequest, AudioEndpoints),
	Stop(u64),
	Flush(u64),
	Duck {
		id: u64,
		duration_ms: u32,
		volume: f64,
	},
	Unduck {
		id: u64,
		duration_ms: u32,
	},
	/// Provider application states are explicit, rather than guessed from metadata.
	AppState {
		speech: AudioSpeech,
		phone_call: bool,
		turn_by_turn: bool,
	},
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSpeech {
	#[default]
	None,
	Speaking,
	Listening,
}

#[async_trait]
pub trait CarplayProviderAudio: Send {
	/// Cancel safe; a closed provider ends its audio streams.
	async fn next_event(&mut self) -> Result<Option<AudioEvent>>;
	async fn status(&mut self, status: AudioStatus) -> Result<()>;
}
