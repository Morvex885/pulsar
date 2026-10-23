// Glue between Home Assistant (ESPHome API), the microphone front-end, playback and hardware UI.
// Voice-satellite behaviour follows OHF linux-voice-assistant (satellite.py).
#pragma once

#include <map>
#include <memory>
#include <string>

#include "arbiter.h"
#include "audio/frontend.h"
#include "core/loop.h"
#include "esphome/api_server.h"
#include "hw/hw.h"
#include "media/player.h"
#include "model.h"
#include "sendspin/sendspin_service.h"
#include "update/updater.h"
#include "zigbee/service.h"

namespace cs {

struct Settings {
  // Volume on the stock curve (Model::volume_map), state.conf volume2=. volume= keeps the same raw
  // level on the curve of earlier builds, so a binary rolled back by supervise.sh plays as loud.
  float volume = 0.5f;
  bool has_volume = false;       // volume2= was present
  float volume_legacy = -1;      // volume=; < 0: absent
  bool legacy_on_stock = false;  // volume_curve=2: one interim build wrote volume= on the stock curve
  bool muted = false;          // media mute
  bool mic_muted = false;      // software microphone mute
  bool clock_on = true;
  float clock_brightness = 1.0f;
  int clock_scroll_ms = 400;   // clock text scroll step
  LightState light;            // user ring light
  std::string wake_word = "alisa";
  std::string tz = "MSK-3";    // POSIX TZ (default matches what the Yandex firmware showed); updated from HA
  bool auto_update = false;    // install new releases without asking
  bool nearest_answers = true; // wake arbitration with other pulsar stations
  std::string music_viz = "All styles";  // one of Leds::music_styles()
  std::string clock_music = "Off";       // one of Clock::music_styles()
  float led_speed = 1;             // 0.25..4
  bool voice_indication = true;    // listening / thinking / speaking shown on the ring and the clock
  std::string led_case_color;      // stem of Leds::case_colors(), "" = none
  // Release being installed. Still set after a restart into another version: that release was
  // rolled back by supervise.sh (crash loop), automatic updates skip it.
  std::string update_attempt;
  // zigbee_role=off | pulsar-coordinator | router (ZigbeeService::kRoles)
  ZigbeeService::Mode zigbee_role = ZigbeeService::Mode::OFF;
  // zigbee_flash_failed=<hex>: radio image version given up on (the radio did not take it); not
  // written again until the role changes or another image version is installed.
  uint32_t zigbee_flash_failed = 0;
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
  void on_button(ButtonId id) override;
  void on_light(const LightState &st, uint32_t transition_ms) override;
  void on_number(NumberId id, float value) override;
  void on_text(TextId id, const std::string &value) override;
  void on_select(SelectId id, const std::string &option) override;
  void on_action(ActionId id, const std::string &value, int32_t seconds) override;
  void on_led_show(const std::string &effect, const std::string &color, int32_t seconds) override;
  void on_time(uint32_t epoch_seconds, const std::string &posix_tz) override;
  void on_restart() override;
  void on_voice_start_failed() override;
  void on_voice_audio_overflow() override;
  void on_update(UpdateCommand cmd) override;

 private:
  enum class Phase { IDLE, LISTENING, PROCESSING, SPEAKING };

  void on_wake(const std::string &phrase, float level_db);
  void answer_wake(const std::string &phrase, std::vector<int16_t> preroll);
  void arbitration_done(bool won, const WakeArbiter::Claim &best, size_t peers);
  void cancel_arbitration();
  bool arbitration_on() const { return st_.nearest_answers && arbiter_ok_; }
  void on_command(const std::string &word);
  void update_commands();
  bool music_pause();  // true when something was playing
  void music_resume();
  void start_pipeline(const std::string &phrase, bool earcon, std::vector<int16_t> preroll = {});
  void stop_streaming();
  void play_tts();
  void tts_finished(bool ok);
  void finish_interaction();
  void voice_link_lost();
  void abort_dialog(const char *why);
  void arm_watchdog();
  void interrupt_speech();
  void end_announce();
  void play_closing_earcon(const std::string &path);
  void stop_timer_ring();
  void ring_timer();
  void resume_ring();
  void show_timers();
  void update_ui();
  void show_ui(UiState s);  // ring and clock
  void update_detection();
  void update_duck();
  void apply_gains();
  void set_volume(float v, bool feedback);
  void set_muted(bool m);
  void start_sendspin(const DeviceIdentity &id);
  void start_zigbee(const DeviceIdentity &id);
  void publish_media();
  void update_music();
  void persist();
  void check_update();
  void install_update(const char *why);
  void maybe_auto_update();
  bool idle_for_update() const;
  void publish_update();
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
  WakeArbiter arbiter_;

  Phase phase_ = Phase::IDLE;
  int64_t listen_started_ms_ = 0;  // start of the current LISTENING phase (commands window)
  bool hw_mic_muted_ = false;
  bool error_shown_ = false;
  // Wake arbitration: the window between our claim and the decision. Microphone audio and a
  // command word heard in it are held until the decision.
  bool arbiter_ok_ = false;
  bool arbitrating_ = false;
  std::string arb_phrase_;
  float arb_level_ = -100;
  std::vector<int16_t> arb_audio_;
  std::string arb_command_;
  int64_t commands_blocked_until_ms_ = 0;  // another station answers the wake word just heard
  uint64_t watchdog_ = 0;  // loop timer ending a LISTENING/PROCESSING phase HA never advances
  // Pipeline events carry no run id. HA ends a run it replaces (barge-in: our new start cancels
  // the old run) with a RUN_END sent before the new run's RUN_START, so until RUN_START every
  // event belongs to the replaced run.
  bool awaiting_run_start_ = false;  // this dialog's start request has no RUN_START yet
  bool run_active_ = false;          // some run is between RUN_START and RUN_END
  bool stale_run_end_ = false;       // the RUN_END of a replaced run is still due
  bool stop_sent_ = false;           // voice_request_stop already sent for this dialog's run
  std::string tts_url_;
  bool tts_played_ = false;
  bool continue_conversation_ = false;
  bool announcing_ = false;  // an HA announcement (assist or media player) plays on ANNOUNCE
  bool ducked_ = false;
  bool closing_earcon_ = false;
  uint64_t earcon_seq_ = 0;
  // Completion callbacks act only for what is still current: dialog_ changes whenever a dialog
  // starts or ends, announce_seq_ whenever an HA announcement starts or is interrupted.
  uint64_t dialog_ = 0;
  uint64_t announce_seq_ = 0;
  // timers
  bool timer_ringing_ = false;
  bool ring_yielded_ = false;  // the ring gave the ANNOUNCE channel to speech; resume_ring() continues it
  uint64_t timer_ring_tick_ = 0;
  int64_t timer_ring_started_ms_ = 0;
  struct HaTimer {
    bool active;
    int64_t end_ms;         // while active
    uint32_t seconds_left;  // while paused
  };
  std::map<std::string, HaTimer> timers_;  // HA timers by TimerInfo::id
  std::string shown_timer_;                // id on the clock, "" = none
  HaTimer shown_{};                        // its data when it was put there
  uint64_t timer_tick_ = 0;                // looks again when the shown running timer runs out
  // light sensor publishing
  float last_lux_published_ = -1;
  int64_t last_lux_ms_ = 0;
  // Clock text / face entity states; not persisted (the clock shows the time after a restart).
  std::string clock_text_;
  std::string clock_face_ = "None";
  // updates
  Updater updater_;
  Release release_;            // latest release found; version "" = none known
  std::string update_error_;
  bool update_installing_ = false;
  float update_progress_ = 0;
  uint64_t update_retry_ = 0;  // waits for idle before an automatic install
  // Zigbee radio (Midi): coordinator for ZHA or router in another network.
  std::unique_ptr<ZigbeeService> zigbee_;
  std::string zigbee_status_;  // last status line published
  // Sendspin (Music Assistant) player; declared last so it stops (and stops calling back into
  // the members above) first.
  bool sendspin_pairing_prompt_ = false;
  std::unique_ptr<SendspinService> sendspin_;
};

}  // namespace cs
