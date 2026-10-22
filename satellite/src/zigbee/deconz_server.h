// The Midi's Zigbee radio offered to Home Assistant (ZHA) and Zigbee2MQTT as a network
// coordinator speaking the deCONZ serial protocol (ConBee/RaspBee) over TCP: ZHA (zigpy-deconz)
// connects to socket://<ip>:<port>, Zigbee2MQTT to tcp://<ip>:<port> with adapter "deconz". The
// port is advertised over mDNS as _zigbee-coordinator._tcp (radio_type=deconz), which ZHA
// discovers by itself.
//
// Frames are SLIP (0xC0 ends, 0xDB escapes) with a 16-bit two's complement checksum, LE, after
// the frame: [command][seq][status][frame length LE16]... One client at a time; a new connection
// replaces the previous one.
//
// The host chooses the network: PAN ID, extended PAN ID, key, channel, coordinator IEEE address
// and frame counter are written before switching the network state to CONNECTED; parameters the
// host does not write keep the radio's current values. Only the global trust center link key is
// supported (no install codes).
#pragma once

#include <cstdint>
#include <deque>
#include <map>
#include <optional>
#include <string>
#include <vector>

#include "core/loop.h"
#include "zigbee/coordinator.h"

namespace cs {

class DeconzServer {
 public:
  DeconzServer(Loop &loop, ZigbeeCoordinator &coordinator, uint16_t port);
  ~DeconzServer();
  DeconzServer(const DeconzServer &) = delete;
  DeconzServer &operator=(const DeconzServer &) = delete;

  bool start();

  // ZigbeeCoordinator hooks.
  void on_received(const ApsFrame &f);
  void on_sent(uint32_t handle, uint8_t status);
  void on_changed();

  static constexpr size_t kEndpointSlots = 4;

 private:
  struct Indication {
    ApsFrame frame;
  };
  struct Request {
    uint8_t req_id;
    uint8_t dst_mode;   // deCONZ address mode: 1 group, 2 NWK, 3 IEEE
    uint64_t dst_addr;  // NWK, group or IEEE
    uint8_t dst_ep;
    uint8_t src_ep;
  };
  struct Confirm {
    Request req;
    uint8_t status;
  };

  void on_accept();
  void on_client_io(uint32_t events);
  void close_client();
  void flush();
  void on_slip(const std::vector<uint8_t> &frame);
  void handle(uint8_t cmd, uint8_t seq, const uint8_t *p, size_t n);
  void reply(uint8_t cmd, uint8_t seq, uint8_t status, const std::vector<uint8_t> &body, bool payload_length);
  void read_parameter(uint8_t seq, const uint8_t *p, size_t n);
  void write_parameter(uint8_t seq, const uint8_t *p, size_t n);
  void change_network_state(uint8_t seq, uint8_t target);
  // declared: the payload length the frame states (may differ from n, see the .cpp).
  void aps_data_request(uint8_t seq, const uint8_t *p, size_t n, size_t declared);
  uint8_t network_state() const;
  uint8_t device_state() const;
  void notify_state();
  uint8_t form_channel() const;
  // The network the host asked for, from the values it wrote over the radio's current ones.
  NetworkConfig next_network() const;
  void set_slot(size_t slot, const SimpleDescriptor &d);

  Loop &loop_;
  ZigbeeCoordinator &coord_;
  uint16_t port_;
  int listen_fd_ = -1;
  int client_fd_ = -1;
  std::vector<uint8_t> in_;   // SLIP bytes since the last 0xC0
  std::string out_;
  uint32_t generation_ = 0;   // per client: confirms of a previous client are dropped
  uint8_t last_state_ = 0xFF;  // device state last sent to the client

  // Host view of the network.
  bool online_ = true;             // the host's change_network_state: CONNECTED (true) / OFFLINE
  bool new_network_ = false;       // the host wrote network parameters the radio does not have
  // Forming a new network: wait for the old one to be erased, then form; JOINING meanwhile.
  enum class Formation { NONE, AWAIT_ERASED, FORMING } formation_ = Formation::NONE;
  NetworkConfig pending_;          // the network to form, fixed before the erase
  uint32_t channel_mask_ = 0;      // written by the host; 0 = none
  uint8_t update_id_ = 0;
  // Written by the host for the next network; unset: the current value.
  std::optional<uint16_t> pan_;
  std::optional<uint64_t> ext_pan_, ieee_;
  std::optional<NetworkKey> key_;
  uint32_t frame_counter_ = 0;
  // deCONZ endpoint slots: zigpy-deconz and zigbee-herdsman write the coordinator's endpoints by
  // slot; the coordinator gets the endpoints they hold.
  std::optional<SimpleDescriptor> slots_[kEndpointSlots];
  std::deque<Indication> indications_;
  std::deque<Confirm> confirms_;
  std::map<uint32_t, Request> requests_;  // coordinator handle -> request
};

}  // namespace cs
