// ESPHome native API server (plaintext framing) exposing this speaker to Home Assistant
// as an assist satellite + media player + auxiliary entities.
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
enum class SwitchId : uint32_t { MIC_MUTE = 1, CLOCK = 2 };
enum class SensorId : uint32_t { ILLUMINANCE = 1 };
enum class BinarySensorId : uint32_t { HW_MIC_MUTE = 1 };
enum class NumberId : uint32_t { CLOCK_BRIGHTNESS = 1 };  // 0..100 %
enum class EventId : uint32_t { BUTTON = 1 };             // event types: "action", "action_long", "volume_up", "volume_down"

class ApiHooks {
 public:
  virtual ~ApiHooks() = default;
  virtual void on_client_connected(const std::string &client_info) = 0;
  virtual void on_client_disconnected() = 0;
  // HA subscribed/unsubscribed to voice assistant (SubscribeVoiceAssistantRequest).
  virtual void on_voice_subscribed(bool subscribed) = 0;
  virtual void on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) = 0;
  virtual void on_announce(const AnnounceRequest &req) = 0;
  virtual void on_timer_event(const TimerInfo &t) = 0;
  // HA changed active wake words (VoiceAssistantSetConfiguration).
  virtual void on_active_wake_words(const std::vector<std::string> &ids) = 0;
  virtual void on_media_command(const MediaCommandRequest &cmd) = 0;
  virtual void on_switch(SwitchId id, bool state) = 0;
  virtual void on_light(const LightState &st) = 0;
  virtual void on_number(NumberId id, float value) = 0;
  // Time from HA (GetTimeResponse); posix_tz is the POSIX TZ string (e.g. "MSK-3"), may be empty.
  virtual void on_time(uint32_t epoch_seconds, const std::string &posix_tz) {}
};

struct ApiConfig {
  uint16_t port = 6053;
  std::vector<WakeWordInfo> wake_words;          // available on-device wake words
  std::vector<std::string> active_wake_words;     // initially active ids
  std::vector<std::string> light_effects;         // ring light effects, besides "None"
};

class ApiServer {
 public:
  ApiServer(Loop &loop, DeviceIdentity id, ApiConfig cfg, ApiHooks &hooks);
  ~ApiServer();
  bool start();  // binds TCP port and starts mDNS advertising (_esphomelib._tcp)

  bool connected() const;         // an HA client completed hello
  bool voice_subscribed() const;  // HA subscribed to voice assistant
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
  void publish_light(const LightState &st);
  void fire_event(EventId id, const std::string &event_type);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

}  // namespace cs
