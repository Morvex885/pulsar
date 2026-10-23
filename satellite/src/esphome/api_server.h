// ESPHome native API server exposing this speaker to Home Assistant as an assist satellite +
// media player + auxiliary entities. Several clients may be connected (HA, log viewers); state
// updates go to every client subscribed to states, the voice pipeline to one owner.
// All methods must be called on the Loop thread. Hooks are invoked on the Loop thread.
#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <string>
#include <vector>

#include "core/loop.h"

namespace cs {

class MdnsResponder;

struct DeviceIdentity {
  std::string name;           // hostname-like, e.g. "yandex-midi-nq3g" (<= 31 chars)
  std::string friendly_name;  // e.g. "Яндекс Станция Миди"
  std::string mac;            // "AA:BB:CC:DD:EE:FF" (wlan0)
  std::string model;          // "Yandex Station Midi"
  std::string board;          // devicetree codename, e.g. "cucumber"
  std::string manufacturer;   // "Yandex"
  std::string project_version;
};

// Mirrors aioesphomeapi VoiceAssistantEventType values.
enum class VaEvent : uint32_t {
  ERROR = 0,
  RUN_START = 1,
  RUN_END = 2,
  STT_START = 3,
  STT_END = 4,
  INTENT_START = 5,
  INTENT_END = 6,
  TTS_START = 7,
  TTS_END = 8,
  WAKE_WORD_START = 9,
  WAKE_WORD_END = 10,
  STT_VAD_START = 11,
  STT_VAD_END = 12,
  TTS_STREAM_START = 98,
  TTS_STREAM_END = 99,
  INTENT_PROGRESS = 100,
};

enum class TimerEvent : uint32_t { STARTED = 0, UPDATED = 1, CANCELLED = 2, FINISHED = 3 };

struct TimerInfo {
  TimerEvent event;
  std::string id;
  std::string name;
  uint32_t total_seconds = 0;
  uint32_t seconds_left = 0;
  bool is_active = false;
};

struct AnnounceRequest {
  std::string media_id;
  std::string text;
  std::string preannounce_media_id;
  bool start_conversation = false;
};

struct WakeWordInfo {
  std::string id;         // e.g. "alisa"
  std::string phrase;     // e.g. "Алиса"
  std::vector<std::string> languages;  // e.g. {"ru"}
};

// Mirrors MediaPlayerState / MediaPlayerCommand of api.proto.
enum class MediaState : uint32_t { NONE = 0, IDLE = 1, PLAYING = 2, PAUSED = 3, ANNOUNCING = 4, OFF = 5, ON = 6 };
enum class MediaCommand : uint32_t {
  PLAY = 0, PAUSE = 1, STOP = 2, MUTE = 3, UNMUTE = 4, TOGGLE = 5, VOLUME_UP = 6, VOLUME_DOWN = 7,
  ENQUEUE = 8, REPEAT_ONE = 9, REPEAT_OFF = 10, CLEAR_PLAYLIST = 11, TURN_ON = 12, TURN_OFF = 13,
};

struct MediaCommandRequest {
  bool has_command = false;
  MediaCommand command = MediaCommand::PLAY;
  bool has_volume = false;
  float volume = 0;
  bool has_media_url = false;
  std::string media_url;
  bool has_announcement = false;
  bool announcement = false;
};

struct LightState {
  bool on = false;
  float brightness = 1.0f;  // 0..1
  float r = 1, g = 1, b = 1;  // 0..1
  std::string effect;        // "" or "None" means no effect
};

// Auxiliary entities. Keys are stable and owned by the server.
enum class SwitchId : uint32_t { MIC_MUTE = 1, CLOCK = 2, AUTO_UPDATE = 3, NEAREST_ANSWERS = 4, VOICE_INDICATION = 5 };
enum class SensorId : uint32_t { ILLUMINANCE = 1 };
enum class BinarySensorId : uint32_t { HW_MIC_MUTE = 1 };
// 0..100 %; 100..2000 ms; 0.25..4 ×
enum class NumberId : uint32_t { CLOCK_BRIGHTNESS = 1, CLOCK_SCROLL = 2, LED_SPEED = 3 };
enum class TextId : uint32_t { CLOCK_TEXT = 1 };
enum class SelectId : uint32_t { CLOCK_FACE = 1, ZIGBEE_MODE = 2, MUSIC_VIZ = 3, CASE_COLOR = 4, CLOCK_MUSIC = 5 };
enum class ButtonId : uint32_t { ZIGBEE_JOIN = 1, ZIGBEE_LEAVE = 2 };
enum class TextSensorId : uint32_t { ZIGBEE = 1 };
enum class ActionId : uint32_t { CLOCK_SHOW_TEXT = 1, CLOCK_SHOW_FACE = 2 };  // args (text|face, seconds)
enum class EventId : uint32_t { BUTTON = 1 };             // event types: "action", "action_long", "volume_up", "volume_down"

// "Firmware" update entity (UpdateStateResponse). Mirrors api.proto UpdateCommand.
enum class UpdateCommand : uint32_t { NONE = 0, INSTALL = 1, CHECK = 2 };
struct UpdateState {
  bool in_progress = false;
  bool has_progress = false;
  float progress = 0;  // 0..100
  std::string current_version;
  std::string latest_version;  // == current_version when there is nothing newer (or not checked yet)
  std::string title;
  std::string release_url;
};

class ApiHooks {
 public:
  virtual ~ApiHooks() = default;
  // A client completed hello (called for each client).
  virtual void on_client_connected(const std::string &client_info) = 0;
  // The last client that completed hello disconnected.
  virtual void on_client_disconnected() = 0;
  // The voice pipeline owner appeared/went away. The owner is the most recent client still
  // subscribed (SubscribeVoiceAssistantRequest); when ownership moves between clients (a newer
  // subscriber, or the owner leaving while an older subscriber remains), false precedes true.
  virtual void on_voice_subscribed(bool subscribed) = 0;
  virtual void on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) = 0;
  virtual void on_announce(const AnnounceRequest &req) = 0;
  virtual void on_timer_event(const TimerInfo &t) = 0;
  // HA changed active wake words (VoiceAssistantSetConfiguration); ids are known and non-empty.
  // The reported active set changes only through ApiServer::set_active_wake_words().
  virtual void on_active_wake_words(const std::vector<std::string> &ids) = 0;
  virtual void on_media_command(const MediaCommandRequest &cmd) = 0;
  virtual void on_switch(SwitchId id, bool state) = 0;
  // transition_ms: the command's transition length, ESPHome's default (1 s) when it has none.
  virtual void on_light(const LightState &st, uint32_t transition_ms) = 0;
  virtual void on_number(NumberId id, float value) = 0;
  virtual void on_text(TextId id, const std::string &value) = 0;       // <= 64 UTF-8 characters
  virtual void on_select(SelectId id, const std::string &option) = 0;  // one of the select's options
  // User-defined action call: value is the text (<= 64 chars) or the face ("" or one of clock_faces).
  virtual void on_action(ActionId id, const std::string &value, int32_t seconds) = 0;
  // Action led_show: effect "" (solid colour) or one of ApiConfig::light_effects; color as given.
  virtual void on_led_show(const std::string &effect, const std::string &color, int32_t seconds) = 0;
  // Time from HA (GetTimeResponse); posix_tz is the POSIX TZ string (e.g. "MSK-3"), may be empty.
  virtual void on_time(uint32_t epoch_seconds, const std::string &posix_tz) {}
  // HA answered VoiceAssistantRequest(start) with error=true: no pipeline will run.
  virtual void on_voice_start_failed() {}
  // The link to HA fell too far behind to carry mic audio; the server already sent
  // VoiceAssistantRequest(start=false), so the controller should end the pipeline locally.
  virtual void on_voice_audio_overflow() {}
  // "Restart" button. The daemon should leave the loop and exit 0 (supervise.sh restarts it);
  // the call may destroy the server, so it is the last thing the server does for that message.
  virtual void on_restart() {}
  // Update entity: install the offered release or check for one now.
  virtual void on_update(UpdateCommand cmd) {}
  virtual void on_button(ButtonId id) {}
};

struct ApiConfig {
  uint16_t port = 6053;
  std::vector<WakeWordInfo> wake_words;          // available on-device wake words
  std::vector<std::string> active_wake_words;     // initially active ids
  std::vector<std::string> light_effects;         // ring light effects, besides "None"
  std::vector<std::string> clock_faces;           // Select «Clock face» options, "None" first
  std::vector<std::string> music_styles;          // Select «Music visualization» options
  std::vector<std::string> case_colors;           // Select «Case color» options, "None" first
  std::vector<std::string> clock_music_styles;    // Select «Clock music» options
  // Select «Zigbee role» options; empty: no Zigbee entities (no radio).
  std::vector<std::string> zigbee_modes;
  // File holding the Noise encryption key (base64 of 32 bytes, as in ESPHome's api: encryption:
  // key). Missing/empty file: plaintext (and all-zeros-key provisioning). HA may set or rotate it
  // with NoiseEncryptionSetKeyRequest. Empty path: encryption unsupported.
  std::string key_path;
};

class ApiServer {
 public:
  ApiServer(Loop &loop, DeviceIdentity id, ApiConfig cfg, ApiHooks &hooks);
  ~ApiServer();
  bool start();  // binds TCP port and starts mDNS advertising (_esphomelib._tcp)

  bool connected() const;         // some client completed hello
  bool voice_subscribed() const;  // a client owns the voice pipeline
  MdnsResponder *mdns();          // shared mDNS responder (null before start())

  // --- voice assistant (device -> HA) ---
  // Start a pipeline. wake_word_phrase empty => started by button / continue conversation.
  void voice_request_start(const std::string &wake_word_phrase);
  void voice_request_stop();
  // 16 kHz mono S16LE audio while the pipeline is listening.
  void voice_send_audio(const int16_t *pcm, size_t samples);
  void voice_announce_finished(bool success);
  void set_active_wake_words(const std::vector<std::string> &ids);

  // --- state publishing (sent if a client subscribed to states) ---
  void publish_media(MediaState st, float volume, bool muted);
  void publish_switch(SwitchId id, bool state);
  void publish_binary_sensor(BinarySensorId id, bool state);
  void publish_sensor(SensorId id, float value);
  void publish_number(NumberId id, float value);
  void publish_text(TextId id, const std::string &state);
  void publish_select(SelectId id, const std::string &state);
  void publish_text_sensor(TextSensorId id, const std::string &state);
  void publish_light(const LightState &st);
  void fire_event(EventId id, const std::string &event_type);
  void publish_update(const UpdateState &st);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

}  // namespace cs
