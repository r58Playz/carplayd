#pragma once
#include "core.hpp"
#include <aap_protobuf/service/media/sink/MediaSinkService.pb.h>
#include <aap_protobuf/service/mediaplayback/message/MediaPlaybackMetadata.pb.h>
#include <aap_protobuf/service/mediaplayback/message/MediaPlaybackStatus.pb.h>
#include <algorithm>

namespace bridge {
inline void video_configuration(aap_protobuf::service::media::sink::MediaSinkService &service,
                                const Geometry &g) {
    namespace shared = aap_protobuf::service::media::shared::message;
    namespace sink = aap_protobuf::service::media::sink::message;
    service.set_available_type(shared::MEDIA_CODEC_VIDEO_H264_BP);
    auto *config = service.add_video_configs();
    config->set_codec_resolution(static_cast<sink::VideoCodecResolutionType>(g.resolution));
    config->set_frame_rate(g.fps == 60 ? sink::VIDEO_FPS_60 : sink::VIDEO_FPS_30);
    config->set_density(160);
    config->set_real_density(160);
    config->set_pixel_aspect_ratio_e4(10000);
    auto *margins = config->mutable_ui_config()->mutable_margins();
    margins->set_left(g.left);
    margins->set_top(g.top);
    margins->set_right(g.canvas_width - g.left - g.width);
    margins->set_bottom(g.canvas_height - g.top - g.height);
    // UiConfig supplies exact per-side margins, including asymmetric chroma
    // alignment. Do not add deprecated symmetric margins a second time.
}
inline uint32_t milliseconds(uint32_t seconds) {
    return uint32_t(std::min<uint64_t>(uint64_t(seconds) * 1000, UINT32_MAX));
}
inline void metadata_update(
    Json &snapshot,
    const aap_protobuf::service::mediaplayback::message::MediaPlaybackMetadata &metadata) {
    auto &item = snapshot["media_item"];
    // carplayd treats absent optional fields as unspecified, not cleared.
    // Explicit defaults prevent the previous track's text from persisting.
    item = {{"title", ""}, {"artist", ""}, {"album_title", ""}, {"playback_duration", 0}};
    if (metadata.has_song())
        item["title"] = metadata.song();
    if (metadata.has_artist())
        item["artist"] = metadata.artist();
    if (metadata.has_album())
        item["album_title"] = metadata.album();
    if (metadata.has_duration_seconds())
        item["playback_duration"] = milliseconds(metadata.duration_seconds());
}
inline void playback_update(
    Json &snapshot,
    const aap_protobuf::service::mediaplayback::message::MediaPlaybackStatus &playback) {
    if (playback.has_state())
        snapshot["status"] = playback.state() == playback.PLAYING  ? "Playing"
                             : playback.state() == playback.PAUSED ? "Paused"
                                                                   : "Stopped";
    if (playback.has_playback_seconds())
        snapshot["elapsed_time"] = milliseconds(playback.playback_seconds());
    if (playback.has_media_source())
        snapshot["app_name"] = playback.media_source();
    if (playback.has_shuffle())
        snapshot["shuffle_mode"] = playback.shuffle() ? "Songs" : "Off";
    if (playback.has_repeat() || playback.has_repeat_one())
        snapshot["repeat_mode"] = playback.repeat_one() ? "One" : playback.repeat() ? "All" : "Off";
}
} // namespace bridge
