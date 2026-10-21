// Native Sendspin player (Music Assistant's synchronized streaming protocol) built on the official
// sendspin-cpp client library. Server-initiated mode: the library listens on ws://<ip>:8928/sendspin
// and the device is advertised as _sendspin._tcp by our mDNS responder (done by the owner).
//
// Threads: the library's protocol task and sync (decode) task run on their own threads; its
// "main loop" (SendspinClient::loop()) is driven by a cs::Loop timer, so every hook below and every
// public method runs on the cs::Loop thread. Decoded PCM goes straight to a dedicated ALSA
// playback handle on PCM "speakers" (dmix, mixed with the GStreamer channels) from the sync task.
#pragma once

#include <cstdint>
#include <functional>
#include <memory>
#include <string>

#include "core/loop.h"

namespace cs {

struct SendspinConfig {
  std::string name;  // friendly name shown in Music Assistant
  std::string product_name;
  std::string manufacturer;
  std::string software_version;
  std::string mac;        // "aa:bb:cc:dd:ee:ff"
  std::string state_dir;  // identity/pairing persistence directory
  uint16_t port = 8928;
};

class SendspinService {
 public:
  struct Hooks {
    std::function<void(bool active)> on_stream;      // a stream started (true) / ended (false)
    std::function<void(float volume)> on_volume;     // server set the player volume, 0..1
    std::function<void(bool muted)> on_mute;         // server set the player mute
    std::function<void(const std::string &code)> on_pairing_code;  // show code; empty = clear
    std::function<void(bool open)> on_pairing_window;  // ask the operator for the pairing gesture
  };

  SendspinService(Loop &loop, SendspinConfig cfg, Hooks hooks);
  ~SendspinService();
  SendspinService(const SendspinService &) = delete;
  SendspinService &operator=(const SendspinService &) = delete;

  // Starts the client (WebSocket server on cfg.port) with the current local volume/mute.
  bool start(float volume, bool muted);

  bool streaming() const;  // a Sendspin audio stream is playing on this speaker
  bool connected() const;  // a Sendspin server (e.g. Music Assistant) is connected

  // Software gain 0..1 applied to Sendspin audio (smoothly ramped), used for ducking.
  void set_duck_gain(float gain);
  // Local volume/mute change (buttons, HA): reported to the server when it differs.
  void report_volume(float volume, bool muted);

  // Group transport command sent to the server (controller role): affects the whole Sendspin
  // group this speaker is in. Returns false when no server is connected or it does not currently
  // offer the command (e.g. NEXT with an empty queue).
  enum class Transport { PLAY, PAUSE, STOP, NEXT, PREVIOUS };
  bool transport(Transport t);

  // Another local source took over the speaker: leave the Sendspin group (stops our playback,
  // the rest of the group keeps playing).
  void yield_to_local();
  // Operator performed the pairing-window gesture.
  void confirm_pairing_window();

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

}  // namespace cs
