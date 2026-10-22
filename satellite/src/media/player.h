// Audio playback through the GStreamer build bundled with the Yandex firmware
// (/system/vendor/quasar/libs/libquasar_gstreamer.so, loaded with dlopen) into
// the model's ALSA playback PCM (Model::playback_pcm: "speakers", or "default" on the Mini 2;
// dmix + softvol). Two independent channels:
//   MUSIC    - media_player content, can be paused/ducked.
//   ANNOUNCE - TTS, announcements, earcons; plays over (ducked) music.
// Public methods are thread-safe; callbacks are delivered on the Loop thread.
#pragma once

#include <functional>
#include <memory>
#include <string>
#include <vector>

#include "core/loop.h"
#include "model.h"

namespace cs {

enum class Channel { MUSIC = 0, ANNOUNCE = 1 };
enum class PlayState { IDLE, LOADING, PLAYING, PAUSED, ERROR };

class Player {
 public:
  // Called exactly once per play(): true if every url played to its end; false if any url failed
  // or the list was cut short by stop() or by a later play() on the same channel. A superseded
  // list's done(false) is queued to the Loop by that stop()/play() call itself, so it runs after
  // the caller returns and before anything the new playback reports.
  using DoneCb = std::function<void(bool ok)>;
  using StateCb = std::function<void(Channel, PlayState)>;

  explicit Player(Loop &loop);
  ~Player();
  // dlopen + gst_init + static plugin registration + GMainLoop thread; pcm = Model::playback_pcm.
  bool init(const std::string &pcm = "speakers");

  // Plays urls sequentially (http(s)://, file://, or absolute path). Replaces current playback on the channel.
  // MUSIC retries an http(s) url that fails for network reasons (3 times, 1/2/4 s apart, resuming
  // at the position reached) before moving on; local files and ANNOUNCE fail at once.
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

// Master output volume via ALSA control "Master Playback Volume" (softvol of the playback PCM),
// on the stock curve of the model (Model::volume_map): volume v is platform level v * top level.
class Mixer {
 public:
  explicit Mixer(const Model &model) : model_(model) {}
  bool open();             // card 0
  float volume() const;    // 0..1
  void set_volume(float);  // 0..1
  bool muted() const;
  void set_muted(bool);
  // Volume on the curve of builds before the stock map (state.conf volume=) <-> the volume giving
  // the same raw level now (legacy levels below the lowest stock step rise to it).
  float from_legacy(float v) const;
  float to_legacy(float v) const;

 private:
  int to_raw(float v) const;
  float from_raw(int raw) const;
  void apply();
  const Model &model_;
  void *ctl_ = nullptr;
  float volume_ = 0.5f;
  bool muted_ = false;
};

}  // namespace cs
