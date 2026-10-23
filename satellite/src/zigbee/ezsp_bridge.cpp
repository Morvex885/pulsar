#include "zigbee/ezsp_bridge.h"

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <termios.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

#include "core/log.h"

namespace cs {

namespace {
constexpr size_t kMaxPending = 256 * 1024;  // a client that stops reading is dropped
}

EzspBridge::EzspBridge(Loop &loop, std::string tty, unsigned baud, uint16_t port, std::function<void()> changed)
    : loop_(loop), tty_path_(std::move(tty)), baud_(baud), port_(port), changed_(std::move(changed)) {}

EzspBridge::~EzspBridge() {
  close_client(nullptr);
  if (listen_fd_ >= 0) {
    loop_.del_fd(listen_fd_);
    ::close(listen_fd_);
  }
}

void EzspBridge::set_status(const std::string &s) {
  if (s == status_) return;
  status_ = s;
  if (changed_) changed_();
}

bool EzspBridge::start() {
  listen_fd_ = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
  if (listen_fd_ < 0) return false;
  int one = 1;
  setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  sockaddr_in sa{};
  sa.sin_family = AF_INET;
  sa.sin_addr.s_addr = htonl(INADDR_ANY);
  sa.sin_port = htons(port_);
  if (bind(listen_fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0 || listen(listen_fd_, 1) != 0) {
    LOGE("ezsp: bind/listen :%u: %s", port_, strerror(errno));
    ::close(listen_fd_);
    listen_fd_ = -1;
    set_status("error: port " + std::to_string(port_) + " busy");
    return false;
  }
  loop_.add_fd(listen_fd_, EPOLLIN, [this](uint32_t) { on_accept(); });
  set_status("EZSP bridge :" + std::to_string(port_) + ", no client");
  LOGI("ezsp: bridge %s at %u baud on tcp :%u", tty_path_.c_str(), baud_, port_);
  return true;
}

bool EzspBridge::open_tty() {
  tty_fd_ = ::open(tty_path_.c_str(), O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC);
  if (tty_fd_ < 0) {
    LOGW("ezsp: open %s: %s", tty_path_.c_str(), strerror(errno));
    return false;
  }
  termios t{};
  tcgetattr(tty_fd_, &t);
  cfmakeraw(&t);
  const speed_t sp = baud_ == 460800 ? B460800 : baud_ == 230400 ? B230400 : baud_ == 57600 ? B57600 : B115200;
  cfsetispeed(&t, sp);
  cfsetospeed(&t, sp);
  t.c_cflag |= CLOCAL | CREAD;
  t.c_cflag &= ~CRTSCTS;
  t.c_cc[VMIN] = 0;
  t.c_cc[VTIME] = 0;
  tcsetattr(tty_fd_, TCSANOW, &t);
  tcflush(tty_fd_, TCIOFLUSH);
  loop_.add_fd(tty_fd_, EPOLLIN, [this](uint32_t ev) { on_tty(ev); });
  return true;
}

void EzspBridge::on_accept() {
  sockaddr_in sa{};
  socklen_t sl = sizeof sa;
  const int fd = accept4(listen_fd_, reinterpret_cast<sockaddr *>(&sa), &sl, SOCK_NONBLOCK | SOCK_CLOEXEC);
  if (fd < 0) return;
  if (client_fd_ >= 0) close_client("replaced");
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one);
  char ip[INET_ADDRSTRLEN] = "?";
  inet_ntop(AF_INET, &sa.sin_addr, ip, sizeof ip);
  peer_ = ip;
  if (!open_tty()) {
    ::close(fd);
    set_status("error: cannot open " + tty_path_);
    return;
  }
  client_fd_ = fd;
  to_client_.clear();
  to_tty_.clear();
  bytes_in_ = bytes_out_ = 0;
  loop_.add_fd(client_fd_, EPOLLIN, [this](uint32_t ev) { on_client(ev); });
  LOGI("ezsp: client %s connected", ip);
  set_status("client " + peer_ + " connected");
}

void EzspBridge::close_client(const char *why) {
  if (client_fd_ >= 0) {
    loop_.del_fd(client_fd_);
    ::close(client_fd_);
    client_fd_ = -1;
  }
  if (tty_fd_ >= 0) {
    loop_.del_fd(tty_fd_);
    ::close(tty_fd_);
    tty_fd_ = -1;
  }
  to_client_.clear();
  to_tty_.clear();
  if (why) {
    LOGI("ezsp: client %s gone (%s), %llu bytes to the radio, %llu from it", peer_.c_str(), why,
         (unsigned long long)bytes_out_, (unsigned long long)bytes_in_);
    set_status("EZSP bridge :" + std::to_string(port_) + ", no client");
  }
}

// Writes as much of buf as fd takes; the rest waits for EPOLLOUT. which: 0 = client, 1 = tty.
void EzspBridge::flush(int fd, std::vector<uint8_t> &buf, int which) {
  while (!buf.empty()) {
    const ssize_t n = ::write(fd, buf.data(), buf.size());
    if (n > 0) {
      buf.erase(buf.begin(), buf.begin() + n);
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
    return close_client(strerror(errno));
  }
  if (buf.size() > kMaxPending) return close_client("peer too slow");
  loop_.mod_fd(fd, EPOLLIN | (buf.empty() ? 0u : unsigned(EPOLLOUT)));
  (void)which;
}

void EzspBridge::on_client(uint32_t events) {
  if (events & (EPOLLERR | EPOLLHUP)) return close_client("closed");
  if (events & EPOLLOUT) {
    flush(client_fd_, to_client_, 0);
    if (client_fd_ < 0) return;
  }
  if (!(events & EPOLLIN)) return;
  uint8_t buf[2048];
  const ssize_t n = ::read(client_fd_, buf, sizeof buf);
  if (n == 0) return close_client("closed");
  if (n < 0) {
    if (errno == EAGAIN || errno == EINTR) return;
    return close_client(strerror(errno));
  }
  bytes_out_ += uint64_t(n);
  to_tty_.insert(to_tty_.end(), buf, buf + n);
  flush(tty_fd_, to_tty_, 1);
}

void EzspBridge::on_tty(uint32_t events) {
  if (events & EPOLLERR) return close_client("serial error");
  if (events & EPOLLOUT) {
    flush(tty_fd_, to_tty_, 1);
    if (client_fd_ < 0) return;
  }
  if (!(events & EPOLLIN)) return;
  uint8_t buf[2048];
  const ssize_t n = ::read(tty_fd_, buf, sizeof buf);
  if (n <= 0) return;
  bytes_in_ += uint64_t(n);
  to_client_.insert(to_client_.end(), buf, buf + n);
  flush(client_fd_, to_client_, 0);
}

}  // namespace cs
