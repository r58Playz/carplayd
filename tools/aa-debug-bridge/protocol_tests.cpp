#include "aa_protocol.hpp"
#include <iostream>
int main() {
    using namespace bridge;
    try {
        auto g = Geometry::from_init(
            {{"display_width", 1080}, {"display_height", 678}, {"display_fps", 30}});
        aap_protobuf::service::media::sink::MediaSinkService service;
        video_configuration(service, g);
        auto wire = service.SerializeAsString();
        aap_protobuf::service::media::sink::MediaSinkService parsed;
        require(parsed.ParseFromString(wire), "invalid video configuration protobuf");
        const auto &config = parsed.video_configs(0);
        const auto &margin = config.ui_config().margins();
        require(config.codec_resolution() == 2 && margin.left() == 0 && margin.right() == 200 &&
                    margin.top() == 0 && margin.bottom() == 42,
                "AA margins disagree with crop");
        require(!config.has_width_margin() && !config.has_height_margin(), "margins applied twice");
        Json snapshot = {{"media_item", Json::object()}};
        aap_protobuf::service::mediaplayback::message::MediaPlaybackMetadata metadata;
        metadata.set_song("Track");
        metadata.set_artist("Artist");
        metadata.set_album("Album");
        metadata.set_duration_seconds(123);
        metadata_update(snapshot, metadata);
        aap_protobuf::service::mediaplayback::message::MediaPlaybackStatus status;
        status.set_state(status.PLAYING);
        status.set_playback_seconds(12);
        status.set_media_source("Player");
        status.set_shuffle(true);
        status.set_repeat(true);
        status.set_repeat_one(true);
        playback_update(snapshot, status);
        require(snapshot == Json({{"media_item",
                                   {{"title", "Track"},
                                    {"artist", "Artist"},
                                    {"album_title", "Album"},
                                    {"playback_duration", 123000}}},
                                  {"status", "Playing"},
                                  {"elapsed_time", 12000},
                                  {"app_name", "Player"},
                                  {"shuffle_mode", "Songs"},
                                  {"repeat_mode", "One"}}),
                "now-playing wire shape");
        require(milliseconds(UINT32_MAX) == UINT32_MAX, "metadata milliseconds overflow");
        metadata.Clear();
        metadata.set_song("New track");
        metadata_update(snapshot, metadata);
        require(snapshot["media_item"] == Json({{"title", "New track"},
                                                {"artist", ""},
                                                {"album_title", ""},
                                                {"playback_duration", 0}}),
                "stale track metadata retained");
        std::cout << "AA video margins and metadata conversion checks passed\n";
    } catch (const std::exception &e) {
        std::cerr << e.what() << '\n';
        return 1;
    }
}
