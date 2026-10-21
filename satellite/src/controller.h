// Glue between Home Assistant (ESPHome API), the microphone front-end, playback and hardware UI.
// Voice-satellite behaviour follows OHF linux-voice-assistant (satellite.py).
#pragma once

#include <memory>
#include <string>

#include "audio/frontend.h"
#include "core/loop.h"
#include "esphome/api_server.h"
#include "hw/hw.h"
#include "media/player.h"
#include "model.h"
#include "sendspin/sendspin_service.h"

namespace cs {

struct Settings {
  float volume = 0.5f;
  bool muted = false;          // media mute
  bool mic_muted = false;      // software microphone mute
  bool clock_on = true;
  float clock_brightness = 1.0f;
  LightState light;            // user ring light
  std::string wake_word = "alisa";
  std::string tz = "MSK-3";    // POSIX TZ (default matches what the Yandex firmware showed); updated from HA
  bool load(const std::string &path);
  void save(const std::string &path) const;
};

class Controller : public ApiHooks {
 public:
  Controller(Loop &loop, const Model &model, const std::string &state_path);
  ~Controller() override;
  bool start();

  // ApiHooks
  void on_client_connected(const std::string &client_info) override;
  void on_client_disconnected() override;
  void on_voice_subscribed(bool subscribed) override;
  void on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) override;
  void on_announce(const AnnounceRequest &req) override;
  void on_timer_event(const TimerInfo &t) override;
  void on_active_wake_words(const std::vector<std::string> &ids) override;
  void on_media_command(const MediaCommandRequest &cmd) override;
  void on_switch(SwitchId id, bool state) override;
  void on_light(const LightState &st) override;
  void on_number(NumberId id, float value) override;
  void on_time(uint32_t epoch_seconds, const std::string &posix_tz) override;

 private:
  enum class Phase { IDLE, LISTENING, PROCESSING, SPEAKING };

  void on_wake(const std::string &phrase);
  void on_command(const std::string &word);
  void update_commands();
  void music_pause();
  void music_resume();
  void start_pipeline(const std::string &phrase, bool earcon);
  void stop_streaming();
  void play_tts();
  void tts_finished();
  void finish_interaction();
  void stop_timer_ring();
  void ring_timer();
  void update_ui();
  void update_detection();
  void duck(bool on);
  void set_volume(float v, bool feedback);
  void set_muted(bool m);
  void start_sendspin(const DeviceIdentity &id);
  void publish_media();
  void persist();
  std::string model_dir(const std::string &id) const;

  Loop &loop_;
  const Model &model_;
  std::string state_path_;
  Settings st_;
  std::unique_ptr<ApiServer> api_;
  AudioFrontend mic_;
  Player player_;
  Mixer mixer_;
  Leds leds_;
  Clock clock_;
  Input input_;
  LightSensor light_;

  Phase phase_ = Phase::IDLE;
  int64_t listen_started_ms_ = 0;  // start of the current LISTENING phase (commands window)
  bool hw_mic_muted_ = false;
  bool error_shown_ = false;
  std::string tts_url_;
  bool tts_played_ = false;
  bool continue_conversation_ = false;
  bool announcing_ = false;
  // timers
  bool timer_ringing_ = false;
  uint64_t timer_ring_tick_ = 0;
  int64_t timer_ring_started_ms_ = 0;
  int64_t timer_end_ms_ = 0;  // active countdown shown on the clock (0 = none)
  uint64_t timer_tick_ = 0;
  // light sensor publishing
  float last_lux_published_ = -1;
  int64_t last_lux_ms_ = 0;
  // Sendspin (Music Assistant) player; declared last so it stops (and stops calling back into
  // the members above) first.
  bool sendspin_pairing_prompt_ = false;
  std::unique_ptr<SendspinService> sendspin_;
};

}  // namespace cs
