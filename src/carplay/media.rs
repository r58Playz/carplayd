use serde::{Deserialize, Serialize};

use crate::iap2::msg::Param;

macro_rules! encode_optional_diff {
	($params:ident, $current:ident, $previous:ident, $value:ident;
		$($field:ident => $encoded:expr),* $(,)?) => {
		$(
			if $current.$field != $previous.$field
				&& let Some($value) = &$current.$field
			{
				$params.push($encoded);
			}
		)*
	};
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum MediaType {
	Music = 0,
	Podcast = 1,
	AudioBook = 2,
}

/// corresponds to MediaItemIsLikeSupported + MediaItemIsLiked
/// if not specified: both params not specified
/// if `Unsupported`: MediaItemIsLikeSupported specified as false, MediaItemIsLiked not specified
/// if `No`/`Yes`: MediaItemIsLikeSupported specified as true, MediaItemIsLiked specified as
/// `false`/`true` respectively
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
enum MediaItemLiked {
	Unsupported,
	No,
	Yes,
}

/// corresponds to MediaItemIsBanSupported + MediaItemIsBanned
/// if not specified: both params not specified
/// if `Unsupported`: MediaItemIsBanSupported specified as false, MediaItemIsBanned not specified
/// if `No`/`Yes`: MediaItemIsBanSupported specified as true, MediaItemIsBanned specified as
/// `false`/`true` respectively
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
enum MediaItemBanned {
	Unsupported,
	No,
	Yes,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MediaItem {
	persistent_identifier: Option<u64>,
	title: Option<String>,
	media_type: Vec<MediaType>,
	/// 0 to 5
	rating: Option<u8>,
	/// milliseconds
	playback_duration: Option<u32>,

	album_persistent_identifier: Option<u64>,
	album_title: Option<String>,
	album_track_number: Option<u16>,
	album_track_count: Option<u16>,
	album_disc_number: Option<u16>,
	album_disc_count: Option<u16>,

	artist_persistent_identifier: Option<u64>,
	artist: Option<String>,
	album_artist_persistent_identifier: Option<u64>,
	album_artist: Option<String>,
	genre_persistent_identifier: Option<u64>,
	genre: Option<String>,
	composer_persistent_identifier: Option<u64>,
	composer: Option<String>,

	is_part_of_compilation: Option<bool>,
	liked: Option<MediaItemLiked>,
	banned: Option<MediaItemBanned>,
	is_resident_on_device: Option<bool>,
	//artwork: Option<Bytes>,
	chapter_count: Option<u16>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NowPlayingMediaItem {
	persistent_identifier: Option<u64>,
	title: Option<String>,
	/// milliseconds
	playback_duration: Option<u32>,

	album_title: Option<String>,
	album_track_number: Option<u16>,
	album_track_count: Option<u16>,
	album_disc_number: Option<u16>,
	album_disc_count: Option<u16>,

	artist: Option<String>,
	genre: Option<String>,
	composer: Option<String>,

	liked: Option<MediaItemLiked>,
	banned: Option<MediaItemBanned>,
	//artwork: Option<Bytes>,
	chapter_count: Option<u16>,
}

impl NowPlayingMediaItem {
	pub fn encode_diff(&self, previous: &Self) -> Option<Param> {
		let mut params = Vec::new();
		// R29 Table 97-117, restricted to the fields in Table 97-127.
		encode_optional_diff!(params, self, previous, value;
			persistent_identifier => Param::u64_(0, *value),
			title => Param::utf8(1, value),
			playback_duration => Param::u32_(4, *value),
			album_title => Param::utf8(6, value),
			album_track_number => Param::u16_(7, *value),
			album_track_count => Param::u16_(8, *value),
			album_disc_number => Param::u16_(9, *value),
			album_disc_count => Param::u16_(10, *value),
			artist => Param::utf8(12, value),
			genre => Param::utf8(16, value),
			composer => Param::utf8(18, value),
		);
		if self.liked != previous.liked
			&& let Some(value) = &self.liked
		{
			params.push(Param::bool(21, *value != MediaItemLiked::Unsupported));
			if *value != MediaItemLiked::Unsupported {
				params.push(Param::bool(23, *value == MediaItemLiked::Yes));
			}
		}
		if self.banned != previous.banned
			&& let Some(value) = &self.banned
		{
			params.push(Param::bool(22, *value != MediaItemBanned::Unsupported));
			if *value != MediaItemBanned::Unsupported {
				params.push(Param::bool(24, *value == MediaItemBanned::Yes));
			}
		}
		encode_optional_diff!(params, self, previous, value;
			chapter_count => Param::u16_(27, *value),
		);
		if params.is_empty() {
			None
		} else {
			Some(Param::group(0, &params))
		}
	}
}

impl From<MediaItem> for NowPlayingMediaItem {
	fn from(value: MediaItem) -> Self {
		let MediaItem {
			persistent_identifier,
			title,
			playback_duration,
			album_title,
			album_track_number,
			album_track_count,
			album_disc_number,
			album_disc_count,
			artist,
			genre,
			composer,
			liked,
			banned,
			//	artwork,
			chapter_count,
			..
		} = value;
		Self {
			persistent_identifier,
			title,
			playback_duration,
			album_title,
			album_track_number,
			album_track_count,
			album_disc_number,
			album_disc_count,
			artist,
			genre,
			composer,
			liked,
			banned,
			//	artwork,
			chapter_count,
		}
	}
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStatus {
	Stopped = 0,
	Playing = 1,
	Paused = 2,
	SeekForward = 3,
	SeekBackward = 4,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackShuffle {
	Off = 0,
	Songs = 1,
	Albums = 2,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackRepeat {
	Off = 0,
	One = 1,
	All = 2,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NowPlaying {
	media_item: NowPlayingMediaItem,

	status: Option<PlaybackStatus>,
	/// milliseconds
	elapsed_time: Option<u32>,
	queue_index: Option<u32>,
	/// value of `0` while status is not `PlaybackStatus::Stopped` = "endless" playback
	queue_count: Option<u32>,
	queue_chapter_index: Option<u32>,

	shuffle_mode: Option<PlaybackShuffle>,
	repeat_mode: Option<PlaybackRepeat>,

	app_name: Option<String>,

	media_library_unique_identifier: Option<String>,
	apple_music_radio_ad: Option<bool>,
	apple_music_radio_station_name: Option<String>,
	apple_music_radio_station_media_playlist_id: Option<u64>,

	playback_speed: Option<u16>,

	set_elapsed_time_allowed: Option<bool>,
}

impl NowPlaying {
	/// Explicitly clear the accessory's cached state when the media source disappears.
	/// `None` fields are unspecified on the wire, so a default snapshot cannot clear it.
	pub fn unavailable() -> Self {
		Self {
			media_item: NowPlayingMediaItem {
				persistent_identifier: Some(0),
				title: Some(String::new()),
				playback_duration: Some(0),
				album_title: Some(String::new()),
				album_track_number: Some(0),
				album_track_count: Some(0),
				album_disc_number: Some(0),
				album_disc_count: Some(0),
				artist: Some(String::new()),
				genre: Some(String::new()),
				composer: Some(String::new()),
				liked: Some(MediaItemLiked::Unsupported),
				banned: Some(MediaItemBanned::Unsupported),
				chapter_count: Some(0),
			},
			status: Some(PlaybackStatus::Stopped),
			elapsed_time: Some(0),
			queue_index: Some(0),
			queue_count: Some(0),
			queue_chapter_index: Some(0),
			shuffle_mode: Some(PlaybackShuffle::Off),
			repeat_mode: Some(PlaybackRepeat::Off),
			app_name: Some(String::new()),
			media_library_unique_identifier: Some(String::new()),
			apple_music_radio_ad: Some(false),
			apple_music_radio_station_name: Some(String::new()),
			apple_music_radio_station_media_playlist_id: Some(0),
			playback_speed: Some(100),
			set_elapsed_time_allowed: Some(false),
		}
	}

	pub fn encode_diff(&self, previous: &Self) -> Vec<Param> {
		let mut groups = Vec::new();
		if let Some(media_item) = self.media_item.encode_diff(&previous.media_item) {
			groups.push(media_item);
		}

		let mut playback = Vec::new();
		// R29 Table 97-131.
		encode_optional_diff!(playback, self, previous, value;
			status => Param::u8_(0, *value as u8),
			elapsed_time => Param::u32_(1, *value),
			queue_index => Param::u32_(2, *value),
			queue_count => Param::u32_(3, *value),
			queue_chapter_index => Param::u32_(4, *value),
			shuffle_mode => Param::u8_(5, *value as u8),
			repeat_mode => Param::u8_(6, *value as u8),
			app_name => Param::utf8(7, value),
			media_library_unique_identifier => Param::utf8(8, value),
			apple_music_radio_ad => Param::bool(9, *value),
			apple_music_radio_station_name => Param::utf8(10, value),
			apple_music_radio_station_media_playlist_id => Param::u64_(11, *value),
			playback_speed => Param::u16_(12, *value),
			set_elapsed_time_allowed => Param::bool(13, *value),
		);
		if !playback.is_empty() {
			groups.push(Param::group(1, &playback));
		}
		groups
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::iap2::msg::{Message, ids};

	#[test]
	fn initial_update_preserves_group_namespaces_and_wire_types() {
		let current = NowPlaying {
			media_item: NowPlayingMediaItem {
				persistent_identifier: Some(0x0123_4567_89ab_cdef),
				title: Some("Track".into()),
				playback_duration: Some(180_000),
				album_track_number: Some(2),
				..Default::default()
			},
			status: Some(PlaybackStatus::Stopped),
			elapsed_time: Some(0),
			apple_music_radio_ad: Some(false),
			playback_speed: Some(150),
			set_elapsed_time_allowed: Some(true),
			..Default::default()
		};
		let message = Message::with(
			ids::ID_NOW_PLAYING_UPDATE,
			current.encode_diff(&NowPlaying::default()),
		);
		let decoded = Message::decode_one(&message.encode().unwrap()).unwrap().0;
		let media = decoded.get(0).unwrap();
		assert_eq!(
			media.sub(0).unwrap().unwrap().data.as_ref(),
			&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,]
		);
		assert_eq!(media.sub(1).unwrap().unwrap().data.as_ref(), b"Track\0");
		assert_eq!(media.sub(4).unwrap().unwrap().as_u32().unwrap(), 180_000);
		assert_eq!(media.sub(7).unwrap().unwrap().as_u16().unwrap(), 2);
		let playback = decoded.get(1).unwrap();
		assert_eq!(playback.sub(0).unwrap().unwrap().as_u8().unwrap(), 0);
		assert_eq!(playback.sub(1).unwrap().unwrap().as_u32().unwrap(), 0);
		assert!(!playback.sub(9).unwrap().unwrap().as_bool().unwrap());
		assert_eq!(playback.sub(12).unwrap().unwrap().as_u16().unwrap(), 150);
		assert!(playback.sub(13).unwrap().unwrap().as_bool().unwrap());
	}

	#[test]
	fn elapsed_time_update_omits_unchanged_metadata_and_empty_groups() {
		let previous = NowPlaying {
			media_item: NowPlayingMediaItem {
				title: Some("Track".into()),
				..Default::default()
			},
			status: Some(PlaybackStatus::Playing),
			elapsed_time: Some(1_000),
			..Default::default()
		};
		assert!(previous.encode_diff(&previous).is_empty());
		assert!(
			NowPlaying::default()
				.encode_diff(&NowPlaying::default())
				.is_empty()
		);
		let mut current = previous.clone();
		current.elapsed_time = Some(2_000);
		assert_eq!(
			current.encode_diff(&previous),
			vec![Param::group(1, &[Param::u32_(1, 2_000)]),]
		);
	}

	#[test]
	fn like_and_ban_encode_support_and_value_together() {
		let previous = NowPlayingMediaItem {
			liked: Some(MediaItemLiked::Unsupported),
			banned: Some(MediaItemBanned::Unsupported),
			..Default::default()
		};
		let initial = previous
			.encode_diff(&NowPlayingMediaItem::default())
			.unwrap();
		assert_eq!(
			initial.as_group().unwrap(),
			vec![Param::bool(21, false), Param::bool(22, false),]
		);
		let current = NowPlayingMediaItem {
			liked: Some(MediaItemLiked::No),
			banned: Some(MediaItemBanned::Yes),
			..Default::default()
		};
		assert_eq!(
			current.encode_diff(&previous).unwrap().as_group().unwrap(),
			vec![
				Param::bool(21, true),
				Param::bool(23, false),
				Param::bool(22, true),
				Param::bool(24, true),
			]
		);
		assert_eq!(
			previous.encode_diff(&current).unwrap().as_group().unwrap(),
			vec![Param::bool(21, false), Param::bool(22, false),]
		);
		let reversed = NowPlayingMediaItem {
			liked: Some(MediaItemLiked::Yes),
			banned: Some(MediaItemBanned::No),
			..Default::default()
		};
		assert_eq!(
			reversed.encode_diff(&current).unwrap().as_group().unwrap(),
			vec![
				Param::bool(21, true),
				Param::bool(23, true),
				Param::bool(22, true),
				Param::bool(24, false),
			]
		);
	}

	#[test]
	fn unspecified_values_are_omitted_and_explicit_clears_are_sent() {
		let previous = NowPlaying {
			media_item: NowPlayingMediaItem {
				title: Some("Track".into()),
				liked: Some(MediaItemLiked::Yes),
				banned: Some(MediaItemBanned::No),
				..Default::default()
			},
			..Default::default()
		};
		assert!(NowPlaying::default().encode_diff(&previous).is_empty());
		let current = NowPlaying {
			media_item: NowPlayingMediaItem {
				title: Some(String::new()),
				..Default::default()
			},
			..Default::default()
		};
		assert_eq!(
			current.encode_diff(&previous),
			vec![Param::group(0, &[Param::utf8(1, "")]),]
		);
		let mut previous = previous;
		previous.set_elapsed_time_allowed = Some(true);
		assert!(NowPlaying::default().encode_diff(&previous).is_empty());
		let current = NowPlaying {
			set_elapsed_time_allowed: Some(false),
			..Default::default()
		};
		assert_eq!(
			current.encode_diff(&previous),
			vec![Param::group(1, &[Param::bool(13, false)]),]
		);
	}

	#[test]
	fn explicit_false_seek_capability_is_encoded_against_default() {
		let current = NowPlaying {
			set_elapsed_time_allowed: Some(false),
			..Default::default()
		};
		assert_eq!(
			current.encode_diff(&NowPlaying::default()),
			vec![Param::group(1, &[Param::bool(13, false)])],
		);
		assert!(current.encode_diff(&current).is_empty());
	}
}
