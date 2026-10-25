// One-off: runs deconz_script.txt against the C++ DeconzServer + ZigbeeCoordinator (rust-port-base)
// over the stub stop-and-wait link; prints what the radio got and what the client got.
#define private public
#include "zigbee/deconz_server.h"
#undef private

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstdio>
#include <fstream>
#include <memory>
#include <sstream>
#include <string>
#include <vector>

namespace cs {
void emit(const std::string &line) { printf("%s\n", line.c_str()); }
std::string hexs(const uint8_t *p, size_t n) {
  if (n == 0) return "-";
  std::string s;
  char b[3];
  for (size_t i = 0; i < n; i++) {
    snprintf(b, sizeof b, "%02x", p[i]);
    s += b;
  }
  return s;
}
}  // namespace cs
using namespace cs;

static std::vector<uint8_t> unhex(const std::string &h) {
  std::vector<uint8_t> v;
  if (h == "-") return v;
  for (size_t i = 0; i + 1 < h.size(); i += 2) v.push_back(uint8_t(std::stoi(h.substr(i, 2), nullptr, 16)));
  return v;
}

static std::vector<uint8_t> encode(uint8_t cmd, uint8_t seq, const std::vector<uint8_t> &p) {
  std::vector<uint8_t> f{cmd, seq, 0, uint8_t((5 + p.size()) & 0xFF), uint8_t((5 + p.size()) >> 8)};
  f.insert(f.end(), p.begin(), p.end());
  uint16_t sum = 0;
  for (uint8_t b : f) sum = uint16_t(sum + b);
  sum = uint16_t(~sum + 1);
  f.push_back(uint8_t(sum));
  f.push_back(uint8_t(sum >> 8));
  std::vector<uint8_t> out{0xC0};
  for (uint8_t b : f) {
    if (b == 0xC0) {
      out.push_back(0xDB);
      out.push_back(0xDC);
    } else if (b == 0xDB) {
      out.push_back(0xDB);
      out.push_back(0xDD);
    } else {
      out.push_back(b);
    }
  }
  out.push_back(0xC0);
  return out;
}

struct Bench {
  Loop loop;
  TelinkLink link{loop};
  std::unique_ptr<ZigbeeCoordinator> coord;
  std::unique_ptr<DeconzServer> server;
  int peer = -1;
  uint8_t seq = 0;
  void settle() {
    for (;;) {
      if (loop.drain()) continue;
      if (link.radio_step()) continue;
      break;
    }
  }
  void client_out() {
    if (peer < 0) return;
    usleep(1000);
    std::vector<uint8_t> got;
    uint8_t buf[4096];
    for (;;) {
      ssize_t n = recv(peer, buf, sizeof buf, MSG_DONTWAIT);
      if (n <= 0) break;
      got.insert(got.end(), buf, buf + n);
    }
    if (!got.empty()) printf("client %s\n", hexs(got.data(), got.size()).c_str());
  }
};

int main(int argc, char **argv) {
  std::ifstream in(argv[1]);
  std::string line;
  std::unique_ptr<Bench> b;
  while (std::getline(in, line)) {
    if (line.empty() || line[0] == '#') continue;
    printf("> %s\n", line.c_str());
    std::istringstream ss(line);
    std::string op;
    ss >> op;
    if (op == "new") {
      if (b && b->peer >= 0) close(b->peer);
      b.reset();
      b = std::make_unique<Bench>();
      b->coord = std::make_unique<ZigbeeCoordinator>(b->loop, b->link);
      b->server = std::make_unique<DeconzServer>(b->loop, *b->coord, 0);
      b->server->start();
      ZigbeeCoordinator::Hooks h;
      DeconzServer *srv = b->server.get();
      h.received = [srv](const ApsFrame &f) { srv->on_received(f); };
      h.sent = [srv](uint32_t handle, uint8_t status) { srv->on_sent(handle, status); };
      h.changed = [srv] { srv->on_changed(); };
      b->coord->start(std::move(h));
      b->settle();
    } else if (op == "client") {
      sockaddr_in sa{};
      socklen_t sl = sizeof sa;
      getsockname(b->server->listen_fd_, reinterpret_cast<sockaddr *>(&sa), &sl);
      sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      if (b->peer >= 0) close(b->peer);
      b->peer = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
      { int one = 1; setsockopt(b->peer, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one); }
      connect(b->peer, reinterpret_cast<sockaddr *>(&sa), sizeof sa);
      int fl = fcntl(b->server->listen_fd_, F_GETFL);
      fcntl(b->server->listen_fd_, F_SETFL, fl & ~O_NONBLOCK);
      b->server->on_accept();
      fcntl(b->server->listen_fd_, F_SETFL, fl);
      b->settle();
    } else if (op == "req" || op == "raw") {
      std::vector<uint8_t> bytes;
      if (op == "req") {
        std::string cmd, hex;
        ss >> cmd >> hex;
        b->seq++;
        bytes = encode(uint8_t(std::stoi(cmd, nullptr, 16)), b->seq, unhex(hex));
      } else {
        std::string hex;
        ss >> hex;
        bytes = unhex(hex);
      }
      if (b->server->client_fd_ >= 0) {
        send(b->peer, bytes.data(), bytes.size(), MSG_NOSIGNAL);
        pollfd pf{b->server->client_fd_, POLLIN, 0};
        poll(&pf, 1, 500);
        usleep(1000);
        b->server->on_client_io(EPOLLIN);
      }
      b->settle();
    } else if (op == "radio") {
      std::string ty, hex;
      ss >> ty >> hex;
      const uint16_t t = uint16_t(std::stoi(ty, nullptr, 16));
      if (t == 0x8907)
        b->link.inject_boot();
      else
        b->link.inject(t, unhex(hex));
      b->settle();
    } else if (op == "ack") {
      int st;
      ss >> st;
      b->link.acks_.push_back(st);
    }
    b->client_out();
    printf("client_connected %d\n", b->server->client_fd_ >= 0 ? 1 : 0);
  }
  return 0;
}
