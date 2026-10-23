#include "model.h"

#include <fstream>
#include <iterator>

#include "core/log.h"

namespace cs {

namespace {

// Station Midi: aarch64, 4 mics + 4 speaker reference channels at 96 kHz, 24-LED ring + 6-LED
// logo, clock on two aw21036/aw21012 chips.
Model midi() {
  Model m;
  m.codename = "cucumber";
  m.host_prefix = "yandex-midi-";
  m.friendly_name = "Яндекс Станция Миди";
  m.model_name = "Yandex Station Midi";
  m.capture_pcm = "hw:0,2";
  m.capture_rate = 96000;
  m.capture_channels = 8;
  m.mic_channels = {0, 1, 2, 3};
  m.spk_channels = {4, 5, 6, 7};
  m.vqe_preset = "cucumber";
  m.led_prefix = "cucumber";
  m.ring = {24, 0, "/dev/fb0", "ring", "ledfb-ring", "ring"};
  m.logo = {6, 0, "/dev/fb1", "logo", "ledfb-logo", "logo"};
  m.clock_dir = "/sys/class/leds/cucumber:white:clock-";
  m.clock_seg_base = 'a';
  m.clock_pos_base = 1;
  // p3 + q3 form the colon; the rest are auxiliary dots/icons maind never lights for the time.
  m.clock_colon = {"p3", "q3"};
  m.clock_aux = {"deg", "h1", "h2", "l1", "l2", "m1", "m2", "p1", "p2", "p4", "p5", "q1", "q2", "q4", "q5"};
  m.clock_chip_current = true;
  m.keys_input = "awinic_sar";
  m.mute_input = "mute";
  m.error_sound = "ru-RU/vins_error.wav";
  // { "0": 0, "1": 49, "20": 255 }: softvol min_dB -60, so 2.6 dB per level, level 1 at -48.5 dB.
  m.volume_map = {{0, 0}, {1, 49}, {20, 255}};
  // Telink B91 with Yandex's gateway firmware (quasar.cfg zigbee.tty.port). Reset and boot are
  // GPIOZ_11 / GPIOZ_9 (devicetree node "zigbee", driver zb_control); maind drives them through
  // these files when quasar.cfg zigbee.zbControl is set, as it is on this firmware.
  m.zigbee_tty = "/dev/ttyS2";
  m.zigbee_reset = "/sys/devices/platform/zigbee/zb-reset/value";
  m.zigbee_boot = "/sys/devices/platform/zigbee/zb-boot/value";
  return m;
}

// Station Lite 2: 32-bit userland, 2 PDM mics (capture channels 0 and 2) + speaker feedback
// (channel 6) at 48 kHz on hw:0,1 (layout of multi_capture_raw in /etc/asound.conf), 3-LED ring,
// clock LEDs "plum:white:clock<A..H><0..3>" + DH/DL (colon), DM, DEGREE (quasar.cfg /leds/clock).
Model lite2() {
  Model m;
  m.codename = "plum";
  m.host_prefix = "yandex-lite2-";
  m.friendly_name = "Яндекс Станция Лайт 2";
  m.model_name = "Yandex Station Lite 2";
  m.capture_pcm = "hw:0,1";
  m.capture_rate = 48000;
  m.capture_channels = 8;
  m.mic_channels = {0, 2};
  m.spk_channels = {6};
  m.vqe_preset = "plum";
  m.led_prefix = "plum";
  m.ring = {3, 1, "/dev/fb0", "YNDX LED ring", "led_ring_fb", "ring"};
  m.clock_dir = "/sys/class/leds/plum:white:clock";
  m.clock_seg_base = 'A';
  m.clock_pos_base = 0;
  m.clock_colon = {"DH", "DL"};
  m.clock_aux = {"H0", "H1", "H2", "H3", "DEGREE"};
  m.clock_center = "DM";  // centre dot of the stock voice patterns (their extra LED 35)
  m.clock_chip_current = false;
  m.keys_input = "aw9610x";
  m.mute_input = "gpio-keys";
  m.error_sound = "ru-RU/playback_error.mp3";
  m.volume_map = {{0, 0}, {1, 49}, {20, 255}};  // quasar.cfg: the same map as the Midi
  return m;
}

}  // namespace

const Model *detect_model() {
  static const Model *model = [] () -> const Model * {
    std::ifstream f("/proc/device-tree/compatible", std::ios::binary);
    std::string compat((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    // NUL-separated list, e.g. "yandex,cucumber-dvt2\0yandex,cucumber\0amlogic, a5\0".
    for (size_t pos = 0; pos < compat.size();) {
      std::string entry = compat.c_str() + pos;
      pos += entry.size() + 1;
      if (entry == "yandex,cucumber") return new Model(midi());
      if (entry == "yandex,plum") return new Model(lite2());
    }
    for (char &c : compat)
      if (c == '\0') c = ' ';
    LOGE("unsupported device (devicetree compatible: %s)", compat.c_str());
    return nullptr;
  }();
  return model;
}

}  // namespace cs
