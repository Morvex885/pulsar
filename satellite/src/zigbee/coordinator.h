// Zigbee coordinator on pulsar's coordinator firmware (radio/coordinator, a network co-processor):
// the chip runs the stack and the trust center, the host chooses the network (PAN ID, extended
// PAN ID, key, channel, coordinator IEEE address, frame counter) and gets every APS frame, ZDO
// included. Protocol: stock zbhci framing, commands 0x0Axx / 0x8Axx with little-endian fields
// (radio/coordinator/coordinator.c has the layouts). ZDO addressed to the coordinator itself is
// answered here.
#pragma once

#include <array>
#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "core/loop.h"
#include "zigbee/telink_link.h"

namespace cs {

struct ApsFrame {
  uint16_t src = 0, dst = 0;  // NWK addresses (dst: group when `group`); dst >= 0xFFF8 is a broadcast
  bool group = false;
  uint8_t src_ep = 0, dst_ep = 0;
  uint16_t profile = 0, cluster = 0;
  std::vector<uint8_t> data;
  uint8_t lqi = 0;
  int8_t rssi = 0;
  uint8_t tx_options = 0;  // sending: deCONZ transmit options (0x04 = APS ACK)
  uint8_t radius = 0;      // sending: 0 = default
};

struct SimpleDescriptor {
  uint8_t endpoint = 0;
  uint16_t profile = 0, device = 0;
  uint8_t version = 0;
  std::vector<uint16_t> in, out;
  bool operator==(const SimpleDescriptor &) const = default;
};
// [endpoint][profile LE][device LE][version][n in][in…][n out][out…]: the layout of ZDO
// Simple_Desc_rsp, deCONZ endpoint parameters and the radio's 0x0A04.
void put_descriptor(std::vector<uint8_t> &v, const SimpleDescriptor &d);
bool get_descriptor(const uint8_t *p, size_t n, SimpleDescriptor &d);

using NetworkKey = std::array<uint8_t, 16>;

struct ZigbeeNetwork {
  bool formed = false;
  bool forming = false;
  uint16_t pan = 0xFFFF, nwk = 0xFFFE;
  uint64_t ext_pan = 0, ieee = 0;
  uint8_t channel = 0;
  uint8_t update_id = 0;
  NetworkKey key{};
  uint8_t key_seq = 0;
  uint32_t frame_counter = 0;  // outgoing NWK frame counter
};

// A network to form.
struct NetworkConfig {
  uint8_t channel = 15;
  uint16_t pan = 0xFFFF;  // 0xFFFF: the stack picks
  uint64_t ext_pan = 0;   // 0: the coordinator's IEEE address
  NetworkKey key{};       // all zero: the stack picks
  uint64_t ieee = 0;      // 0: the radio's own
  uint32_t frame_counter = 0;
  uint8_t update_id = 0;
};

class ZigbeeCoordinator {
 public:
  struct Hooks {
    std::function<void(const ApsFrame &)> received;
    // Delivery result of send(): 0 = delivered (APS ACK for unicasts), else a Zigbee status.
    std::function<void(uint32_t handle, uint8_t status)> sent;
    std::function<void()> changed;  // ready() or network() changed
  };
  static constexpr uint8_t kStatusNoAck = 0xA7;  // APS NO_ACK

  // link: open, the running firmware identified as pulsar's coordinator.
  ZigbeeCoordinator(Loop &loop, TelinkLink &link);
  ~ZigbeeCoordinator();
  ZigbeeCoordinator(const ZigbeeCoordinator &) = delete;
  ZigbeeCoordinator &operator=(const ZigbeeCoordinator &) = delete;

  void start(Hooks hooks);
  bool ready() const { return ready_; }  // the radio reported its network state
  const ZigbeeNetwork &network() const { return net_; }
  size_t pending() const { return sent_.size(); }

  // Queues an APS data request (dst by NWK address or group, or by IEEE address when
  // dst_ieee != 0); the result arrives through Hooks::sent. false: it cannot be sent at all.
  bool send(uint32_t handle, ApsFrame f, uint64_t dst_ieee = 0);
  // Opens (seconds > 0) or closes joining on the coordinator itself.
  void permit_join(uint8_t seconds);
  // The coordinator's endpoints, by number.
  void set_endpoint(const SimpleDescriptor &d);
  void remove_endpoint(uint8_t endpoint);
  // Erases the network in the radio (devices must join again).
  void factory_reset();
  // Forms a network (none may exist); done(true) once it is up, done(false) when the radio
  // refused or failed.
  void form(const NetworkConfig &config, std::function<void(bool ok)> done);
  // Moves the formed network to another channel (broadcast Mgmt_NWK_Update_req).
  void change_channel(uint8_t channel, uint8_t update_id);

 private:
  struct Pending {
    uint32_t handle;
    int64_t deadline;
  };
  struct Scan {
    uint32_t handle;
    uint8_t tsn;
    uint32_t mask;
  };

  void on_boot();
  void on_frame(uint16_t type, const uint8_t *p, size_t n);
  void on_state(const uint8_t *p, size_t n);
  void on_indication(const uint8_t *p, size_t n);
  void on_event(const uint8_t *p, size_t n);
  void on_scan(const uint8_t *p, size_t n);
  void on_confirm(uint8_t radio_handle, uint8_t status);
  void finish_form(bool ok);
  void push_endpoint(const SimpleDescriptor &d);
  void sweep();
  void set_ready(bool r);
  void post_sent(uint32_t handle, uint8_t status);
  void deliver_zdo(uint16_t src, uint16_t rsp_cluster, std::vector<uint8_t> payload);
  void local_zdo(uint32_t handle, const ApsFrame &f);
  void energy_scan(uint32_t handle, uint8_t tsn, uint32_t mask, uint8_t exponent);

  Loop &loop_;
  TelinkLink &link_;
  Hooks hooks_;
  bool started_ = false;
  bool ready_ = false;
  ZigbeeNetwork net_;
  std::map<uint8_t, SimpleDescriptor> endpoints_;
  std::map<uint8_t, Pending> sent_;  // by the radio's 8-bit handle: 0x8A10 due
  uint8_t next_handle_ = 0;
  std::optional<Scan> scan_;
  // Formation in progress: the radio accepted 0x0A01 (acked), result due.
  std::function<void(bool)> form_done_;
  bool form_acked_ = false;
  NetworkConfig form_config_;
  uint64_t sweep_timer_ = 0;
  int sweeps_ = 0;
  // Posted callbacks and one-shot timers outlive the object when ZigbeeService replaces it.
  std::shared_ptr<int> alive_ = std::make_shared<int>(0);
};

}  // namespace cs
