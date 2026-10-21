// Audio playback through the GStreamer build bundled with the Yandex firmware
// (/system/vendor/quasar/libs/libquasar_gstreamer.so, loaded with dlopen) into
// the ALSA "speakers" PCM (dmix + softvol). Two independent channels:
//   MUSIC    - media_player content, can be paused/ducked.
//   ANNOUNCE - TTS, announcements, earcons; plays over (ducked) music.
// Public methods are thread-safe; callbacks are delivered on the Loop thread.
#pragma once

#include <functional>
#include <memory>
#include <string>
#include <vector>

#include "core/loop.h"

namespace cs {

enum class Channel { MUSIC = 0, ANNOUNCE = 1 };
enum class PlayState { IDLE, LOADING, PLAYING, PAUSED, ERROR };

class Player {
 public:
  using DoneCb = std::function<void(bool ok)>;  // natural end (true) or error (false); not called on stop()
  using StateCb = std::function<void(Channel, PlayState)>;

  explicit Player(Loop &loop);
  ~Player();
  bool init();  // dlopen + gst_init + static plugin registration + GMainLoop thread

  // Plays urls sequentially (http(s)://, file://, or absolute path). Replaces current playback on the channel.
  void play(Channel ch, std::vector<std::string> urls, DoneCb done = nullptr);
  void pause(Channel ch);
  void resume(Channel ch);
  void stop(Channel ch);
  PlayState state(Channel ch) const;

  // Per-channel gain 0..1 (used for ducking MUSIC while listening/speaking).
  void set_channel_gain(Channel ch, float gain);
  void on_state(StateCb cb);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

// Master output volume via ALSA control "Master Playback Volume" (softvol of the "speakers" PCM).
class Mixer {
 public:
  bool open();             // card 0
  float volume() const;    // 0..1
  void set_volume(float);  // 0..1
  bool muted() const;
  void set_muted(bool);

 private:
  void apply();
  void *ctl_ = nullptr;
  float volume_ = 0.5f;
  bool muted_ = false;
};

}  // namespace cs
