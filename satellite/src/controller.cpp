#include "controller.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fstream>
#include <sstream>

#include "core/log.h"
#include "esphome/mdns.h"

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
constexpr uint16_t kSendspinPort = 8928;

std::string read_line(const char *path) {
  std::ifstream f(path);
  std::string s;
  std::getline(f, s);
  return s;
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
    if (k == "volume") volume = fv;
    else if (k == "muted") muted = v == "1";
    else if (k == "mic_muted") mic_muted = v == "1";
    else if (k == "clock_on") clock_on = v == "1";
    else if (k == "clock_brightness") clock_brightness = fv;
    else if (k == "light_on") light.on = v == "1";
    else if (k == "light_brightness") light.brightness = fv;
    else if (k == "light_r") light.r = fv;
    else if (k == "light_g") light.g = fv;
    else if (k == "light_b") light.b = fv;
    else if (k == "light_effect") light.effect = v;
    else if (k == "wake_word") wake_word = v;
    else if (k == "tz") tz = v;
  }
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
    f << "volume=" << volume << "\nmuted=" << muted << "\nmic_muted=" << mic_muted << "\nclock_on=" << clock_on
      << "\nclock_brightness=" << clock_brightness << "\nlight_on=" << light.on
      << "\nlight_brightness=" << light.brightness << "\nlight_r=" << light.r << "\nlight_g=" << light.g
      << "\nlight_b=" << light.b << "\nlight_effect=" << light.effect << "\nwake_word=" << wake_word
      << "\ntz=" << tz << "\n";
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
      leds_(loop, model),
      clock_(loop, model),
      input_(loop, model),
      light_(loop) {}

Controller::~Controller() = default;

std::string Controller::model_dir(const std::string &id) const { return kModels + id; }

bool Controller::start() {
  const bool have_state = st_.load(state_path_);
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
  id.project_version = "0.1.0";

  ApiConfig cfg;
  cfg.wake_words = {{"alisa", "Алиса", {"ru"}}, {"yandex", "Яндекс", {"ru"}}};
  if (st_.wake_word != "alisa" && st_.wake_word != "yandex") st_.wake_word = "alisa";
  cfg.active_wake_words = {st_.wake_word};
  cfg.light_effects = {"Rainbow", "Breathe"};
  api_ = std::make_unique<ApiServer>(loop_, id, cfg, *this);

  // Hardware first: UI feedback should work even if HA is not connected.
  if (!leds_.open()) LOGW("LEDs unavailable");
  if (!clock_.open()) LOGW("clock unavailable");
  clock_.set_enabled(st_.clock_on);
  clock_.set_brightness(st_.clock_brightness);
  leds_.set_user_light(st_.light.on,
                       Rgb{uint8_t(st_.light.r * 255), uint8_t(st_.light.g * 255), uint8_t(st_.light.b * 255)},
                       st_.light.brightness, st_.light.effect);

  if (!mixer_.open()) LOGW("mixer unavailable");
  // First run: keep the volume the speaker already had, but never start loud: on the Lite 2 the
  // softvol control is at 0 dB (100 %) after boot until the Yandex stack sets it.
  if (!have_state) st_.volume = std::min(mixer_.volume(), 0.3f);
  mixer_.set_volume(st_.volume);
  mixer_.set_muted(st_.muted);
  if (!player_.init()) LOGE("player init failed");
  // ANNOUNCING is reported only for HA announcements (on_announce / media announce), not for
  // earcons, timer alarms or pipeline TTS that share the ANNOUNCE playback channel.
  player_.on_state([this](Channel, PlayState) { publish_media(); });

  if (input_.open()) {
    hw_mic_muted_ = input_.hw_muted();
    input_.on_mute([this](bool m) {
      hw_mic_muted_ = m;
      LOGI("hardware mic mute: %d", m);
      api_->publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, m);
      if (m && phase_ == Phase::LISTENING) {
        api_->voice_request_stop();
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
        if (phase_ == Phase::SPEAKING || announcing_) {
          player_.stop(Channel::ANNOUNCE);
          announcing_ = false;
          return tts_finished();
        }
        if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) {
          api_->voice_request_stop();
          player_.play(Channel::ANNOUNCE, {kSndCancel});
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
      int64_t now = Loop::now_ms();
      float diff = std::abs(lux - last_lux_published_);
      if (last_lux_published_ < 0 || diff > std::max(1.0f, last_lux_published_ * 0.1f) || now - last_lux_ms_ > 60000) {
        api_->publish_sensor(SensorId::ILLUMINANCE, lux);
        last_lux_published_ = lux;
        last_lux_ms_ = now;
      }
    });
  }

  mic_.on_wake([this](const std::string &phrase) { on_wake(phrase); });
  mic_.on_command([this](const std::string &word) { on_command(word); });
  mic_.on_audio([this](const int16_t *pcm, size_t n) {
    if (phase_ == Phase::LISTENING) api_->voice_send_audio(pcm, n);
  });
  if (!mic_.start(model_dir(st_.wake_word), kCommandModel)) LOGE("microphone front-end failed to start");
  update_detection();

  start_sendspin(id);
  if (!api_->start()) {
    LOGE("ESPHome API server failed to start");
    return false;
  }
  if (sendspin_ && api_->mdns())
    api_->mdns()->add_service("_sendspin._tcp", id.name, kSendspinPort,
                              {"path=/sendspin", "name=" + id.friendly_name});
  update_ui();
  return true;
}

void Controller::persist() { st_.save(state_path_); }

// ---------------- HA connection ----------------

void Controller::on_client_connected(const std::string &client_info) {
  LOGI("Home Assistant connected: %s", client_info.c_str());
  error_shown_ = false;
  publish_media();
  api_->publish_switch(SwitchId::MIC_MUTE, st_.mic_muted);
  api_->publish_switch(SwitchId::CLOCK, st_.clock_on);
  api_->publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, hw_mic_muted_);
  api_->publish_number(NumberId::CLOCK_BRIGHTNESS, st_.clock_brightness * 100.f);
  api_->publish_light(st_.light);
  if (last_lux_published_ >= 0) api_->publish_sensor(SensorId::ILLUMINANCE, last_lux_published_);
}

void Controller::on_client_disconnected() {
  LOGW("Home Assistant disconnected");
  if (phase_ != Phase::IDLE) finish_interaction();
}

void Controller::on_voice_subscribed(bool subscribed) {
  LOGI("voice assistant %s", subscribed ? "subscribed" : "unsubscribed");
  if (!subscribed && phase_ != Phase::IDLE) finish_interaction();
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

void Controller::on_wake(const std::string &phrase) {
  if (st_.mic_muted || hw_mic_muted_) return;
  if (timer_ringing_) return stop_timer_ring();  // the wake word works as a stop word while ringing
  if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) return;
  if (phase_ == Phase::SPEAKING || announcing_) {  // barge-in: interrupt the answer
    player_.stop(Channel::ANNOUNCE);
    announcing_ = false;
  }
  start_pipeline(phrase, true);
}

void Controller::start_pipeline(const std::string &phrase, bool earcon) {
  if (!api_->voice_subscribed()) {
    LOGW("wake ignored: Home Assistant voice assistant not subscribed");
    leds_.set_ui_state(UiState::ERROR);
    loop_.add_timer(1500, [this] { update_ui(); });
    return;
  }
  LOGI("starting voice pipeline (wake='%s')", phrase.c_str());
  phase_ = Phase::LISTENING;
  listen_started_ms_ = Loop::now_ms();
  loop_.add_timer(kCommandAfterWakeMs + 100, [this] { update_commands(); });
  tts_url_.clear();
  tts_played_ = false;
  continue_conversation_ = false;
  duck(true);
  if (earcon) player_.play(Channel::ANNOUNCE, {kSndWake});
  api_->voice_request_start(phrase);
  mic_.set_streaming(true);
  update_ui();
}

void Controller::stop_streaming() {
  mic_.set_streaming(false);
  if (phase_ == Phase::LISTENING) phase_ = Phase::PROCESSING;
}

void Controller::on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) {
  auto get = [&](const char *k) -> std::string {
    auto it = data.find(k);
    return it == data.end() ? std::string() : it->second;
  };
  // Numbers: VaEvent (1 run-start, 2 run-end, 3/4 stt, 5/6 intent, 7/8 tts, 11/12 vad, 0 error).
  if (type != VaEvent::INTENT_PROGRESS) LOGI("voice event %u", (unsigned)type);
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
      phase_ = Phase::PROCESSING;
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
      LOGW("pipeline error: %s %s", get("code").c_str(), get("message").c_str());
      stop_streaming();
      error_shown_ = true;
      leds_.set_ui_state(UiState::ERROR);
      if (get("code") != "stt-no-text-recognized") player_.play(Channel::ANNOUNCE, {kSounds + model_.error_sound});
      break;
    case VaEvent::RUN_END:
      stop_streaming();
      if (!tts_played_) finish_interaction();
      break;
    default:
      break;
  }
}

void Controller::play_tts() {
  if (tts_played_ || tts_url_.empty()) return;
  tts_played_ = true;
  phase_ = Phase::SPEAKING;
  update_ui();
  player_.play(Channel::ANNOUNCE, {tts_url_}, [this](bool) { tts_finished(); });
}

void Controller::tts_finished() {
  if (continue_conversation_ && !st_.mic_muted && !hw_mic_muted_) {
    continue_conversation_ = false;
    start_pipeline("", false);
    return;
  }
  finish_interaction();
}

void Controller::finish_interaction() {
  mic_.set_streaming(false);
  phase_ = Phase::IDLE;
  tts_played_ = false;
  continue_conversation_ = false;
  if (!timer_ringing_ && !announcing_) duck(false);
  if (error_shown_) {
    error_shown_ = false;
    loop_.add_timer(1200, [this] { update_ui(); });
  } else {
    update_ui();
  }
}

void Controller::on_announce(const AnnounceRequest &req) {
  LOGI("announce: '%s'", req.text.c_str());
  std::vector<std::string> urls;
  if (!req.preannounce_media_id.empty()) urls.push_back(req.preannounce_media_id);
  urls.push_back(req.media_id);
  duck(true);
  phase_ = Phase::SPEAKING;
  announcing_ = true;
  update_ui();
  bool conv = req.start_conversation;
  player_.play(Channel::ANNOUNCE, urls, [this, conv](bool ok) {
    announcing_ = false;
    api_->voice_announce_finished(ok);
    if (conv && !st_.mic_muted && !hw_mic_muted_) {
      start_pipeline("", false);
    } else {
      finish_interaction();
    }
  });
}

// ---------------- timers ----------------

void Controller::on_timer_event(const TimerInfo &t) {
  LOGI("timer %u '%s' left=%u active=%d", (unsigned)t.event, t.name.c_str(), t.seconds_left, t.is_active);
  switch (t.event) {
    case TimerEvent::STARTED:
    case TimerEvent::UPDATED:
      if (t.is_active) {
        timer_end_ms_ = Loop::now_ms() + int64_t(t.seconds_left) * 1000;
        if (!timer_tick_)
          timer_tick_ = loop_.add_timer(
              0,
              [this] {
                int64_t left = timer_end_ms_ - Loop::now_ms();
                if (timer_end_ms_ == 0 || left <= 0) {
                  clock_.set_timer(0);
                  loop_.cancel_timer(timer_tick_);
                  timer_tick_ = 0;
                  return;
                }
                clock_.set_timer(uint32_t((left + 999) / 1000));
              },
              500);
      } else {  // paused
        if (timer_tick_) loop_.cancel_timer(timer_tick_);
        timer_tick_ = 0;
        clock_.set_timer(t.seconds_left);
      }
      break;
    case TimerEvent::CANCELLED:
      timer_end_ms_ = 0;
      if (timer_tick_) loop_.cancel_timer(timer_tick_);
      timer_tick_ = 0;
      clock_.set_timer(0);
      break;
    case TimerEvent::FINISHED:
      timer_end_ms_ = 0;
      if (timer_tick_) loop_.cancel_timer(timer_tick_);
      timer_tick_ = 0;
      clock_.set_timer(0);
      if (!timer_ringing_) {
        timer_ringing_ = true;
        timer_ring_started_ms_ = Loop::now_ms();
        duck(true);
        update_ui();
        ring_timer();
      }
      break;
  }
}

void Controller::ring_timer() {
  if (!timer_ringing_) return;
  if (Loop::now_ms() - timer_ring_started_ms_ > kTimerRingMaxMs) return stop_timer_ring();
  player_.play(Channel::ANNOUNCE, {kSndTimer}, [this](bool) {
    if (timer_ringing_) timer_ring_tick_ = loop_.add_timer(300, [this] { ring_timer(); });
  });
}

void Controller::stop_timer_ring() {
  timer_ringing_ = false;
  if (timer_ring_tick_) loop_.cancel_timer(timer_ring_tick_);
  timer_ring_tick_ = 0;
  player_.stop(Channel::ANNOUNCE);
  announcing_ = false;
  if (phase_ == Phase::IDLE) duck(false);
  update_ui();
}

// ---------------- media ----------------

void Controller::duck(bool on) {
  player_.set_channel_gain(Channel::MUSIC, on ? kDuckGain : 1.0f);
  if (sendspin_) sendspin_->set_duck_gain(on ? kDuckGain : 1.0f);
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
  sendspin_ = std::make_unique<SendspinService>(loop_, std::move(cfg), std::move(h));
  if (!sendspin_->start(st_.volume, st_.muted)) {
    LOGE("Sendspin player unavailable");
    sendspin_.reset();
  }
}

void Controller::set_muted(bool m) {
  st_.muted = m;
  mixer_.set_muted(m);
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
}

void Controller::on_media_command(const MediaCommandRequest &cmd) {
  if (cmd.has_media_url) {
    if (cmd.has_announcement && cmd.announcement) {
      // Media-player announcements report completion through state only.
      duck(true);
      announcing_ = true;
      phase_ = phase_ == Phase::IDLE ? Phase::SPEAKING : phase_;
      update_ui();
      player_.play(Channel::ANNOUNCE, {cmd.media_url}, [this](bool) {
        announcing_ = false;
        finish_interaction();
        publish_media();
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
        // While Sendspin plays, HA's pause/stop leave the Sendspin group (the speaker stops).
        if (ss_on && player_.state(Channel::MUSIC) != PlayState::PLAYING) sendspin_->yield_to_local();
        player_.pause(Channel::MUSIC);
        break;
      case MediaCommand::TOGGLE:
        if (player_.state(Channel::MUSIC) == PlayState::PLAYING)
          player_.pause(Channel::MUSIC);
        else if (ss_on)
          sendspin_->yield_to_local();
        else
          player_.resume(Channel::MUSIC);
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

void Controller::on_switch(SwitchId id, bool state) {
  if (id == SwitchId::MIC_MUTE) {
    st_.mic_muted = state;
    if (state && phase_ == Phase::LISTENING) {
      api_->voice_request_stop();
      finish_interaction();
    }
    update_detection();
    update_ui();
  } else if (id == SwitchId::CLOCK) {
    st_.clock_on = state;
    clock_.set_enabled(state);
  }
  api_->publish_switch(id, state);
  persist();
}

void Controller::on_number(NumberId id, float value) {
  if (id == NumberId::CLOCK_BRIGHTNESS) {
    st_.clock_brightness = std::min(100.f, std::max(0.f, value)) / 100.f;
    clock_.set_brightness(st_.clock_brightness);
    api_->publish_number(id, st_.clock_brightness * 100.f);
    persist();
  }
}

void Controller::on_light(const LightState &l) {
  st_.light = l;
  leds_.set_user_light(l.on, Rgb{uint8_t(l.r * 255), uint8_t(l.g * 255), uint8_t(l.b * 255)}, l.brightness,
                       l.effect == "None" ? "" : l.effect);
  persist();
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
  bool after_wake = phase_ == Phase::LISTENING && Loop::now_ms() - listen_started_ms_ < kCommandAfterWakeMs;
  mic_.set_commands(!st_.mic_muted && !hw_mic_muted_ && (playing || after_wake));
}

void Controller::music_pause() {
  if (player_.state(Channel::MUSIC) == PlayState::PLAYING || player_.state(Channel::MUSIC) == PlayState::LOADING)
    player_.pause(Channel::MUSIC);
  if (sendspin_ && sendspin_->streaming()) sendspin_->transport(SendspinService::Transport::PAUSE);
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
  bool after_wake = phase_ == Phase::LISTENING && Loop::now_ms() - listen_started_ms_ < kCommandAfterWakeMs;
  LOGI("voice command '%s'%s", word.c_str(), after_wake ? " (after wake word, HA pipeline cancelled)" : "");
  if (after_wake) {
    api_->voice_request_stop();
    finish_interaction();
  }
  switch (c) {
    case STOP:
      if (timer_ringing_) {
        stop_timer_ring();
      } else if (phase_ == Phase::SPEAKING || announcing_) {
        player_.stop(Channel::ANNOUNCE);
        announcing_ = false;
        continue_conversation_ = false;
        finish_interaction();
      } else {
        music_pause();
      }
      break;
    case PAUSE:
      music_pause();
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
      if (!sendspin_ || !sendspin_->transport(c == NEXT ? SendspinService::Transport::NEXT
                                                         : SendspinService::Transport::PREVIOUS))
        LOGI("'%s' ignored: no Sendspin queue to skip in", word.c_str());
      break;
    case NONE:
      break;
  }
  update_commands();
}

void Controller::update_ui() {
  update_commands();
  if (timer_ringing_) return leds_.set_ui_state(UiState::TIMER_RING);
  switch (phase_) {
    case Phase::LISTENING:
      return leds_.set_ui_state(UiState::LISTENING);
    case Phase::PROCESSING:
      return leds_.set_ui_state(UiState::THINKING);
    case Phase::SPEAKING:
      return leds_.set_ui_state(UiState::SPEAKING);
    case Phase::IDLE:
      break;
  }
  if (announcing_) return leds_.set_ui_state(UiState::SPEAKING);
  leds_.set_ui_state((st_.mic_muted || hw_mic_muted_) ? UiState::MUTED : UiState::IDLE);
}

}  // namespace cs
