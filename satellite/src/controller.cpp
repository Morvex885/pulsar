#include "controller.h"

#include <algorithm>
#include <cmath>
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
// Longest wait for the next pipeline event before the dialog is given up locally. HA's voice
// command segmenter ends speech capture after at most 15 s of audio. Between INTENT_START and
// INTENT_END nothing arrives (ESPHome forwards INTENT_PROGRESS only once, when TTS streaming
// starts), and conversation agents (LLMs with tool calls) may think for a minute or more.
constexpr int64_t kListenTimeoutMs = 30'000;
constexpr int64_t kProcessTimeoutMs = 120'000;
constexpr uint16_t kSendspinPort = 8928;

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
    f << "volume2=" << volume << "\nvolume=" << volume_legacy << "\nmuted=" << muted << "\nmic_muted=" << mic_muted
      << "\nclock_on=" << clock_on << "\nclock_brightness=" << clock_brightness
      << "\nclock_scroll_ms=" << clock_scroll_ms << "\nlight_on=" << light.on
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
      mixer_(model),
      leds_(loop, model),
      clock_(loop, model),
      input_(loop, model),
      light_(loop) {}

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
  cfg.light_effects = {"Rainbow", "Breathe"};
  cfg.clock_faces = {"None"};
  cfg.clock_faces.insert(cfg.clock_faces.end(), Clock::face_names().begin(), Clock::face_names().end());
  const auto slash = state_path_.rfind('/');
  cfg.key_path = (slash == std::string::npos ? std::string(".") : state_path_.substr(0, slash)) + "/api_key";
  api_ = std::make_unique<ApiServer>(loop_, id, cfg, *this);

  // Hardware first: UI feedback should work even if HA is not connected.
  if (!leds_.open()) LOGW("LEDs unavailable");
  if (!clock_.open()) LOGW("clock unavailable");
  clock_.set_enabled(st_.clock_on);
  clock_.set_brightness(st_.clock_brightness);
  clock_.set_scroll_ms(st_.clock_scroll_ms);
  leds_.set_user_light(st_.light.on,
                       Rgb{uint8_t(st_.light.r * 255), uint8_t(st_.light.g * 255), uint8_t(st_.light.b * 255)},
                       st_.light.brightness, st_.light.effect);

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

  mic_.on_wake([this](const std::string &phrase) { on_wake(phrase); });
  mic_.on_command([this](const std::string &word) { on_command(word); });
  mic_.on_audio([this](const int16_t *pcm, size_t n) {
    if (phase_ == Phase::LISTENING) api_->voice_send_audio(pcm, n);
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
  update_ui();
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
  api_->publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, hw_mic_muted_);
  api_->publish_number(NumberId::CLOCK_BRIGHTNESS, st_.clock_brightness * 100.f);
  api_->publish_number(NumberId::CLOCK_SCROLL, float(st_.clock_scroll_ms));
  api_->publish_text(TextId::CLOCK_TEXT, clock_text_);
  api_->publish_select(SelectId::CLOCK_FACE, clock_face_);
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

void Controller::on_wake(const std::string &phrase) {
  if (st_.mic_muted || hw_mic_muted_) return;
  if (timer_ringing_) return stop_timer_ring();  // the wake word works as a stop word while ringing
  if (phase_ == Phase::LISTENING || phase_ == Phase::PROCESSING) return;
  if (phase_ == Phase::SPEAKING || announcing_) interrupt_speech();  // barge-in: cut the answer short
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
  mic_.set_streaming(true);
  arm_watchdog();
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
      LOGW("pipeline error: %s %s", get("code").c_str(), get("message").c_str());
      stop_streaming();
      error_shown_ = true;
      leds_.set_ui_state(UiState::ERROR);
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
  mic_.set_streaming(false);
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
  leds_.set_ui_state(UiState::ERROR);
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
  sendspin_ = std::make_unique<SendspinService>(loop_, std::move(cfg), std::move(h));
  if (!sendspin_->start(st_.volume, st_.muted)) {
    LOGE("Sendspin player unavailable");
    sendspin_.reset();
  }
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

void Controller::on_restart() {
  LOGW("restart requested from Home Assistant");
  loop_.stop();
}

void Controller::on_switch(SwitchId id, bool state) {
  if (id == SwitchId::MIC_MUTE) {
    st_.mic_muted = state;
    if (state && phase_ == Phase::LISTENING) {
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
  } else if (id == NumberId::CLOCK_SCROLL) {
    st_.clock_scroll_ms = std::clamp(int(std::lround(value / 50.f)) * 50, 100, 2000);
    clock_.set_scroll_ms(st_.clock_scroll_ms);
    api_->publish_number(id, float(st_.clock_scroll_ms));
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
  clock_face_ = option;
  clock_.set_face(clock_face_index(option));
  api_->publish_select(id, option);
  if (option != "None" && !clock_text_.empty()) {
    clock_text_.clear();
    api_->publish_text(TextId::CLOCK_TEXT, clock_text_);
  }
}

void Controller::on_action(ActionId id, const std::string &value, int32_t seconds) {
  const bool text = id == ActionId::CLOCK_SHOW_TEXT;
  LOGI("clock: temporary %s '%s' for %d s", text ? "text" : "face", value.c_str(), int(seconds));
  if (text) clock_.show_temporary_text(value, seconds);
  else clock_.show_temporary_face(clock_face_index(value), seconds);
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
