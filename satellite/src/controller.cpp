#include "controller.h"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <unistd.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fstream>
#include <sstream>

#include "core/log.h"
#include "esphome/mdns.h"
#include "update/updater.h"

namespace cs {
namespace {

constexpr const char *kModels = "/system/vendor/quasar/spotter_models/activation/";
constexpr const char *kCommandModel = "/system/vendor/quasar/spotter_models/command";
// After the wake word a stock command ("Алиса, громче") is handled locally if it is heard
// within this window; the HA pipeline is then cancelled.
constexpr int64_t kCommandAfterWakeMs = 2500;
constexpr const char *kSounds = "/system/vendor/quasar/sounds/";
const std::string kSndWake = std::string(kSounds) + "common/spotter_start.mp3";
const std::string kSndCancel = std::string(kSounds) + "common/spotter_cancel.mp3";
const std::string kSndTimer = std::string(kSounds) + "common/timer.wav";
constexpr float kDuckGain = 0.15f;
constexpr int64_t kTimerRingMaxMs = 15 * 60 * 1000;
// Longest wait for the next pipeline event before the dialog is given up locally. HA's voice
// command segmenter ends speech capture after at most 15 s of audio. Between INTENT_START and
// INTENT_END nothing arrives (ESPHome forwards INTENT_PROGRESS only once, when TTS streaming
// starts), and conversation agents (LLMs with tool calls) may think for a minute or more.
constexpr int64_t kListenTimeoutMs = 30'000;
constexpr int64_t kProcessTimeoutMs = 120'000;
constexpr uint16_t kSendspinPort = 8928;
// deCONZ-protocol Zigbee coordinator (ZHA socket:// / Zigbee2MQTT tcp://); 6638 is the port
// network Zigbee coordinators conventionally use.
constexpr uint16_t kZigbeePort = 6638;
// Updates: releases of this repository on GitHub. The first check waits for the network after
// boot; an automatic install waits until the speaker is idle (it restarts the daemon).
constexpr const char *kUpdateRepo = "Morvex885/pulsar";
constexpr int64_t kUpdateFirstCheckMs = 60'000;
constexpr int64_t kUpdateCheckPeriodMs = 6 * 3600'000LL;
constexpr int64_t kUpdateIdleRetryMs = 10 * 60'000;

std::string read_line(const char *path) {
  std::ifstream f(path);
  std::string s;
  std::getline(f, s);
  return s;
}

// Index in Clock::face_names(); -1 for "None" / "".
int clock_face_index(const std::string &name) {
  const auto &names = Clock::face_names();
  for (size_t i = 0; i < names.size(); i++)
    if (names[i] == name) return int(i);
  return -1;
}

}  // namespace

// ---------------- Settings ----------------

bool Settings::load(const std::string &path) {
  std::ifstream f(path);
  if (!f) return false;
  std::string line;
  while (std::getline(f, line)) {
    auto eq = line.find('=');
    if (eq == std::string::npos) continue;
    std::string k = line.substr(0, eq), v = line.substr(eq + 1);
    float fv = strtof(v.c_str(), nullptr);
    if (k == "volume2") {
      volume = fv;
      has_volume = true;
    } else if (k == "volume") volume_legacy = fv;
    else if (k == "volume_curve") legacy_on_stock = v == "2";
    else if (k == "muted") muted = v == "1";
    else if (k == "mic_muted") mic_muted = v == "1";
    else if (k == "clock_on") clock_on = v == "1";
    else if (k == "clock_brightness") clock_brightness = fv;
    else if (k == "clock_scroll_ms") clock_scroll_ms = std::clamp(int(fv), 100, 2000);
    else if (k == "light_on") light.on = v == "1";
    else if (k == "light_brightness") light.brightness = fv;
    else if (k == "light_r") light.r = fv;
    else if (k == "light_g") light.g = fv;
    else if (k == "light_b") light.b = fv;
    else if (k == "light_effect") light.effect = v;
    else if (k == "wake_word") wake_word = v;
    else if (k == "tz") tz = v;
    else if (k == "auto_update") auto_update = v == "1";
    else if (k == "nearest_answers") nearest_answers = v == "1";
    else if (k == "update_attempt") update_attempt = v;
    else if (k == "zigbee_role") zigbee_role = ZigbeeService::parse_mode(v);
    else if (k == "zigbee_flash_failed") zigbee_flash_failed = uint32_t(strtoul(v.c_str(), nullptr, 16));
    else if (k == "music_viz") music_viz = v;
    else if (k == "clock_music") clock_music = v;
    else if (k == "led_speed") led_speed = std::isfinite(fv) ? std::clamp(fv, 0.25f, 4.f) : 1.f;
    else if (k == "voice_indication") voice_indication = v == "1";
    else if (k == "led_case_color") led_case_color = v;
  }
  const auto &styles = Leds::music_styles();
  if (std::find(styles.begin(), styles.end(), music_viz) == styles.end()) music_viz = "All styles";
  const auto &clock_styles = Clock::music_styles();
  if (std::find(clock_styles.begin(), clock_styles.end(), clock_music) == clock_styles.end()) clock_music = "Off";
  return true;
}

void Settings::save(const std::string &path) const {
  std::string tmp = path + ".tmp";
  {
    std::ofstream f(tmp, std::ios::trunc);
    if (!f) {
      LOGW("cannot write %s", tmp.c_str());
      return;
    }
    f << "volume2=" << volume << "\nvolume=" << volume_legacy << "\nmuted=" << muted << "\nmic_muted=" << mic_muted
      << "\nclock_on=" << clock_on << "\nclock_brightness=" << clock_brightness
      << "\nclock_scroll_ms=" << clock_scroll_ms << "\nlight_on=" << light.on
      << "\nlight_brightness=" << light.brightness << "\nlight_r=" << light.r << "\nlight_g=" << light.g
      << "\nlight_b=" << light.b << "\nlight_effect=" << light.effect << "\nwake_word=" << wake_word
      << "\ntz=" << tz << "\nauto_update=" << auto_update << "\nupdate_attempt=" << update_attempt
      << "\nzigbee_role=" << ZigbeeService::role(zigbee_role).name << "\nzigbee_flash_failed=" << std::hex
      << zigbee_flash_failed << std::dec << "\nnearest_answers=" << nearest_answers << "\nmusic_viz=" << music_viz
      << "\nled_speed=" << led_speed << "\nvoice_indication=" << voice_indication
      << "\nled_case_color=" << led_case_color << "\nclock_music=" << clock_music << "\n";
  }
  rename(tmp.c_str(), path.c_str());
}

// ---------------- Controller ----------------

Controller::Controller(Loop &loop, const Model &model, const std::string &state_path)
    : loop_(loop),
      model_(model),
      state_path_(state_path),
      mic_(loop, model),
      player_(loop),
      mixer_(model),
      leds_(loop, model),
      clock_(loop, model),
      input_(loop, model),
      light_(loop),
      arbiter_(loop),
      updater_(loop, kUpdateRepo, PULSAR_VERSION) {}

Controller::~Controller() = default;

std::string Controller::model_dir(const std::string &id) const { return kModels + id; }

bool Controller::start() {
  st_.load(state_path_);
  if (!st_.tz.empty()) {  // clock shows local time before HA connects
    setenv("TZ", st_.tz.c_str(), 1);
    tzset();
  }

  DeviceIdentity id;
  std::string mac = read_line("/sys/class/net/wlan0/address");
  for (auto &c : mac) c = toupper(c);
  std::string suffix;
  for (char c : mac)
    if (c != ':') suffix += tolower(c);
  suffix = suffix.size() >= 6 ? suffix.substr(suffix.size() - 6) : suffix;
  id.name = model_.host_prefix + suffix;
  id.friendly_name = model_.friendly_name;
  id.mac = mac;
  id.model = model_.model_name;
  id.board = model_.codename;
  id.manufacturer = "Yandex";
  id.project_version = PULSAR_VERSION;

  ApiConfig cfg;
  cfg.wake_words = {{"alisa", "Алиса", {"ru"}}, {"yandex", "Яндекс", {"ru"}}};
  if (st_.wake_word != "alisa" && st_.wake_word != "yandex") st_.wake_word = "alisa";
  cfg.active_wake_words = {st_.wake_word};
  cfg.light_effects = Leds::effect_names();
  cfg.music_styles = Leds::music_styles();
  cfg.clock_music_styles = Clock::music_styles();
  cfg.case_colors = {"None"};
  const auto case_colors = Leds::case_colors();
  cfg.case_colors.insert(cfg.case_colors.end(), case_colors.begin(), case_colors.end());
  cfg.clock_faces = {"None"};
  cfg.clock_faces.insert(cfg.clock_faces.end(), Clock::face_names().begin(), Clock::face_names().end());
  const auto slash = state_path_.rfind('/');
  cfg.key_path = (slash == std::string::npos ? std::string(".") : state_path_.substr(0, slash)) + "/api_key";
  if (!model_.zigbee_tty.empty())
    for (const auto &r : ZigbeeService::kRoles) cfg.zigbee_modes.push_back(r.label);
  api_ = std::make_unique<ApiServer>(loop_, id, cfg, *this);
  arbiter_ok_ = arbiter_.start(id.name);

  // Hardware first: UI feedback should work even if HA is not connected.
  if (!leds_.open()) LOGW("LEDs unavailable");
  if (!clock_.open()) LOGW("clock unavailable");
  clock_.set_enabled(st_.clock_on);
  clock_.set_brightness(st_.clock_brightness);
  clock_.set_scroll_ms(st_.clock_scroll_ms);
  clock_.set_voice_indication(st_.voice_indication);
  clock_.set_music_style(st_.clock_music);
  leds_.set_speed(st_.led_speed);
  leds_.set_voice_indication(st_.voice_indication);
  leds_.set_music_style(st_.music_viz);
  leds_.set_user_light(st_.light.on,
                       Rgb{uint8_t(st_.light.r * 255), uint8_t(st_.light.g * 255), uint8_t(st_.light.b * 255)},
                       st_.light.brightness, st_.light.effect, 0);

  if (!mixer_.open()) LOGW("mixer unavailable");
  if (st_.has_volume) {
    // volume2= is authoritative
  } else if (st_.volume_legacy >= 0) {  // written by an earlier build: keep the speaker as loud
    st_.volume = st_.legacy_on_stock ? std::min(1.0f, st_.volume_legacy) : mixer_.from_legacy(st_.volume_legacy);
    LOGI("volume %.3f from an earlier build -> %.3f", st_.volume_legacy, st_.volume);
    persist();
  } else {
    // No volume saved: keep the one the speaker already had, but never start loud: on the Lite 2
    // the softvol control is at 0 dB (100 %) after boot until the Yandex stack sets it. 0.3 is
    // stock level 6 of 20 (Alice's "3").
    st_.volume = std::min(mixer_.volume(), 0.3f);
  }
  mixer_.set_volume(st_.volume);
  mixer_.set_muted(st_.muted);
  if (!st_.led_case_color.empty()) {  // after the volume: persist() writes it too
    const bool listed =
        std::find(case_colors.begin(), case_colors.end(), st_.led_case_color) != case_colors.end();
    if (!listed) LOGW("case colour %s not in this firmware, none used", st_.led_case_color.c_str());
    if (!listed || !leds_.set_case_color(st_.led_case_color)) {  // Leds logs an unreadable file
      st_.led_case_color.clear();
      persist();
    }
  }
  if (!st_.update_attempt.empty()) {
    if (st_.update_attempt == PULSAR_VERSION) {
      LOGI("updated to %s", PULSAR_VERSION);
      st_.update_attempt.clear();
      persist();
    } else {
      LOGW("update to %s did not stay installed (rolled back?); automatic updates skip it",
           st_.update_attempt.c_str());
    }
  }
  publish_update();
  loop_.add_timer(kUpdateFirstCheckMs, [this] { check_update(); }, kUpdateCheckPeriodMs);
  if (!player_.init()) LOGE("player init failed");
  // ANNOUNCING is reported only for HA announcements (on_announce / media announce), not for
  // earcons, timer alarms or pipeline TTS that share the ANNOUNCE playback channel.
  player_.on_state([this](Channel, PlayState) { publish_media(); });

  if (input_.open()) {
    hw_mic_muted_ = input_.hw_muted();
    input_.on_mute([this](bool m) {
      if (m) cancel_arbitration();
      hw_mic_muted_ = m;
      LOGI("hardware mic mute: %d", m);
      api_->publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, m);
      if (m && phase_ == Phase::LISTENING) {
        finish_interaction();
      }
      update_detection();
      update_ui();
    });
    input_.on_button([this](Button b, Press p) {
      if (b == Button::ACTION) {
        api_->fire_event(EventId::BUTTON, p == Press::LONG ? "action_long" : "action");
        if (p == Press::LONG) return;  // exposed to HA automations only
        if (sendspin_pairing_prompt_ && sendspin_) return sendspin_->confirm_pairing_window();
        if (timer_ringing_) return stop_timer_ring();
        if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) {
          interrupt_speech();  // an announcement playing over the dialog goes with it
          play_closing_earcon(kSndCancel);
          return finish_interaction();
        }
        if (phase_ == Phase::SPEAKING || announcing_) {
          interrupt_speech();
          return finish_interaction();
        }
        start_pipeline("", true);
      } else {
        api_->fire_event(EventId::BUTTON, b == Button::VOLUME_UP ? "volume_up" : "volume_down");
        set_volume(st_.volume + (b == Button::VOLUME_UP ? 0.05f : -0.05f), true);
      }
    });
  } else {
    LOGW("input devices unavailable");
  }

  if (light_.open()) {
    light_.on_lux([this](float lux) {
      clock_.set_ambient_lux(lux);
      leds_.set_ambient_lux(lux);
      int64_t now = Loop::now_ms();
      float diff = std::abs(lux - last_lux_published_);
      if (last_lux_published_ < 0 || diff > std::max(1.0f, last_lux_published_ * 0.1f) || now - last_lux_ms_ > 60000) {
        api_->publish_sensor(SensorId::ILLUMINANCE, lux);
        last_lux_published_ = lux;
        last_lux_ms_ = now;
      }
    });
  }

  mic_.on_wake([this](const std::string &phrase, float level_db) { on_wake(phrase, level_db); });
  mic_.on_command([this](const std::string &word) { on_command(word); });
  mic_.on_audio([this](const int16_t *pcm, size_t n) {
    if (phase_ == Phase::LISTENING)
      api_->voice_send_audio(pcm, n);
    else if (arbitrating_)  // the request goes on with what was said right after the wake word
      arb_audio_.insert(arb_audio_.end(), pcm, pcm + n);
  });
  mic_.on_music([this](const MusicFrame &f) {
    leds_.music_frame(f, false);
    clock_.music_frame(f, false);
  });
  if (!mic_.start(model_dir(st_.wake_word), kCommandModel)) LOGE("microphone front-end failed to start");
  update_detection();

  start_sendspin(id);
  apply_gains();  // a saved mute
  if (!api_->start()) {
    LOGE("ESPHome API server failed to start");
    return false;
  }
  if (sendspin_ && api_->mdns())
    api_->mdns()->add_service("_sendspin._tcp", id.name, kSendspinPort,
                              {"path=/sendspin", "name=" + id.friendly_name});
  start_zigbee(id);
  update_ui();
  // Last: start() blocks the loop (Midi ~0.4 s), which would swallow the eyes opening.
  clock_.play(ClockShot::BOOT);
  return true;
}

void Controller::persist() {
  st_.volume_legacy = mixer_.to_legacy(st_.volume);
  st_.save(state_path_);
}

// ---------------- HA connection ----------------

void Controller::on_client_connected(const std::string &client_info) {
  LOGI("Home Assistant connected: %s", client_info.c_str());
  error_shown_ = false;
  publish_media();
  api_->publish_switch(SwitchId::MIC_MUTE, st_.mic_muted);
  api_->publish_switch(SwitchId::CLOCK, st_.clock_on);
  api_->publish_switch(SwitchId::AUTO_UPDATE, st_.auto_update);
  api_->publish_switch(SwitchId::NEAREST_ANSWERS, st_.nearest_answers);
  api_->publish_switch(SwitchId::VOICE_INDICATION, st_.voice_indication);
  publish_update();
  api_->publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, hw_mic_muted_);
  api_->publish_number(NumberId::CLOCK_BRIGHTNESS, st_.clock_brightness * 100.f);
  api_->publish_number(NumberId::CLOCK_SCROLL, float(st_.clock_scroll_ms));
  api_->publish_text(TextId::CLOCK_TEXT, clock_text_);
  api_->publish_select(SelectId::CLOCK_FACE, clock_face_);
  api_->publish_number(NumberId::LED_SPEED, st_.led_speed);
  api_->publish_select(SelectId::MUSIC_VIZ, st_.music_viz);
  api_->publish_select(SelectId::CLOCK_MUSIC, st_.clock_music);
  api_->publish_select(SelectId::CASE_COLOR, st_.led_case_color.empty() ? "None" : st_.led_case_color);
  if (zigbee_) {
    api_->publish_select(SelectId::ZIGBEE_MODE, ZigbeeService::role(st_.zigbee_role).label);
    api_->publish_text_sensor(TextSensorId::ZIGBEE, zigbee_->status());
  }
  api_->publish_light(st_.light);
  if (last_lux_published_ >= 0) api_->publish_sensor(SensorId::ILLUMINANCE, last_lux_published_);
}

void Controller::on_client_disconnected() {
  LOGW("Home Assistant disconnected");
  voice_link_lost();
}

void Controller::on_voice_subscribed(bool subscribed) {
  LOGI("voice assistant %s", subscribed ? "subscribed" : "unsubscribed");
  if (!subscribed) voice_link_lost();
}

// No more events come for the current run, and a stop would reach whichever client subscribes
// next. A dialog still listening or thinking ends now. An answer already playing is fetched over
// HTTP independently of the API link, so it plays out (still ducked) and its end closes the
// dialog; a stream that dies with HA ends it through the player instead.
void Controller::voice_link_lost() {
  awaiting_run_start_ = run_active_ = stale_run_end_ = false;
  continue_conversation_ = false;
  if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) finish_interaction();
}

void Controller::on_time(uint32_t epoch, const std::string &tz) {
  if (!tz.empty()) {
    setenv("TZ", tz.c_str(), 1);
    tzset();
    if (tz != st_.tz) {
      st_.tz = tz;
      persist();
    }
  }
  LOGI("time from HA: epoch=%u tz='%s' (local clock now follows TZ)", epoch, tz.c_str());
}

// ---------------- voice pipeline ----------------

void Controller::update_detection() { mic_.set_detection(!st_.mic_muted && !hw_mic_muted_); }

void Controller::on_wake(const std::string &phrase, float level_db) {
  if (st_.mic_muted || hw_mic_muted_) return;
  LOGI("wake word '%s', level %.1f dB", phrase.c_str(), level_db);
  if (timer_ringing_) {  // the wake word works as a stop word while ringing
    // A ringing station takes the turn so that its neighbours do not start a dialog.
    cancel_arbitration();
    if (arbitration_on()) arbiter_.claim(st_.wake_word, level_db, true, nullptr);
    return stop_timer_ring();
  }
  if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING || arbitrating_) return;
  // Without the HA pipeline this station could win and stay silent: it does not take part.
  if (!arbitration_on() || !api_->voice_subscribed()) return answer_wake(phrase, {});
  arbitrating_ = true;
  commands_blocked_until_ms_ = 0;  // a station that lost the previous wake word may win this one
  arb_phrase_ = phrase;
  arb_level_ = level_db;
  arb_audio_.clear();
  arb_command_.clear();
  mic_.set_streaming(true);
  update_commands();
  update_ui();
  arbiter_.claim(st_.wake_word, level_db, phase_ == Phase::SPEAKING || announcing_,
                 [this](bool won, const WakeArbiter::Claim &best, size_t peers) { arbitration_done(won, best, peers); });
}

void Controller::answer_wake(const std::string &phrase, std::vector<int16_t> preroll) {
  if (timer_ringing_) return stop_timer_ring();
  if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) return;
  if (phase_ == Phase::SPEAKING || announcing_) interrupt_speech();  // barge-in: cut the answer short
  start_pipeline(phrase, true, std::move(preroll));
}

void Controller::arbitration_done(bool won, const WakeArbiter::Claim &best, size_t peers) {
  arbitrating_ = false;
  std::vector<int16_t> audio = std::move(arb_audio_);
  std::string command = std::move(arb_command_);
  arb_audio_.clear();
  arb_command_.clear();
  if (!won) {
    mic_.set_streaming(false);
    LOGI("wake word: %s answers (%s%.1f dB, ours %.1f dB)", best.station.c_str(), best.busy ? "busy, " : "",
         best.level_db, arb_level_);
    // The command spotter of a playing station would still act on "Алиса, громче".
    commands_blocked_until_ms_ = Loop::now_ms() + kCommandAfterWakeMs;
    update_commands();
    update_ui();
    return;
  }
  LOGI("wake word: this station answers (%zu other station(s) heard it)", peers);
  if (!command.empty()) {  // "Алиса, громче": handled locally, as right after the wake word
    mic_.set_streaming(false);
    update_commands();
    update_ui();
    return on_command(command);
  }
  // Capture keeps streaming into the new dialog: switching it off here could drop the chunk
  // between the preroll and live audio.
  answer_wake(arb_phrase_, std::move(audio));
  if (phase_ != Phase::LISTENING) mic_.set_streaming(false);  // no dialog started (e.g. HA gone)
}

void Controller::cancel_arbitration() {
  if (!arbitrating_) return;
  arbiter_.cancel();
  arbitrating_ = false;
  arb_audio_.clear();
  arb_command_.clear();
  if (phase_ != Phase::LISTENING) mic_.set_streaming(false);
}

void Controller::start_pipeline(const std::string &phrase, bool earcon, std::vector<int16_t> preroll) {
  cancel_arbitration();  // the button, a continued conversation or HA's start_conversation win
  if (!api_->voice_subscribed()) {
    LOGW("wake ignored: Home Assistant voice assistant not subscribed");
    show_ui(UiState::ERROR);
    loop_.add_timer(1500, [this] { update_ui(); });
    return;
  }
  LOGI("starting voice pipeline (wake='%s')", phrase.c_str());
  commands_blocked_until_ms_ = 0;
  phase_ = Phase::LISTENING;
  dialog_++;
  stale_run_end_ = run_active_;
  awaiting_run_start_ = true;
  stop_sent_ = false;
  listen_started_ms_ = Loop::now_ms();
  loop_.add_timer(kCommandAfterWakeMs + 100, [this] { update_commands(); });
  tts_url_.clear();
  tts_played_ = false;
  continue_conversation_ = false;
  update_duck();
  if (earcon) player_.play(Channel::ANNOUNCE, {kSndWake});
  const uint64_t dialog = dialog_;
  api_->voice_request_start(phrase);
  if (dialog != dialog_) return;  // sending failed and closed the voice client, which ended the dialog
  // HA drops audio queued before the start and queues what follows it on the same connection.
  for (size_t i = 0; i < preroll.size() && dialog == dialog_; i += 512)
    api_->voice_send_audio(preroll.data() + i, std::min<size_t>(512, preroll.size() - i));
  if (dialog != dialog_) return;  // the audio backlog overflowed and aborted the dialog
  mic_.set_streaming(true);
  arm_watchdog();
  update_ui();
}

void Controller::stop_streaming() {
  mic_.set_streaming(arbitrating_);  // a wake word heard meanwhile keeps its audio
  if (phase_ == Phase::LISTENING) phase_ = Phase::PROCESSING;
}

void Controller::on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) {
  auto get = [&](const char *k) -> std::string {
    auto it = data.find(k);
    return it == data.end() ? std::string() : it->second;
  };
  // Numbers: VaEvent (1 run-start, 2 run-end, 3/4 stt, 5/6 intent, 7/8 tts, 11/12 vad, 0 error).
  if (type != VaEvent::INTENT_PROGRESS) LOGI("voice event %u", (unsigned)type);
  if (type == VaEvent::RUN_START) {
    run_active_ = true;
    awaiting_run_start_ = false;
  } else if (awaiting_run_start_) {
    if (type != VaEvent::RUN_END || stale_run_end_) {
      if (type == VaEvent::RUN_END) stale_run_end_ = run_active_ = false;
      LOGD("voice event %u: from the replaced run, ignored", (unsigned)type);
      return;
    }
    // A RUN_END with no run to replace: HA ended our request without running it (it intercepts
    // the wake word while an automation waits for one).
    awaiting_run_start_ = false;
  } else if (type == VaEvent::RUN_END) {
    run_active_ = false;
  }
  if (phase_ == Phase::IDLE) {  // the run of a dialog that already ended; it is being stopped
    LOGD("voice event %u: dialog already ended, ignored", (unsigned)type);
    return;
  }
  switch (type) {
    case VaEvent::RUN_START:
      tts_url_ = get("url");
      tts_played_ = false;
      continue_conversation_ = false;
      break;
    case VaEvent::STT_VAD_END:
    case VaEvent::STT_END:
      stop_streaming();
      update_ui();
      break;
    case VaEvent::INTENT_START:
      if (phase_ == Phase::LISTENING) stop_streaming();
      update_ui();
      break;
    case VaEvent::INTENT_PROGRESS:
      if (get("tts_start_streaming") == "1") play_tts();
      break;
    case VaEvent::INTENT_END:
      continue_conversation_ = get("continue_conversation") == "1";
      break;
    case VaEvent::TTS_END:
      if (!get("url").empty()) tts_url_ = get("url");
      play_tts();
      break;
    case VaEvent::ERROR:
      // HA accepts one wake word phrase per 2 s across satellites: another one answers.
      if (get("code") == "duplicate_wake_up_detected") {
        LOGI("pipeline: another satellite answers this wake word (HA duplicate wake-up)");
        stop_streaming();
        break;
      }
      LOGW("pipeline error: %s %s", get("code").c_str(), get("message").c_str());
      stop_streaming();
      error_shown_ = true;
      show_ui(UiState::ERROR);
      if (get("code") != "stt-no-text-recognized") play_closing_earcon(kSounds + model_.error_sound);
      break;
    case VaEvent::RUN_END:
      stop_streaming();
      if (!tts_played_) finish_interaction();
      break;
    default:
      break;
  }
  arm_watchdog();
}

void Controller::play_tts() {
  if (tts_played_ || tts_url_.empty()) return;
  tts_played_ = true;
  phase_ = Phase::SPEAKING;
  update_ui();
  player_.play(Channel::ANNOUNCE, {tts_url_}, [this, dialog = dialog_](bool ok) {
    if (dialog == dialog_ && phase_ == Phase::SPEAKING) tts_finished(ok);
    resume_ring();
  });
}

// A cut-short or failed answer does not reopen the microphone, nor does a ringing timer.
void Controller::tts_finished(bool ok) {
  if (ok && continue_conversation_ && !timer_ringing_ && !st_.mic_muted && !hw_mic_muted_) {
    continue_conversation_ = false;
    start_pipeline("", false);
    return;
  }
  finish_interaction();
}

void Controller::finish_interaction() {
  // The dialog may end before its run does (answer cut short, cancel, timeout): stop the run so
  // HA does not go on generating; whatever it still sends is dropped while idle.
  if ((run_active_ || awaiting_run_start_) && !stop_sent_) {
    api_->voice_request_stop();
    stop_sent_ = true;
  }
  awaiting_run_start_ = false;
  mic_.set_streaming(arbitrating_);
  phase_ = Phase::IDLE;
  dialog_++;
  tts_played_ = false;
  tts_url_.clear();
  continue_conversation_ = false;
  update_duck();
  arm_watchdog();
  if (error_shown_) {
    error_shown_ = false;
    loop_.add_timer(1200, [this] { update_ui(); });
  } else {
    update_ui();
  }
}

// The dialog failed without a pipeline run to end it: error ring, then idle.
void Controller::abort_dialog(const char *why) {
  LOGW("voice pipeline: %s", why);
  error_shown_ = true;
  show_ui(UiState::ERROR);
  finish_interaction();
}

void Controller::on_voice_start_failed() {
  if (phase_ != Phase::LISTENING && phase_ != Phase::PROCESSING) return;
  awaiting_run_start_ = false;  // no run follows
  abort_dialog("Home Assistant refused to start it");
}

void Controller::on_voice_audio_overflow() {
  if (phase_ != Phase::LISTENING && phase_ != Phase::PROCESSING) return;
  stop_sent_ = true;  // by the server itself
  abort_dialog("network too slow for the microphone audio");
}

// Without it a run HA never ends (lost connection state, a hung agent) would leave the ring
// listening, the microphone streaming and the wake word ignored.
void Controller::arm_watchdog() {
  if (watchdog_) loop_.cancel_timer(watchdog_);
  watchdog_ = 0;
  if (phase_ != Phase::LISTENING && phase_ != Phase::PROCESSING) return;
  watchdog_ = loop_.add_timer(phase_ == Phase::LISTENING ? kListenTimeoutMs : kProcessTimeoutMs, [this] {
    watchdog_ = 0;
    abort_dialog("no progress from Home Assistant, giving up");
  });
}

// Cuts the TTS answer or HA announcement short. An interrupted announcement ends here (its
// completion callback then only reports to HA); the dialog of an interrupted answer is ended or
// replaced by the caller, otherwise by the TTS completion callback.
void Controller::interrupt_speech() {
  player_.stop(Channel::ANNOUNCE);
  if (announcing_) {
    announcing_ = false;
    announce_seq_++;
  }
}

// Error and cancel sounds outlive the dialog they close; music stays ducked until they end.
void Controller::play_closing_earcon(const std::string &path) {
  closing_earcon_ = true;
  const uint64_t seq = ++earcon_seq_;
  player_.play(Channel::ANNOUNCE, {path}, [this, seq](bool) {
    if (seq != earcon_seq_) return;
    closing_earcon_ = false;
    update_duck();
  });
}

// Announcements do not take over the dialog phase, and their end leaves any dialog alone. HA
// cancels its own running pipeline before it announces (assist_satellite async_internal_announce),
// so a dialog still active here is either ending (its RUN_END or the watchdog follows) or was
// started by HA after the announcement.
void Controller::on_announce(const AnnounceRequest &req) {
  LOGI("announce: '%s'%s", req.text.c_str(), phase_ == Phase::IDLE ? "" : " (during a dialog)");
  std::vector<std::string> urls;
  if (!req.preannounce_media_id.empty()) urls.push_back(req.preannounce_media_id);
  urls.push_back(req.media_id);
  announcing_ = true;
  const bool conv = req.start_conversation;
  const uint64_t seq = ++announce_seq_;
  update_duck();
  update_ui();
  publish_media();
  player_.play(Channel::ANNOUNCE, urls, [this, conv, seq](bool ok) {
    api_->voice_announce_finished(ok);  // exactly once per announcement, also when cut short
    if (seq == announce_seq_) {
      announcing_ = false;
      if (ok && conv && phase_ == Phase::IDLE && !timer_ringing_ && !st_.mic_muted && !hw_mic_muted_)
        start_pipeline("", false);
      end_announce();
    }
    resume_ring();
  });
}

void Controller::end_announce() {
  update_duck();
  update_ui();
  publish_media();
}

// ---------------- timers ----------------

void Controller::on_timer_event(const TimerInfo &t) {
  LOGI("timer %u '%s' (%s) left=%u active=%d", (unsigned)t.event, t.name.c_str(), t.id.c_str(), t.seconds_left,
       t.is_active);
  switch (t.event) {
    case TimerEvent::STARTED:
      if (!timers_.count(t.id)) leds_.show_timer_started();
      timers_[t.id] = HaTimer{t.is_active, Loop::now_ms() + int64_t(t.seconds_left) * 1000, t.seconds_left};
      break;
    case TimerEvent::UPDATED:
      timers_[t.id] = HaTimer{t.is_active, Loop::now_ms() + int64_t(t.seconds_left) * 1000, t.seconds_left};
      break;
    case TimerEvent::CANCELLED:
      timers_.erase(t.id);
      break;
    case TimerEvent::FINISHED:
      timers_.erase(t.id);
      if (!timer_ringing_) {
        timer_ringing_ = true;
        timer_ring_started_ms_ = Loop::now_ms();
        update_duck();
        update_ui();
        ring_timer();
      }
      break;
  }
  show_timers();
}

// The clock shows the running timer that ends first, else the paused one with the least time
// left. The clock counts a running one down by itself, so it is set only when the choice or the
// timer's data changes, and looked at again when the shown one runs out.
void Controller::show_timers() {
  const int64_t now = Loop::now_ms();
  const std::string *best_id = nullptr;
  const HaTimer *best = nullptr;
  int64_t best_left = 0;
  for (const auto &[id, tm] : timers_) {
    const int64_t left = tm.active ? tm.end_ms - now : int64_t(tm.seconds_left) * 1000;
    if (left <= 0) continue;  // ran out locally; HA's FINISHED follows
    if (!best || (tm.active && !best->active) || (tm.active == best->active && left < best_left)) {
      best_id = &id;
      best = &tm;
      best_left = left;
    }
  }
  if (timer_tick_) loop_.cancel_timer(timer_tick_);
  timer_tick_ = 0;
  if (!best) {
    if (!shown_timer_.empty()) clock_.set_timer(0, true);
    shown_timer_.clear();
    return;
  }
  const bool same = *best_id == shown_timer_ && best->active == shown_.active &&
                    (best->active ? best->end_ms == shown_.end_ms : best->seconds_left == shown_.seconds_left);
  if (!same) {
    clock_.set_timer(best_left, best->active);
    shown_timer_ = *best_id;
    shown_ = *best;
  }
  if (best->active) timer_tick_ = loop_.add_timer(best_left, [this] {
    timer_tick_ = 0;
    show_timers();
  });
}

// The ring gives the ANNOUNCE channel to speech (TTS answer, HA announcement) and resumes after it.
void Controller::ring_timer() {
  if (timer_ring_tick_) loop_.cancel_timer(timer_ring_tick_);
  timer_ring_tick_ = 0;
  if (!timer_ringing_) return;
  if (Loop::now_ms() - timer_ring_started_ms_ > kTimerRingMaxMs) return stop_timer_ring();
  if (announcing_ || phase_ == Phase::SPEAKING) {
    ring_yielded_ = true;
    return;
  }
  player_.play(Channel::ANNOUNCE, {kSndTimer}, [this](bool) {
    if (timer_ringing_ && !timer_ring_tick_) timer_ring_tick_ = loop_.add_timer(300, [this] { ring_timer(); });
  });
}

void Controller::resume_ring() {
  if (!ring_yielded_) return;
  ring_yielded_ = false;
  ring_timer();
}

void Controller::stop_timer_ring() {
  timer_ringing_ = false;
  ring_yielded_ = false;
  if (timer_ring_tick_) loop_.cancel_timer(timer_ring_tick_);
  timer_ring_tick_ = 0;
  interrupt_speech();  // the ring, or the speech it yielded to
  update_duck();
  update_ui();
}

// ---------------- media ----------------

// Music (HA media channel and Sendspin) is ducked while a dialog, HA announcement, timer ring or
// closing earcon is on.
void Controller::update_duck() {
  const bool want = phase_ != Phase::IDLE || announcing_ || timer_ringing_ || closing_earcon_;
  if (want == ducked_) return;
  ducked_ = want;
  apply_gains();
}

// Mute: softvol raw 0 is the bottom of its dB range (-60 dB on the Midi), still audible, so mute
// also zeroes the software gains of both playback channels and Sendspin.
void Controller::apply_gains() {
  const float music = st_.muted ? 0.0f : ducked_ ? kDuckGain : 1.0f;
  player_.set_channel_gain(Channel::MUSIC, music);
  player_.set_channel_gain(Channel::ANNOUNCE, st_.muted ? 0.0f : 1.0f);
  if (sendspin_) sendspin_->set_duck_gain(music);
}

void Controller::start_sendspin(const DeviceIdentity &id) {
  SendspinConfig cfg;
  cfg.name = id.friendly_name;
  cfg.product_name = id.model;
  cfg.manufacturer = id.manufacturer;
  cfg.software_version = id.project_version;
  for (char c : id.mac) cfg.mac += char(tolower(c));
  auto slash = state_path_.rfind('/');
  cfg.state_dir = (slash == std::string::npos ? std::string(".") : state_path_.substr(0, slash)) + "/sendspin";
  cfg.port = kSendspinPort;

  SendspinService::Hooks h;
  h.on_stream = [this](bool active) {
    // Music Assistant took over the speaker: the HA media channel yields to it.
    if (active && player_.state(Channel::MUSIC) != PlayState::IDLE) player_.stop(Channel::MUSIC);
    publish_media();
  };
  h.on_volume = [this](float v) { set_volume(v, false); };
  h.on_mute = [this](bool m) { set_muted(m); };
  h.on_pairing_code = [this](const std::string &code) {
    if (code.empty()) clock_.clear_code();
    else clock_.show_code(code);
  };
  h.on_pairing_window = [this](bool open) {
    sendspin_pairing_prompt_ = open;
    if (open) clock_.show_code("----");
    else clock_.clear_code();
  };
  h.on_music = [this](const MusicFrame &f) {
    leds_.music_frame(f, true);
    clock_.music_frame(f, true);
  };
  sendspin_ = std::make_unique<SendspinService>(loop_, std::move(cfg), std::move(h));
  if (!sendspin_->start(st_.volume, st_.muted)) {
    LOGE("Sendspin player unavailable");
    sendspin_.reset();
  }
}

void Controller::start_zigbee(const DeviceIdentity &id) {
  if (model_.zigbee_tty.empty()) return;
  const auto slash = state_path_.rfind('/');
  const std::string dir = slash == std::string::npos ? std::string(".") : state_path_.substr(0, slash);
  ZigbeeService::Config cfg;
  cfg.lines = {model_.zigbee_tty, model_.zigbee_reset, model_.zigbee_boot};
  cfg.coordinator_image = dir + "/zigbee-coordinator.ybl";
  cfg.router_image = dir + "/zigbee-router.ybl";
  updater_.add_extra_asset("zigbee-coordinator.ybl", cfg.coordinator_image);
  updater_.add_extra_asset("zigbee-router.ybl", cfg.router_image);
  cfg.deconz_port = kZigbeePort;
  cfg.failed_version = st_.zigbee_flash_failed;
  cfg.outcome = [this](uint32_t failed) {
    if (failed == st_.zigbee_flash_failed) return;
    st_.zigbee_flash_failed = failed;
    persist();
  };
  const auto mode = st_.zigbee_role;
  zigbee_ = std::make_unique<ZigbeeService>(loop_, std::move(cfg), mode, [this] {
    const std::string s = zigbee_->status();
    if (s == zigbee_status_) return;
    zigbee_status_ = s;
    LOGI("zigbee: %s", s.c_str());
    api_->publish_text_sensor(TextSensorId::ZIGBEE, s);
  });
  zigbee_->start();
  // ZHA discovers the coordinator (radio_type selects zigpy-deconz, serial_number is its unique id).
  if (mode == ZigbeeService::Mode::COORDINATOR && api_->mdns())
    api_->mdns()->add_service("_zigbee-coordinator._tcp", id.name, kZigbeePort,
                              {"radio_type=deconz", "serial_number=" + id.name, "name=" + id.friendly_name});
}

void Controller::set_muted(bool m) {
  st_.muted = m;
  mixer_.set_muted(m);
  apply_gains();
  if (sendspin_) sendspin_->report_volume(st_.volume, st_.muted);
  publish_media();
  persist();
}

void Controller::set_volume(float v, bool feedback) {
  st_.volume = std::min(1.0f, std::max(0.0f, v));
  mixer_.set_volume(st_.volume);
  if (st_.muted && feedback) {
    st_.muted = false;
    mixer_.set_muted(false);
    apply_gains();
  }
  if (feedback) {
    leds_.show_volume(st_.volume);
    clock_.flash_number(int(st_.volume * 100 + 0.5f));
  }
  if (sendspin_) sendspin_->report_volume(st_.volume, st_.muted);
  publish_media();
  persist();
}

void Controller::publish_media() {
  update_commands();  // command spotter follows playback state
  MediaState s = MediaState::IDLE;
  if (announcing_) {
    s = MediaState::ANNOUNCING;
  } else {
    switch (player_.state(Channel::MUSIC)) {
      case PlayState::PLAYING:
      case PlayState::LOADING:
        s = MediaState::PLAYING;
        break;
      case PlayState::PAUSED:
        s = sendspin_ && sendspin_->streaming() ? MediaState::PLAYING : MediaState::PAUSED;
        break;
      default:
        s = sendspin_ && sendspin_->streaming() ? MediaState::PLAYING : MediaState::IDLE;
    }
  }
  api_->publish_media(s, st_.volume, st_.muted);
  update_music();
}

void Controller::update_music() {
  const PlayState ps = player_.state(Channel::MUSIC);
  const bool media =
      ps == PlayState::PLAYING || ps == PlayState::LOADING || (sendspin_ && sendspin_->streaming());
  leds_.set_media_playing(media);
  clock_.set_media_playing(media);
  // The speaker-reference analysis only runs while its result can be shown.
  mic_.set_music_analysis(media && ((st_.music_viz != "Off" && !st_.light.on) ||
                                    (st_.clock_music != "Off" && st_.clock_on)));
}

void Controller::on_media_command(const MediaCommandRequest &cmd) {
  if (cmd.has_media_url) {
    if (cmd.has_announcement && cmd.announcement) {
      // Media-player announcements report completion through state only.
      announcing_ = true;
      const uint64_t seq = ++announce_seq_;
      update_duck();
      update_ui();
      player_.play(Channel::ANNOUNCE, {cmd.media_url}, [this, seq](bool) {
        if (seq == announce_seq_) {
          announcing_ = false;
          end_announce();
        }
        resume_ring();
      });
    } else {
      if (sendspin_) sendspin_->yield_to_local();  // HA media takes the speaker from Sendspin
      player_.play(Channel::MUSIC, {cmd.media_url});
    }
  }
  if (cmd.has_volume) set_volume(cmd.volume, false);
  if (cmd.has_command) {
    const bool ss_on = sendspin_ && sendspin_->streaming();
    switch (cmd.command) {
      case MediaCommand::PLAY:
        if (ss_on && player_.state(Channel::MUSIC) == PlayState::PAUSED) sendspin_->yield_to_local();
        player_.resume(Channel::MUSIC);
        break;
      case MediaCommand::PAUSE:
        if (ss_on || player_.state(Channel::MUSIC) == PlayState::PLAYING ||
            player_.state(Channel::MUSIC) == PlayState::LOADING)
          clock_.play(ClockShot::PAUSE);
        // While Sendspin plays, HA's pause/stop leave the Sendspin group (the speaker stops).
        if (ss_on && player_.state(Channel::MUSIC) != PlayState::PLAYING) sendspin_->yield_to_local();
        player_.pause(Channel::MUSIC);
        break;
      case MediaCommand::TOGGLE:
        if (player_.state(Channel::MUSIC) == PlayState::PLAYING) {
          clock_.play(ClockShot::PAUSE);
          player_.pause(Channel::MUSIC);
        } else if (ss_on) {
          clock_.play(ClockShot::PAUSE);
          sendspin_->yield_to_local();
        } else {
          player_.resume(Channel::MUSIC);
        }
        break;
      case MediaCommand::STOP:
        player_.stop(Channel::MUSIC);
        if (ss_on) sendspin_->yield_to_local();
        break;
      case MediaCommand::MUTE:
      case MediaCommand::UNMUTE:
        set_muted(cmd.command == MediaCommand::MUTE);
        break;
      case MediaCommand::VOLUME_UP:
        set_volume(st_.volume + 0.05f, false);
        break;
      case MediaCommand::VOLUME_DOWN:
        set_volume(st_.volume - 0.05f, false);
        break;
      default:
        LOGD("media command %u ignored", (unsigned)cmd.command);
    }
  }
  publish_media();
}

// ---------------- entities ----------------

void Controller::on_restart() {
  // A restart cuts the upload short (the shutdown watchdog does not wait ~25 s); the radio would
  // be written again on the next start, but stays without Zigbee meanwhile.
  if (zigbee_ && zigbee_->flashing()) {
    LOGW("restart requested from Home Assistant: refused while the Zigbee radio is being written");
    return;
  }
  LOGW("restart requested from Home Assistant");
  loop_.stop();
}

void Controller::on_switch(SwitchId id, bool state) {
  if (id == SwitchId::MIC_MUTE) {
    st_.mic_muted = state;
    if (state) cancel_arbitration();
    if (state && phase_ == Phase::LISTENING) {
      finish_interaction();
    }
    update_detection();
    update_ui();
  } else if (id == SwitchId::CLOCK) {
    st_.clock_on = state;
    clock_.set_enabled(state);
    update_music();
  } else if (id == SwitchId::AUTO_UPDATE) {
    st_.auto_update = state;
  } else if (id == SwitchId::NEAREST_ANSWERS) {
    st_.nearest_answers = state;
  } else if (id == SwitchId::VOICE_INDICATION) {
    st_.voice_indication = state;
    leds_.set_voice_indication(state);
    clock_.set_voice_indication(state);
  }
  api_->publish_switch(id, state);
  persist();
  if (id == SwitchId::AUTO_UPDATE) maybe_auto_update();
}

// ---------------- updates ----------------

void Controller::publish_update() {
  UpdateState u;
  u.current_version = PULSAR_VERSION;
  u.title = "pulsar";
  // latest == current hides the offer: nothing newer, or not checked yet.
  bool offer = !release_.version.empty() && release_newer(release_.version, PULSAR_VERSION);
  u.latest_version = offer ? release_.version : u.current_version;
  u.release_url = release_.url;
  u.in_progress = u.has_progress = update_installing_;
  u.progress = update_progress_;
  api_->publish_update(u);
}

void Controller::check_update() {
  updater_.check([this](const std::string &err, const Release &r) {
    if (!err.empty()) {
      LOGI("update check: %s", err.c_str());
      return;
    }
    if (r.version != release_.version) LOGI("latest release %s (running %s)", r.version.c_str(), PULSAR_VERSION);
    release_ = r;
    publish_update();
    maybe_auto_update();
  });
}

// Restarting cuts off whatever plays: automatic installs wait for silence.
bool Controller::idle_for_update() const {
  return phase_ == Phase::IDLE && !announcing_ && !timer_ringing_ &&
         player_.state(Channel::MUSIC) != PlayState::PLAYING && !(sendspin_ && sendspin_->streaming()) &&
         !(zigbee_ && zigbee_->flashing());
}

void Controller::maybe_auto_update() {
  if (update_retry_) {
    loop_.cancel_timer(update_retry_);
    update_retry_ = 0;
  }
  bool current_known = false;
  if (!st_.auto_update || release_.version.empty() || update_installing_ ||
      !release_newer(release_.version, PULSAR_VERSION, &current_known) || !current_known ||
      release_.version == st_.update_attempt)
    return;
  if (!idle_for_update()) {
    update_retry_ = loop_.add_timer(kUpdateIdleRetryMs, [this] {
      update_retry_ = 0;
      maybe_auto_update();
    });
    return;
  }
  install_update("automatic");
}

void Controller::install_update(const char *why) {
  if (updater_.busy() || update_installing_) return;
  if (zigbee_ && zigbee_->flashing()) {  // the install ends with a restart
    LOGW("update: not while the Zigbee radio is being written");
    return;
  }
  char exe[512];
  ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
  if (n <= 0) {
    LOGE("update: cannot find the running binary");
    return;
  }
  // A binary replaced under the running daemon (installer, manual mv) reads "<path> (deleted)":
  // the install location is still <path>.
  std::string binary(exe, size_t(n));
  const std::string deleted = " (deleted)";
  if (binary.size() > deleted.size() && binary.ends_with(deleted)) binary.resize(binary.size() - deleted.size());
  const Release r = release_;
  LOGW("installing release %s (%s)", r.version.c_str(), why);
  st_.update_attempt = r.version;
  persist();
  update_installing_ = true;
  clock_.set_updating(true);
  update_progress_ = 0;
  publish_update();
  updater_.install(
      r, binary,
      [this](float p) {
        update_progress_ = p;
        publish_update();
      },
      [this, r](const std::string &err, bool fatal) {
        if (err.empty()) {
          LOGW("installed %s, restarting", r.version.c_str());
          update_progress_ = 100;
          publish_update();
          loop_.add_timer(500, [this] { loop_.stop(); });  // exit 0: supervise.sh starts the new binary
          return;
        }
        LOGE("update to %s failed: %s", r.version.c_str(), err.c_str());
        update_installing_ = false;
        clock_.set_updating(false);
        if (!fatal) {  // a network failure: the next check tries again
          st_.update_attempt.clear();
          persist();
        }
        publish_update();
      });
}

void Controller::on_update(UpdateCommand cmd) {
  if (cmd == UpdateCommand::CHECK) return check_update();
  if (cmd != UpdateCommand::INSTALL) return;
  if (release_.version.empty() || release_.version == PULSAR_VERSION) {
    LOGW("update: no other release to install");
    return;
  }
  install_update("requested from Home Assistant");
}

void Controller::on_number(NumberId id, float value) {
  if (id == NumberId::CLOCK_BRIGHTNESS) {
    st_.clock_brightness = std::min(100.f, std::max(0.f, value)) / 100.f;
    clock_.set_brightness(st_.clock_brightness);
    api_->publish_number(id, st_.clock_brightness * 100.f);
    persist();
  } else if (id == NumberId::CLOCK_SCROLL) {
    st_.clock_scroll_ms = std::clamp(int(std::lround(value / 50.f)) * 50, 100, 2000);
    clock_.set_scroll_ms(st_.clock_scroll_ms);
    api_->publish_number(id, float(st_.clock_scroll_ms));
    persist();
  } else if (id == NumberId::LED_SPEED) {
    st_.led_speed = std::clamp(value, 0.25f, 4.f);
    leds_.set_speed(st_.led_speed);
    api_->publish_number(id, st_.led_speed);
    persist();
  }
}

void Controller::on_text(TextId id, const std::string &value) {
  clock_text_ = value;
  clock_.set_text(value);
  api_->publish_text(id, value);
  // Text and face replace each other on the display; Clock::set_text already dropped the face.
  if (!value.empty() && clock_face_ != "None") {
    clock_face_ = "None";
    api_->publish_select(SelectId::CLOCK_FACE, clock_face_);
  }
}

void Controller::on_select(SelectId id, const std::string &option) {
  if (id == SelectId::ZIGBEE_MODE) {
    if (!zigbee_) return;
    auto role = st_.zigbee_role;
    for (const auto &r : ZigbeeService::kRoles)
      if (option == r.label) role = r.mode;
    if (role == st_.zigbee_role) return api_->publish_select(id, ZigbeeService::role(role).label);
    // The role starts with the daemon, which writes the radio firmware it needs; the
    // coordinator's mDNS advertisement is only made in that role.
    LOGW("zigbee: role %s chosen, restarting", ZigbeeService::role(role).name);
    st_.zigbee_role = role;
    st_.zigbee_flash_failed = 0;
    persist();
    api_->publish_select(id, ZigbeeService::role(role).label);
    loop_.stop();
    return;
  }
  if (id == SelectId::MUSIC_VIZ) {
    st_.music_viz = option;
    leds_.set_music_style(option);
    api_->publish_select(id, option);
    persist();
    update_music();
    return;
  }
  if (id == SelectId::CLOCK_MUSIC) {
    st_.clock_music = option;
    clock_.set_music_style(option);
    api_->publish_select(id, option);
    persist();
    update_music();
    return;
  }
  if (id == SelectId::CASE_COLOR) {
    const std::string stem = option == "None" ? "" : option;
    st_.led_case_color = leds_.set_case_color(stem) ? stem : "";
    api_->publish_select(id, st_.led_case_color.empty() ? "None" : st_.led_case_color);
    persist();
    return;
  }
  clock_face_ = option;
  clock_.set_face(clock_face_index(option));
  api_->publish_select(id, option);
  if (option != "None" && !clock_text_.empty()) {
    clock_text_.clear();
    api_->publish_text(TextId::CLOCK_TEXT, clock_text_);
  }
}

void Controller::on_button(ButtonId id) {
  if (!zigbee_) return;
  if (zigbee_->mode() != ZigbeeService::Mode::ROUTER) {
    LOGW("zigbee: join/leave only apply to the router role");
    return;
  }
  if (id == ButtonId::ZIGBEE_JOIN) zigbee_->join();
  else zigbee_->leave();
}

void Controller::on_action(ActionId id, const std::string &value, int32_t seconds) {
  const bool text = id == ActionId::CLOCK_SHOW_TEXT;
  LOGI("clock: temporary %s '%s' for %d s", text ? "text" : "face", value.c_str(), int(seconds));
  if (text) clock_.show_temporary_text(value, seconds);
  else clock_.show_temporary_face(clock_face_index(value), seconds);
}

void Controller::on_led_show(const std::string &effect, const std::string &color, int32_t seconds) {
  if (effect.empty() && color.empty()) return leds_.clear_temporary();
  Rgb rgb{255, 255, 255};
  if (!color.empty()) {
    const std::string hex = color[0] == '#' ? color.substr(1) : color;
    if (hex.size() != 6 || !std::all_of(hex.begin(), hex.end(), [](char c) { return isxdigit((unsigned char)c); })) {
      LOGW("led_show: bad colour '%s'", color.c_str());
      return;
    }
    const unsigned long v = strtoul(hex.c_str(), nullptr, 16);
    rgb = Rgb{uint8_t(v >> 16), uint8_t(v >> 8), uint8_t(v)};
  }
  LOGI("leds: show '%s' #%02x%02x%02x, %d s requested", effect.c_str(), rgb.r, rgb.g, rgb.b, int(seconds));
  leds_.show_temporary(effect, rgb, seconds);
}

void Controller::on_light(const LightState &l, uint32_t transition_ms) {
  st_.light = l;
  leds_.set_user_light(l.on, Rgb{uint8_t(l.r * 255), uint8_t(l.g * 255), uint8_t(l.b * 255)}, l.brightness,
                       l.effect == "None" ? "" : l.effect, transition_ms);
  persist();
  update_music();
}

void Controller::on_active_wake_words(const std::vector<std::string> &ids) {
  if (ids.empty()) return;
  if (ids[0] == st_.wake_word) return;
  st_.wake_word = ids[0];
  LOGI("wake word -> %s", st_.wake_word.c_str());
  mic_.set_model(model_dir(st_.wake_word));
  api_->set_active_wake_words({st_.wake_word});
  persist();
}

// Stock command spotter ("стоп", "хватит", "пауза", "продолжи", "громче", "тише", "дальше", "назад"):
// active without the wake word while something plays, and right after the wake word.
void Controller::update_commands() {
  bool playing = player_.state(Channel::MUSIC) == PlayState::PLAYING || (sendspin_ && sendspin_->streaming()) ||
                 announcing_ || timer_ringing_ || phase_ == Phase::SPEAKING;
  bool after_wake =
      (phase_ == Phase::LISTENING && Loop::now_ms() - listen_started_ms_ < kCommandAfterWakeMs) || arbitrating_;
  mic_.set_commands(!st_.mic_muted && !hw_mic_muted_ && (playing || after_wake));
}

bool Controller::music_pause() {
  bool paused = false;
  if (player_.state(Channel::MUSIC) == PlayState::PLAYING || player_.state(Channel::MUSIC) == PlayState::LOADING) {
    player_.pause(Channel::MUSIC);
    paused = true;
  }
  if (sendspin_ && sendspin_->streaming()) {
    sendspin_->transport(SendspinService::Transport::PAUSE);
    paused = true;
  }
  return paused;
}

void Controller::music_resume() {
  if (player_.state(Channel::MUSIC) == PlayState::PAUSED)
    player_.resume(Channel::MUSIC);
  else if (sendspin_)
    sendspin_->transport(SendspinService::Transport::PLAY);
}

void Controller::on_command(const std::string &word) {
  if (st_.mic_muted || hw_mic_muted_) return;
  enum { NONE, STOP, PAUSE, RESUME, LOUDER, QUIETER, NEXT, PREV } c = NONE;
  if (word == "стоп" || word == "хватит") c = STOP;
  else if (word == "пауза") c = PAUSE;
  else if (word == "продолжи" || word == "играй") c = RESUME;
  else if (word == "громче") c = LOUDER;
  else if (word == "тише") c = QUIETER;
  else if (word == "дальше" || word == "следующий") c = NEXT;
  else if (word == "назад") c = PREV;
  if (c == NONE) return;  // TV navigation, "включи свет" etc. are left to Home Assistant
  if (Loop::now_ms() < commands_blocked_until_ms_) {
    LOGI("voice command '%s' ignored: another station answers this wake word", word.c_str());
    return;
  }
  if (arbitrating_) {
    arb_command_ = word;
    LOGI("voice command '%s' held until the wake arbitration ends", word.c_str());
    return;
  }
  bool after_wake = phase_ == Phase::LISTENING && Loop::now_ms() - listen_started_ms_ < kCommandAfterWakeMs;
  LOGI("voice command '%s'%s", word.c_str(), after_wake ? " (after wake word, HA pipeline cancelled)" : "");
  if (after_wake) {
    finish_interaction();
  }
  switch (c) {
    case STOP:
      if (timer_ringing_) {
        stop_timer_ring();
      } else if (phase_ == Phase::SPEAKING) {
        interrupt_speech();
        finish_interaction();
      } else if (announcing_) {  // a dialog listening meanwhile goes on
        interrupt_speech();
        update_duck();
        update_ui();
      } else {
        if (music_pause()) clock_.play(ClockShot::PAUSE);
      }
      break;
    case PAUSE:
      if (music_pause()) clock_.play(ClockShot::PAUSE);
      break;
    case RESUME:
      music_resume();
      break;
    case LOUDER:
    case QUIETER:
      set_volume(st_.volume + (c == LOUDER ? 0.1f : -0.1f), true);
      break;
    case NEXT:
    case PREV:
      if (sendspin_ && sendspin_->transport(c == NEXT ? SendspinService::Transport::NEXT
                                                      : SendspinService::Transport::PREVIOUS))
        clock_.play(c == NEXT ? ClockShot::NEXT : ClockShot::PREVIOUS);
      else
        LOGI("'%s' ignored: no Sendspin queue to skip in", word.c_str());
      break;
    case NONE:
      break;
  }
  update_commands();
}

void Controller::update_ui() {
  leds_.set_muted(st_.mic_muted || hw_mic_muted_);
  update_commands();
  if (timer_ringing_) return show_ui(UiState::TIMER_RING);
  switch (phase_) {
    case Phase::LISTENING:
      return show_ui(UiState::LISTENING);
    case Phase::PROCESSING:
      return show_ui(UiState::THINKING);
    case Phase::SPEAKING:
      return show_ui(UiState::SPEAKING);
    case Phase::IDLE:
      break;
  }
  if (arbitrating_) return show_ui(UiState::LISTENING);
  if (announcing_) return show_ui(UiState::SPEAKING);
  show_ui(UiState::IDLE);
}

void Controller::show_ui(UiState s) {
  leds_.set_ui_state(s);
  clock_.set_ui_state(s);
}

}  // namespace cs
