// Self-test harness for the ESPHome API server: runs ApiServer with logging hooks so a real
// client (aioesphomeapi / Home Assistant) can be pointed at it.
//
// Usage: api_selftest [port=16053] [name=pulsar-selftest] [run_seconds=120] [key_file]
//
// key_file: Noise key file as in the daemon's state dir (api_key, base64 of 32 bytes); without it
// the server is plaintext-only and encryption unsupported. A key set from the client
// (NoiseEncryptionSetKeyRequest) is written there.
//
// Behaviour:
//  - hooks print "HOOK ..." lines on stdout and echo state back (switch/number/media/light);
//  - once a client subscribes to the voice assistant it sends a start request (wake word
//    "Алиса"), streams 20 x 512-sample audio chunks, then a stop request;
//  - announcements are acknowledged with VoiceAssistantAnnounceFinished after 300 ms;
//  - every 2 s the illuminance sensor is published and a button "action" event fired;
//  - the "Restart" button stops the test (exit 0).
#include <signal.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <unistd.h>

#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>

#include "core/log.h"
#include "core/loop.h"
#include "esphome/api_server.h"

using namespace cs;

namespace {

struct TestHooks : ApiHooks {
  Loop &loop;
  ApiServer *srv = nullptr;
  MediaState media = MediaState::IDLE;
  float volume = 0.5f;
  bool muted = false;

  explicit TestHooks(Loop &l) : loop(l) {}

  void on_client_connected(const std::string &info) override { printf("HOOK connected '%s'\n", info.c_str()); }
  void on_client_disconnected() override { printf("HOOK disconnected\n"); }

  void on_voice_subscribed(bool sub) override {
    printf("HOOK voice_subscribed %d\n", sub);
    if (!sub) return;
    loop.add_timer(500, [this] {
      printf("TEST voice_request_start\n");
      srv->voice_request_start("Алиса");
      for (int i = 0; i < 20; i++) {
        loop.add_timer(50 + i * 30, [this, i] {
          int16_t pcm[512];
          for (int k = 0; k < 512; k++) pcm[k] = int16_t(1000 * std::sin((i * 512 + k) * 0.1));
          srv->voice_send_audio(pcm, 512);
        });
      }
      loop.add_timer(50 + 20 * 30 + 100, [this] {
        printf("TEST voice_request_stop\n");
        srv->voice_request_stop();
      });
    });
  }

  void on_voice_event(VaEvent type, const std::map<std::string, std::string> &data) override {
    printf("HOOK voice_event %u", unsigned(type));
    for (auto &[k, v] : data) printf(" %s=%s", k.c_str(), v.c_str());
    printf("\n");
  }

  void on_announce(const AnnounceRequest &r) override {
    printf("HOOK announce media=%s text='%s' pre=%s start_conv=%d\n", r.media_id.c_str(), r.text.c_str(),
           r.preannounce_media_id.c_str(), r.start_conversation);
    loop.add_timer(300, [this] { srv->voice_announce_finished(true); });
  }

  void on_timer_event(const TimerInfo &t) override {
    printf("HOOK timer event=%u id=%s name=%s total=%u left=%u active=%d\n", unsigned(t.event), t.id.c_str(),
           t.name.c_str(), t.total_seconds, t.seconds_left, t.is_active);
  }

  void on_active_wake_words(const std::vector<std::string> &ids) override {
    printf("HOOK active_wake_words");
    for (auto &i : ids) printf(" %s", i.c_str());
    printf("\n");
    srv->set_active_wake_words(ids);
  }

  void on_media_command(const MediaCommandRequest &c) override {
    printf("HOOK media has_cmd=%d cmd=%u has_vol=%d vol=%.2f has_url=%d url=%s has_ann=%d ann=%d\n", c.has_command,
           unsigned(c.command), c.has_volume, c.volume, c.has_media_url, c.media_url.c_str(), c.has_announcement,
           c.announcement);
    if (c.has_volume) volume = c.volume;
    if (c.has_command) {
      if (c.command == MediaCommand::PAUSE) media = MediaState::PAUSED;
      if (c.command == MediaCommand::PLAY) media = MediaState::PLAYING;
      if (c.command == MediaCommand::STOP) media = MediaState::IDLE;
      if (c.command == MediaCommand::MUTE) muted = true;
      if (c.command == MediaCommand::UNMUTE) muted = false;
    }
    if (c.has_media_url) media = MediaState::PLAYING;
    srv->publish_media(media, volume, muted);
  }

  void on_switch(SwitchId id, bool state) override {
    printf("HOOK switch %u %d\n", unsigned(id), state);
    srv->publish_switch(id, state);
  }

  void on_light(const LightState &s, uint32_t transition_ms) override {
    printf("HOOK light on=%d bri=%.2f rgb=%.2f,%.2f,%.2f effect='%s' transition=%u ms\n", s.on, s.brightness, s.r,
           s.g, s.b, s.effect.c_str(), transition_ms);
  }

  void on_number(NumberId id, float v) override {
    printf("HOOK number %u %.1f\n", unsigned(id), v);
    srv->publish_number(id, v);
  }

  void on_text(TextId id, const std::string &v) override {
    printf("HOOK text %u '%s'\n", unsigned(id), v.c_str());
    srv->publish_text(id, v);
  }

  void on_select(SelectId id, const std::string &o) override {
    printf("HOOK select %u '%s'\n", unsigned(id), o.c_str());
    srv->publish_select(id, o);
  }

  void on_action(ActionId id, const std::string &v, int32_t seconds) override {
    printf("HOOK action %u '%s' %d\n", unsigned(id), v.c_str(), int(seconds));
  }

  void on_led_show(const std::string &effect, const std::string &color, int32_t seconds) override {
    printf("HOOK led_show '%s' '%s' %d\n", effect.c_str(), color.c_str(), int(seconds));
  }

  void on_time(uint32_t epoch, const std::string &tz) override {
    printf("HOOK time epoch=%u tz='%s'\n", epoch, tz.c_str());
  }

  void on_voice_start_failed() override { printf("HOOK voice_start_failed\n"); }
  void on_voice_audio_overflow() override { printf("HOOK voice_audio_overflow\n"); }
  void on_button(ButtonId id) override {
    printf("HOOK button %u\n", unsigned(id));
    srv->publish_text_sensor(TextSensorId::ZIGBEE, id == ButtonId::ZIGBEE_JOIN ? "router: joining" : "router: not in a network");
  }
  void on_restart() override {
    printf("HOOK restart\n");
    loop.stop();
  }
};

std::string read_mac() {
  std::ifstream f("/sys/class/net/wlan0/address");
  std::string mac;
  if (f >> mac && mac.size() == 17) {
    for (auto &c : mac) c = char(toupper(c));
    return mac;
  }
  return "02:00:00:C0:FF:EE";
}

}  // namespace

int main(int argc, char **argv) {
  setvbuf(stdout, nullptr, _IOLBF, 0);
  uint16_t port = uint16_t(argc > 1 ? atoi(argv[1]) : 16053);
  std::string name = argc > 2 ? argv[2] : "pulsar-selftest";
  int run_s = argc > 3 ? atoi(argv[3]) : 120;

  Loop loop;
  TestHooks hooks(loop);
  DeviceIdentity id{name, "Яндекс Станция Миди (selftest)", read_mac(), "Yandex Station Midi", "cucumber", "Yandex", "0.1.0"};
  ApiConfig cfg;
  cfg.port = port;
  cfg.wake_words = {{"alisa", "Алиса", {"ru"}}, {"yandex", "Яндекс", {"ru"}}};
  cfg.active_wake_words = {"alisa"};
  cfg.light_effects = {"Rainbow", "Pulse"};
  cfg.clock_faces = {"None", "Smiley"};
  cfg.music_styles = {"Off", "All styles", "Spectrum"};
  cfg.case_colors = {"None", "blue"};
  cfg.clock_music_styles = {"Off", "Levels"};
  cfg.zigbee_modes = {"Coordinator", "Router"};
  if (argc > 4) cfg.key_path = argv[4];
  ApiServer srv(loop, id, cfg, hooks);
  hooks.srv = &srv;

  srv.publish_media(MediaState::IDLE, 0.5f, false);
  srv.publish_switch(SwitchId::MIC_MUTE, false);
  srv.publish_switch(SwitchId::CLOCK, true);
  srv.publish_switch(SwitchId::VOICE_INDICATION, true);
  srv.publish_number(NumberId::LED_SPEED, 1);
  srv.publish_select(SelectId::MUSIC_VIZ, "All styles");
  srv.publish_select(SelectId::CASE_COLOR, "None");
  srv.publish_select(SelectId::CLOCK_MUSIC, "Off");
  srv.publish_binary_sensor(BinarySensorId::HW_MIC_MUTE, false);
  srv.publish_number(NumberId::CLOCK_BRIGHTNESS, 50);
  LightState ls;
  ls.on = true;
  ls.brightness = 0.8f;
  ls.r = 0.0f, ls.g = 0.5f, ls.b = 1.0f;
  srv.publish_light(ls);
  srv.publish_number(NumberId::CLOCK_SCROLL, 400);
  srv.publish_text(TextId::CLOCK_TEXT, "");
  srv.publish_select(SelectId::CLOCK_FACE, "None");
  srv.publish_select(SelectId::ZIGBEE_MODE, "Coordinator");
  srv.publish_text_sensor(TextSensorId::ZIGBEE, "coordinator: PAN 0x1234, channel 15");
  if (!srv.start()) return 1;

  float lux = 100;
  loop.add_timer(
      2000,
      [&] {
        if (!srv.connected()) return;
        lux += 1.5f;
        srv.publish_sensor(SensorId::ILLUMINANCE, lux);
        srv.fire_event(EventId::BUTTON, "action");
      },
      2000);

  sigset_t ss;
  sigemptyset(&ss);
  sigaddset(&ss, SIGINT);
  sigaddset(&ss, SIGTERM);
  sigprocmask(SIG_BLOCK, &ss, nullptr);
  int sfd = signalfd(-1, &ss, SFD_NONBLOCK | SFD_CLOEXEC);
  loop.add_fd(sfd, EPOLLIN, [&](uint32_t) { loop.stop(); });
  if (run_s > 0) loop.add_timer(int64_t(run_s) * 1000, [&] { loop.stop(); });

  loop.run();
  printf("TEST exiting\n");
  loop.del_fd(sfd);
  close(sfd);
  return 0;
}
