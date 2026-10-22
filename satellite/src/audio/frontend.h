// Microphone front-end: ALSA capture (mic + speaker-feedback channels, layout per Model)
// -> Yandex VQE (AEC / beamforming / noise reduction, per-model preset)
// -> Yandex phrase spotters (stock models from /system/vendor/quasar/spotter_models):
//    the wake word ("Алиса"/"Яндекс") and, optionally, the command model ("стоп", "громче", ...).
// Both engines come from the firmware's libyandex_evil.so (C API, loaded with dlopen).
// Runs in its own thread; callbacks are delivered on the Loop thread.
#pragma once

#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "audio/music_frame.h"
#include "core/loop.h"
#include "model.h"

namespace cs {

class AudioFrontend {
 public:
  // level_db: wake word level in dBFS on VQE_OMNI (after echo cancellation, before AGC), rounded to
  // 0.1 dB; -100 = no data.
  using WakeCb = std::function<void(const std::string &phrase, float level_db)>;
  using CommandCb = std::function<void(const std::string &word)>;
  using AudioCb = std::function<void(const int16_t *pcm, size_t samples)>;  // 16 kHz mono
  using MusicCb = std::function<void(const MusicFrame &f)>;  // one per VQE chunk (32 ms)

  AudioFrontend(Loop &loop, const Model &model);
  ~AudioFrontend();

  // model_dir: wake-word model, e.g. .../spotter_models/activation/alisa.
  // command_model_dir: stock command model (.../spotter_models/command), optional.
  bool start(const std::string &model_dir, const std::string &command_model_dir = "");
  void stop();

  // Switch wake-word model (applied by the capture thread before the next chunk).
  void set_model(const std::string &model_dir);
  // Wake-word detection on/off (e.g. software mute).
  void set_detection(bool on) { detect_.store(on); }
  // Command spotter on/off (commands without the wake word).
  void set_commands(bool on) { commands_.store(on); }
  // While on, cleaned 16 kHz audio is delivered via on_audio().
  void set_streaming(bool on) { stream_.store(on); }
  // While on, music features of the speaker reference are delivered via on_music().
  void set_music_analysis(bool on) { music_.store(on); }

  void on_wake(WakeCb cb) { wake_cb_ = std::move(cb); }
  void on_command(CommandCb cb) { command_cb_ = std::move(cb); }
  void on_audio(AudioCb cb) { audio_cb_ = std::move(cb); }
  void on_music(MusicCb cb) { music_cb_ = std::move(cb); }

 private:
  struct Engine;
  void run();

  Loop &loop_;
  const Model &model_;
  std::unique_ptr<Engine> eng_;
  std::thread thr_;
  std::atomic<bool> running_{false};
  std::mutex stop_mu_;  // with stop_cv_: wakes the capture thread's retry sleep on stop()
  std::condition_variable stop_cv_;
  std::atomic<bool> detect_{true};
  std::atomic<bool> commands_{false};
  std::atomic<bool> stream_{false};
  std::atomic<bool> music_{false};
  std::mutex model_mu_;
  std::string pending_model_;
  WakeCb wake_cb_;
  CommandCb command_cb_;
  AudioCb audio_cb_;
  MusicCb music_cb_;
};

}  // namespace cs
