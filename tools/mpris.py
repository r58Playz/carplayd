"""Read desktop MPRIS players without blocking the GTK main loop."""

import hashlib
import logging
import math


LOG = logging.getLogger("debug-network-client.mpris")
PREFIX = "org.mpris.MediaPlayer2."
PATH = "/org/mpris/MediaPlayer2"
ROOT = "org.mpris.MediaPlayer2"
PLAYER = ROOT + ".Player"
PROPERTIES = "org.freedesktop.DBus.Properties"
NO_TRACK = PATH + "/TrackList/NoTrack"


def milliseconds(value):
    return min(0xFFFFFFFF, max(0, value // 1000)) if type(value) is int else 0


def text(value):
    if isinstance(value, str):
        return value.replace("\0", "")
    if isinstance(value, (list, tuple)):
        return ", ".join(text(item) for item in value if isinstance(item, str))
    return ""


def number(value):
    return min(0xFFFF, max(0, value)) if type(value) is int else 0


def now_playing(name, root, player):
    """Convert MPRIS microseconds and Xesam metadata into a complete snapshot."""
    metadata = player.get("Metadata")
    if not isinstance(metadata, dict):
        metadata = {}
    track = metadata.get("mpris:trackid")
    identifier = 0
    if isinstance(track, str) and track and track != NO_TRACK:
        identifier = int.from_bytes(hashlib.sha256(f"{name}\0{track}".encode()).digest()[:8], "big")
    duration = milliseconds(metadata.get("mpris:length"))
    status = player.get("PlaybackStatus", "Stopped")
    if status not in ("Playing", "Paused", "Stopped"):
        status = "Stopped"
    elapsed = milliseconds(player.get("Position")) if status != "Stopped" else 0
    if duration:
        elapsed = min(duration, elapsed)
    rate = player.get("Rate", 1.0)
    if type(rate) not in (int, float) or not math.isfinite(rate) or rate <= 0:
        rate = 1.0
    return {
        "media_item": {
            "persistent_identifier": identifier,
            # Missing metadata must explicitly clear the preceding track's fields.
            "title": text(metadata.get("xesam:title")),
            "artist": text(metadata.get("xesam:artist")),
            "album_title": text(metadata.get("xesam:album")),
            "genre": text(metadata.get("xesam:genre")),
            "composer": text(metadata.get("xesam:composer")),
            "playback_duration": duration,
            "album_track_number": number(metadata.get("xesam:trackNumber")),
            "album_disc_number": number(metadata.get("xesam:discNumber")),
            "album_track_count": 0,
            "album_disc_count": 0,
            "chapter_count": 0,
            "liked": "Unsupported",
            "banned": "Unsupported",
        },
        "status": status,
        "elapsed_time": elapsed,
        "app_name": text(root.get("Identity")) or name.removeprefix(PREFIX),
        "shuffle_mode": "Songs" if player.get("Shuffle") is True else "Off",
        "repeat_mode": {"Track": "One", "Playlist": "All"}.get(player.get("LoopStatus"), "Off"),
        "playback_speed": min(0xFFFF, round(min(rate, 655.35) * 100)),
        # The provider currently has no command for setting elapsed time.
        "set_elapsed_time_allowed": False,
    }


class MprisBackend:
    def __init__(self, on_update, player=None, connection=None):
        self.on_update = on_update
        self.preferred = player
        self.selected = None
        self.connection = connection
        self.cancellable = None
        self.closed = False
        self.refreshing = self.refresh_pending = False
        self.timer = None
        self.subscriptions = []
        self.last_snapshot = object()

    def start(self):
        global Gio, GLib
        from gi.repository import Gio, GLib

        self.connection = self.connection or Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.cancellable = Gio.Cancellable()
        for sender, interface, signal, path, callback in (
            ("org.freedesktop.DBus", "org.freedesktop.DBus", "NameOwnerChanged",
             "/org/freedesktop/DBus", self._owner_changed),
            (None, PROPERTIES, "PropertiesChanged", PATH, self._properties_changed),
            (None, PLAYER, "Seeked", PATH, self._seeked),
        ):
            self.subscriptions.append(self.connection.signal_subscribe(
                sender, interface, signal, path, None, Gio.DBusSignalFlags.NONE, callback, None,
            ))
        # Position does not emit PropertiesChanged; sample it even for static metadata.
        self.timer = GLib.timeout_add_seconds(1, self._refresh)
        self._refresh()

    def _call(self, name, path, interface, method, parameters, signature, callback):
        self.connection.call(
            name, path, interface, method, parameters, GLib.VariantType.new(signature),
            Gio.DBusCallFlags.NO_AUTO_START, 2000, self.cancellable, callback, None,
        )

    def _matches(self, name):
        if not name.startswith(PREFIX):
            return False
        if not self.preferred:
            return True
        wanted = self.preferred if self.preferred.startswith(PREFIX) else PREFIX + self.preferred
        return name == wanted or name.startswith(wanted + ".")

    def _refresh(self):
        if self.closed:
            return False
        if self.refreshing:
            self.refresh_pending = True
            return True
        self.refreshing = True
        self._call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                   "ListNames", None, "(as)", self._names_ready)
        return True

    def _names_ready(self, connection, result, _):
        if self.closed:
            return
        try:
            names = sorted(name for name in connection.call_finish(result).unpack()[0] if self._matches(name))
        except Exception as exc:
            LOG.warning("Could not discover MPRIS players: %s", exc)
            self._finish_refresh({})
            return
        if not names:
            self._finish_refresh({})
            return
        batch = {"remaining": len(names) * 2, "players": {name: {} for name in names}}
        for name in names:
            for interface in (ROOT, PLAYER):
                self._call(
                    name, PATH, PROPERTIES, "GetAll", GLib.Variant("(s)", (interface,)), "(a{sv})",
                    lambda c, r, _, n=name, i=interface: self._properties_ready(c, r, batch, n, i),
                )

    def _properties_ready(self, connection, result, batch, name, interface):
        if self.closed:
            return
        try:
            batch["players"][name][interface] = connection.call_finish(result).unpack()[0]
        except Exception as exc:
            LOG.debug("Could not read %s from %s: %s", interface, name, exc)
        batch["remaining"] -= 1
        if batch["remaining"] == 0:
            self._finish_refresh(batch["players"])

    def _finish_refresh(self, players):
        candidates = [name for name, properties in players.items() if PLAYER in properties]
        ranks = {"Playing": 0, "Paused": 1, "Stopped": 2}
        selected = min(candidates, key=lambda name: (
            ranks.get(players[name][PLAYER].get("PlaybackStatus"), 2),
            name != self.selected, name,
        ), default=None)
        if selected != self.selected:
            LOG.info("MPRIS player: %s", selected or "none")
        self.selected = selected
        snapshot = now_playing(selected, players[selected].get(ROOT, {}), players[selected][PLAYER]) if selected else None
        if snapshot != self.last_snapshot:
            self.last_snapshot = snapshot
            self.on_update(snapshot)
        self.refreshing = False
        if self.refresh_pending:
            self.refresh_pending = False
            GLib.idle_add(self._refresh_once)

    def _refresh_once(self):
        self._refresh()
        return False

    def _owner_changed(self, connection, sender, path, interface, signal, parameters, _):
        if self._matches(parameters.unpack()[0]):
            self._refresh()

    def _properties_changed(self, connection, sender, path, interface, signal, parameters, _):
        if parameters.unpack()[0] in (ROOT, PLAYER):
            self._refresh()

    def _seeked(self, *args):
        self._refresh()

    def close(self):
        self.closed = True
        if self.timer is not None:
            GLib.source_remove(self.timer)
            self.timer = None
        if self.connection:
            if self.cancellable:
                self.cancellable.cancel()
            for subscription in self.subscriptions:
                self.connection.signal_unsubscribe(subscription)
            self.subscriptions.clear()
