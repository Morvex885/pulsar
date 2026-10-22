// The Midi's Zigbee radio in one of its roles:
//   off:         pulsar leaves the radio alone (whatever firmware it has keeps running); the
//                default, so an install never writes the radio on its own;
//   coordinator: pulsar's coordinator firmware (radio/coordinator, installed next to the daemon),
//                a deCONZ coordinator for ZHA and Zigbee2MQTT (ZigbeeCoordinator + DeconzServer);
//   router:      pulsar's router firmware (radio/router), a repeater in another network
//                (ZigbeeRouter).
// At start the running firmware is identified (0x0900) before the role object is built. When it is
// not the role's image (another role, another build: a pulsar update brought a new image), or the
// radio is silent (an interrupted upload), the role's image is written through the radio's
// bootloader (flasher.h). At most two uploads per start; an image the radio did not take is
// remembered (Config::failed_version) and not written again on later starts. Each firmware keeps
// its own network in its own flash area, so switching back and forth keeps them.
#pragma once

#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <thread>

#include "core/loop.h"
#include "zigbee/coordinator.h"
#include "zigbee/deconz_server.h"
#include "zigbee/flasher.h"
#include "zigbee/router.h"
#include "zigbee/telink_link.h"

namespace cs {

class ZigbeeService {
 public:
  enum class Mode { OFF, COORDINATOR, ROUTER };
  struct Config {
    RadioLines lines;
    std::string coordinator_image;  // zigbee-coordinator.ybl
    std::string router_image;       // zigbee-router.ybl
    uint16_t deconz_port = 6638;
    // Image version given up on in an earlier start (0: none); outcome(v): the role runs (v = 0)
    // or the radio did not take image version v (persist it as failed_version).
    uint32_t failed_version = 0;
    std::function<void(uint32_t failed)> outcome;
  };
  // The roles: state.conf zigbee_role= value, Home Assistant select option, firmware family
  // (top byte of the image version, radio/*/version_cfg.h).
  struct Role {
    Mode mode;
    const char *name;
    const char *label;
    uint8_t family;
  };
  static const Role kRoles[3];
  static const Role &role(Mode m);
  // state.conf value -> mode; anything else, including "coordinator" (Yandex's gateway firmware,
  // which pulsar no longer drives), is OFF: the radio is never written without a choice in HA.
  static Mode parse_mode(const std::string &name);

  ZigbeeService(Loop &loop, Config cfg, Mode mode, std::function<void()> changed);
  ~ZigbeeService();
  ZigbeeService(const ZigbeeService &) = delete;
  ZigbeeService &operator=(const ZigbeeService &) = delete;

  void start();
  Mode mode() const { return mode_; }
  bool flashing() const { return phase_ == Phase::FLASHING; }
  // One line for Home Assistant, e.g. "router: in network 0x1A2B, channel 15".
  std::string status() const;
  // Router role only: join the network that currently allows joining / leave the current one.
  void join();
  void leave();

 private:
  enum class Phase { OFF, STARTING, RUNNING, FLASHING, FAILED };
  void bring_up();
  void tear_down();
  void on_firmware(uint32_t version);
  void run();
  void flash();
  void fail(const std::string &why);
  void changed();
  const std::string &image_path() const;

  Loop &loop_;
  Config cfg_;
  Mode mode_;
  std::function<void()> changed_;
  Phase phase_ = Phase::OFF;
  std::string error_;
  float progress_ = 0;
  int flashes_ = 0;            // images written since start
  uint64_t answer_timer_ = 0;  // the radio must identify its firmware in time
  uint64_t retry_timer_ = 0;   // a silent radio is asked again
  std::thread flasher_;
  std::unique_ptr<TelinkLink> link_;
  std::unique_ptr<ZigbeeCoordinator> coord_;
  std::unique_ptr<DeconzServer> deconz_;
  std::unique_ptr<ZigbeeRouter> router_;
};

}  // namespace cs
