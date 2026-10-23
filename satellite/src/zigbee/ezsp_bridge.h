// Silicon Labs EZSP NCP (Station 2's Zigbee radio) offered as a TCP serial port: Zigbee2MQTT
// (adapter "ember": tcp://<ip>:<port>) and ZHA (radio_type ezsp, socket://<ip>:<port>) talk ASH /
// EZSP to the NCP themselves; the bridge only copies bytes. The UART is opened while a client is
// connected, so the stock Yandex stack and the bridge never hold it at the same time. One client
// at a time; a new connection replaces the previous one.
#pragma once

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

#include "core/loop.h"

namespace cs {

class EzspBridge {
 public:
  // changed: called when status() changes.
  EzspBridge(Loop &loop, std::string tty, unsigned baud, uint16_t port, std::function<void()> changed);
  ~EzspBridge();
  EzspBridge(const EzspBridge &) = delete;
  EzspBridge &operator=(const EzspBridge &) = delete;

  bool start();  // binds the TCP port
  std::string status() const { return status_; }

 private:
  void on_accept();
  void on_client(uint32_t events);
  void on_tty(uint32_t events);
  void close_client(const char *why);
  bool open_tty();
  void set_status(const std::string &s);
  void flush(int fd, std::vector<uint8_t> &buf, int which);

  Loop &loop_;
  std::string tty_path_;
  unsigned baud_;
  uint16_t port_;
  std::function<void()> changed_;
  int listen_fd_ = -1, client_fd_ = -1, tty_fd_ = -1;
  std::vector<uint8_t> to_client_, to_tty_;  // not yet written
  std::string status_, peer_;
  uint64_t bytes_in_ = 0, bytes_out_ = 0;
};

}  // namespace cs
