// Per-model hardware description of the supported Yandex Stations. The model is detected once
// from the devicetree ("yandex,<codename>" in /proc/device-tree/compatible).
#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace cs {

struct Model {
  std::string codename;       // devicetree codename: "cucumber", "plum", "station-mini2"
  std::string host_prefix;    // ESPHome node name prefix, the MAC suffix is appended
  std::string friendly_name;  // shown in Home Assistant / Music Assistant
  std::string model_name;

  // Capture: interleaved S16 frames from capture_pcm; VQE gets mic_channels / spk_channels.
  std::string capture_pcm;
  unsigned capture_rate = 0, capture_channels = 0;
  std::vector<unsigned> mic_channels, spk_channels;
  std::string vqe_preset;

  // LED ring (+ optional logo), each an Nx1 BGR framebuffer behind an ledfb trigger, with
  // per-channel sysfs LEDs "<led_prefix>:<color>:<ring|logo><index>" as fallback.
  struct LedStrip {
    int count = 0;
    int first_index = 0;      // sysfs numbering starts at 0 (Midi) or 1 (Lite 2)
    std::string fb_dev;       // "/dev/fb0"
    std::string fb_name;      // expected /sys/class/graphics/fbN/name
    std::string trigger;      // LED trigger that routes the framebuffer to the LEDs
    std::string sysfs_kind;   // "ring" / "logo"
  };
  std::string led_prefix;
  LedStrip ring, logo;        // logo.count == 0: no logo

  // 4-digit 7-segment clock: "<clock_dir><segment><position>" sysfs LEDs.
  std::string clock_dir;      // e.g. "/sys/class/leds/cucumber:white:clock-"
  char clock_seg_base = 'a';  // name of segment a ('a' or 'A')
  int clock_pos_base = 1;     // name of the leftmost digit position
  std::vector<std::string> clock_colon, clock_aux;  // colon LEDs; LEDs kept dark
  std::string clock_center;  // LED at g height between digits 1 and 2 ("" = none)
  // true: segments switch 0/255, dimming via the driver chips' "current" attribute;
  // false: dimming via the per-segment brightness value itself.
  bool clock_chip_current = true;

  // Playback PCM of /etc/asound.conf (dmix + the softvol "Master Playback Volume" that Mixer
  // drives): "speakers" on the Midi and the Lite 2, "default" on the Mini 2.
  std::string playback_pcm = "speakers";

  std::string keys_input, mute_input;  // evdev device names
  // Mic mute on a plain GPIO instead of an evdev switch (Mini 2): global GPIO number, 0 = none.
  // The button is momentary, but the mic circuit latches the mute and holds the line grounded, so
  // value 0 = muted. Polled from sysfs, as maind does (its MuteButtonsImpl).
  int mute_gpio = 0;
  // Earcons, relative to the same directory. The Mini 2 firmware has no MP3 decoder in its
  // GStreamer and ships all of these as .opus, so each model names its own files.
  std::string error_sound;
  std::string wake_sound = "common/spotter_start.mp3";
  std::string cancel_sound = "common/spotter_cancel.mp3";
  std::string timer_sound = "common/timer.wav";

  // Stock volume curve: platform level -> raw value (0..255) of the softvol "Master Playback
  // Volume" on the speakers PCM, piecewise linear between the points; the first point is level 0
  // (raw 0), the last one the top level. quasar.cfg volume_capability.volume_manager_settings
  // .platformToAlsaMap; the platform level steps by 5 % (changeStepPercent).
  struct VolumePoint {
    int level, raw;
  };
  std::vector<VolumePoint> volume_map;
  // The stock firmware never sets this curve's raw values between 0 and its lowest step (the
  // second point): any audible level jumps straight to that step. False for a plain line from
  // level 0 to the top (Mini 2, whose set_volume.sh takes the percentage directly).
  bool volume_min_step = true;

  // Zigbee radio: Telink co-processor on a UART; its reset and boot lines are GPIO value files
  // (active low, 1 = asserted) exported by the kernel's zb_control driver. zigbee_tty empty: none.
  std::string zigbee_tty;
  std::string zigbee_reset, zigbee_boot;
};

// The detected model; nullptr on unsupported hardware.
const Model *detect_model();

}  // namespace cs
