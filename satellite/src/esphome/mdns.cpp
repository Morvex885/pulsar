#include "esphome/mdns.h"

#include <arpa/inet.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstring>

#include "core/log.h"

namespace cs {

namespace {

constexpr uint16_t kMdnsPort = 5353;
constexpr const char *kMdnsGroup = "224.0.0.251";
constexpr const char *kServicesEnum = "_services._dns-sd._udp.local";
constexpr uint16_t T_A = 1, T_PTR = 12, T_TXT = 16, T_SRV = 33, T_ANY = 255;
constexpr uint32_t kNoCap = UINT32_MAX;

std::string lower(std::string s) {
  for (auto &c : s)
    if (c >= 'A' && c <= 'Z') c = char(c - 'A' + 'a');
  return s;
}

void put16(std::string &o, uint16_t v) {
  o.push_back(char(v >> 8));
  o.push_back(char(v & 0xff));
}
void put32(std::string &o, uint32_t v) {
  put16(o, uint16_t(v >> 16));
  put16(o, uint16_t(v));
}

// Dotted name -> DNS labels (no compression).
void put_name(std::string &o, const std::string &dotted) {
  size_t start = 0;
  while (start < dotted.size()) {
    size_t dot = dotted.find('.', start);
    if (dot == std::string::npos) dot = dotted.size();
    size_t len = std::min<size_t>(dot - start, 63);
    o.push_back(char(len));
    o.append(dotted, start, len);
    start = dot + 1;
  }
  o.push_back(0);
}

// Reads a possibly compressed name at p[*pos], lowercased and dotted.
bool read_name(const uint8_t *p, size_t n, size_t *pos, std::string *out) {
  out->clear();
  size_t i = *pos;
  bool jumped = false;
  for (int hops = 0; hops < 32;) {
    if (i >= n) return false;
    uint8_t l = p[i];
    if ((l & 0xC0) == 0xC0) {
      if (i + 1 >= n) return false;
      if (!jumped) *pos = i + 2;
      jumped = true;
      i = (size_t(l & 0x3f) << 8) | p[i + 1];
      hops++;
      continue;
    }
    if (l == 0) {
      if (!jumped) *pos = i + 1;
      return true;
    }
    if (l > 63 || i + 1 + l > n) return false;
    if (!out->empty()) out->push_back('.');
    for (size_t k = 0; k < l; k++) {
      char c = char(p[i + 1 + k]);
      out->push_back(c >= 'A' && c <= 'Z' ? char(c - 'A' + 'a') : c);
    }
    i += 1 + l;
  }
  return false;
}

// Name of the interface carrying the IPv4 default route, if any.
std::string default_route_iface() {
  FILE *f = fopen("/proc/net/route", "r");
  if (!f) return {};
  char line[256], ifn[64];
  unsigned long dst, gw, flags;
  std::string res;
  while (fgets(line, sizeof line, f)) {
    if (sscanf(line, "%63s %lx %lx %lx", ifn, &dst, &gw, &flags) == 4 && dst == 0 && (flags & 1)) {
      res = ifn;
      break;
    }
  }
  fclose(f);
  return res;
}

}  // namespace

MdnsResponder::MdnsResponder(Loop &loop, std::string host, std::string service, uint16_t port,
                             std::vector<std::string> txt)
    : loop_(loop), host_fqdn_(host + ".local") {
  services_.push_back({service + ".local", host + "." + service + ".local", port, std::move(txt)});
}

MdnsResponder::~MdnsResponder() {
  for (uint64_t t : {poll_timer_, change_timer_, announce_timers_[0], announce_timers_[1]})
    if (t) loop_.cancel_timer(t);
  if (nl_fd_ >= 0) {
    loop_.del_fd(nl_fd_);
    close(nl_fd_);
  }
  if (fd_ >= 0) {
    announce(true);
    loop_.del_fd(fd_);
    close(fd_);
  }
}

uint32_t MdnsResponder::all_records() const {
  uint32_t m = kA;
  for (size_t i = 0; i < services_.size(); i++)
    m |= bit(i, K_PTR) | bit(i, K_SRV) | bit(i, K_TXT) | bit(i, K_ENUM);
  return m;
}

void MdnsResponder::add_service(std::string service, std::string instance, uint16_t port,
                                std::vector<std::string> txt) {
  if (services_.size() >= kMaxServices) {
    LOGE("mdns: too many services, %s not advertised", service.c_str());
    return;
  }
  services_.push_back({service + ".local", instance + "." + service + ".local", port, std::move(txt)});
  if (fd_ < 0) return;
  LOGI("mdns: advertising %s port %u", services_.back().instance_fqdn.c_str(), port);
  schedule_announcements();
}

void MdnsResponder::set_txt(const std::string &service, std::vector<std::string> txt) {
  for (auto &s : services_) {
    if (s.type_fqdn != service + ".local") continue;
    s.txt = std::move(txt);
    if (fd_ >= 0) schedule_announcements();
    return;
  }
}

// RFC 6762 8.3: at least two announcements one second apart; three, spaced 1 s and 2 s, also
// cover a Wi-Fi link that is still settling after a reconnect.
void MdnsResponder::schedule_announcements() {
  announce(false);
  for (uint64_t &t : announce_timers_)
    if (t) loop_.cancel_timer(t);
  announce_timers_[0] = loop_.add_timer(1000, [this] {
    announce_timers_[0] = 0;
    announce(false);
  });
  announce_timers_[1] = loop_.add_timer(3000, [this] {
    announce_timers_[1] = 0;
    announce(false);
  });
}

bool MdnsResponder::refresh_iface() {
  ifaddrs *ifs = nullptr;
  if (getifaddrs(&ifs) != 0) return false;
  std::string def = default_route_iface();
  const ifaddrs *best = nullptr;
  int best_score = -1;
  for (const ifaddrs *a = ifs; a; a = a->ifa_next) {
    if (!a->ifa_addr || a->ifa_addr->sa_family != AF_INET) continue;
    if (!(a->ifa_flags & IFF_UP) || (a->ifa_flags & IFF_LOOPBACK) || !(a->ifa_flags & IFF_MULTICAST)) continue;
    uint32_t ip = ntohl(reinterpret_cast<const sockaddr_in *>(a->ifa_addr)->sin_addr.s_addr);
    if ((ip >> 16) == 0xA9FE) continue;  // 169.254/16 link-local (e.g. p2p0)
    int score = 1;
    if (def == a->ifa_name) score = 2;
    if (strcmp(a->ifa_name, "wlan0") == 0) score = 3;
    if (score > best_score) {
      best_score = score;
      best = a;
    }
  }
  bool changed = false;
  if (best) {
    uint32_t ip = reinterpret_cast<const sockaddr_in *>(best->ifa_addr)->sin_addr.s_addr;
    int idx = int(if_nametoindex(best->ifa_name));
    changed = ip != ip_ || idx != ifindex_;
    if (changed) {
      char buf[INET_ADDRSTRLEN];
      inet_ntop(AF_INET, &ip, buf, sizeof buf);
      LOGI("mdns: using %s %s", best->ifa_name, buf);
    }
    ip_ = ip;
    ifindex_ = idx;
    ifname_ = best->ifa_name;
  } else {
    changed = ip_ != 0;
    if (changed) LOGI("mdns: no usable IPv4 address");
    ip_ = 0;
    ifindex_ = 0;
    ifname_.clear();
  }
  freeifaddrs(ifs);
  return changed;
}

// Re-asserted on every check: after a Wi-Fi reconnect the driver may have dropped the
// membership (or recreated wlan0) while the address stayed the same; EADDRINUSE = still joined.
void MdnsResponder::join_group() {
  if (!ifindex_) return;
  ip_mreqn mreq{};
  inet_pton(AF_INET, kMdnsGroup, &mreq.imr_multiaddr);
  mreq.imr_address.s_addr = ip_;
  mreq.imr_ifindex = ifindex_;
  bool failed = setsockopt(fd_, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq, sizeof mreq) != 0 && errno != EADDRINUSE;
  if (failed && !join_failed_) LOGW("mdns: IP_ADD_MEMBERSHIP on %s: %s", ifname_.c_str(), strerror(errno));
  if (!failed && join_failed_) LOGI("mdns: joined the multicast group on %s", ifname_.c_str());
  join_failed_ = failed;
  if (setsockopt(fd_, IPPROTO_IP, IP_MULTICAST_IF, &mreq, sizeof mreq) != 0)
    LOGD("mdns: IP_MULTICAST_IF: %s", strerror(errno));
}

void MdnsResponder::check_network(bool force_announce) {
  int old_idx = ifindex_;
  uint32_t old_ip = ip_;
  bool changed = refresh_iface();
  if (changed && old_idx) {
    ip_mreqn mreq{};
    inet_pton(AF_INET, kMdnsGroup, &mreq.imr_multiaddr);
    mreq.imr_address.s_addr = old_ip;
    mreq.imr_ifindex = old_idx;
    setsockopt(fd_, IPPROTO_IP, IP_DROP_MEMBERSHIP, &mreq, sizeof mreq);
  }
  join_group();
  if (changed || force_announce) schedule_announcements();
}

bool MdnsResponder::open_netlink() {
  nl_fd_ = socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, NETLINK_ROUTE);
  if (nl_fd_ < 0) return false;
  sockaddr_nl sa{};
  sa.nl_family = AF_NETLINK;
  sa.nl_groups = RTMGRP_LINK | RTMGRP_IPV4_IFADDR;
  if (bind(nl_fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0) {
    close(nl_fd_);
    nl_fd_ = -1;
    return false;
  }
  loop_.add_fd(nl_fd_, EPOLLIN, [this](uint32_t) { on_netlink(); });
  return true;
}

// Address changes always count. Link messages also carry wireless events (scans, roaming
// noise), so only a change of the up/running state counts. A burst (DHCP renew, reconnect) is
// handled once, after it settles.
void MdnsResponder::on_netlink() {
  alignas(nlmsghdr) char buf[8192];
  bool relevant = false, link_up = false;
  for (;;) {
    ssize_t n = recv(nl_fd_, buf, sizeof buf, 0);
    if (n < 0) {
      if (errno == EINTR) continue;
      if (errno == ENOBUFS) relevant = link_up = true;  // overflowed: something happened, recheck
      break;
    }
    // NLMSG_OK/NLMSG_NEXT spelled out with size_t: the macros mix int and __u32 (-Wsign-compare).
    size_t left = size_t(n);
    for (auto *h = reinterpret_cast<nlmsghdr *>(buf);
         left >= sizeof(nlmsghdr) && h->nlmsg_len >= sizeof(nlmsghdr) && h->nlmsg_len <= left;
         left -= std::min<size_t>(left, NLMSG_ALIGN(h->nlmsg_len)),
              h = reinterpret_cast<nlmsghdr *>(reinterpret_cast<char *>(h) + NLMSG_ALIGN(h->nlmsg_len))) {
      if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR) {
        auto *ifa = static_cast<ifaddrmsg *>(NLMSG_DATA(h));
        if (ifa->ifa_family == AF_INET) relevant = true;
      } else if (h->nlmsg_type == RTM_NEWLINK || h->nlmsg_type == RTM_DELLINK) {
        auto *ifi = static_cast<ifinfomsg *>(NLMSG_DATA(h));
        unsigned flags = h->nlmsg_type == RTM_DELLINK ? 0 : ifi->ifi_flags & (IFF_UP | IFF_RUNNING);
        auto it = std::find_if(link_flags_.begin(), link_flags_.end(),
                               [&](const auto &p) { return p.first == ifi->ifi_index; });
        if (it == link_flags_.end()) {
          link_flags_.emplace_back(ifi->ifi_index, flags);
          relevant = true;
        } else if (it->second != flags) {
          it->second = flags;
          relevant = true;
        }
        if (flags == (IFF_UP | IFF_RUNNING)) link_up = true;
      }
    }
  }
  if (!relevant) return;
  change_forced_ = change_forced_ || link_up;
  if (change_timer_) return;
  change_timer_ = loop_.add_timer(1500, [this] {
    change_timer_ = 0;
    bool force = change_forced_;
    change_forced_ = false;
    check_network(force);
  });
}

bool MdnsResponder::start() {
  fd_ = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
  if (fd_ < 0) {
    LOGE("mdns: socket: %s", strerror(errno));
    return false;
  }
  int one = 1, ttl = 255;
  setsockopt(fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  setsockopt(fd_, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
  setsockopt(fd_, IPPROTO_IP, IP_MULTICAST_TTL, &ttl, sizeof ttl);
  setsockopt(fd_, IPPROTO_IP, IP_MULTICAST_LOOP, &one, sizeof one);
  sockaddr_in sa{};
  sa.sin_family = AF_INET;
  sa.sin_port = htons(kMdnsPort);
  sa.sin_addr.s_addr = htonl(INADDR_ANY);
  if (bind(fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0) {
    // Another responder holds 5353 without SO_REUSEADDR/SO_REUSEPORT: we can still announce,
    // but cannot hear queries.
    LOGW("mdns: bind :5353 failed (%s); announcing only", strerror(errno));
    sa.sin_port = 0;
    if (bind(fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0) {
      LOGE("mdns: bind: %s", strerror(errno));
      close(fd_);
      fd_ = -1;
      return false;
    }
  }
  refresh_iface();
  join_group();
  loop_.add_fd(fd_, EPOLLIN, [this](uint32_t) { on_readable(); });

  schedule_announcements();
  if (!open_netlink()) LOGW("mdns: rtnetlink unavailable (%s); relying on the 30 s poll", strerror(errno));
  // Fallback for missed netlink events; announces only on an actual change.
  // Also re-announces every 30 s: Music Assistant (python-zeroconf) finds the Sendspin player from
  // the unsolicited announcement and does not reconnect after its own restart otherwise.
  poll_timer_ = loop_.add_timer(30000, [this] {
    check_network(false);
    announce(false);
  }, 30000);
  for (const auto &s : services_) LOGI("mdns: advertising %s port %u", s.instance_fqdn.c_str(), s.port);
  return true;
}

std::string MdnsResponder::build(uint16_t id, uint32_t answers, uint32_t additional, uint32_t ttl_cap,
                                 bool goodbye, const std::string &question) const {
  const bool legacy = ttl_cap != kNoCap;  // legacy unicast: no cache-flush bit, short TTLs
  if (!ip_) {
    answers &= ~kA;
    additional &= ~kA;
  }
  additional &= ~answers;
  std::string body;
  auto rr = [&](const std::string &name, uint16_t type, bool unique, uint32_t ttl, const std::string &rdata) {
    put_name(body, name);
    put16(body, type);
    put16(body, uint16_t((unique && !legacy ? 0x8000 : 0) | 1));
    put32(body, goodbye ? 0 : std::min(ttl, ttl_cap));
    put16(body, uint16_t(rdata.size()));
    body += rdata;
  };
  auto emit = [&](uint32_t mask) {
    int count = 0;
    if (mask & kA) {
      std::string rd(reinterpret_cast<const char *>(&ip_), 4);
      rr(host_fqdn_, T_A, true, 120, rd);
      count++;
    }
    for (size_t i = 0; i < services_.size(); i++) {
      const Service &s = services_[i];
      if (mask & bit(i, K_PTR)) {
        std::string rd;
        put_name(rd, s.instance_fqdn);
        rr(s.type_fqdn, T_PTR, false, 4500, rd);
        count++;
      }
      if (mask & bit(i, K_SRV)) {
        std::string rd;
        put16(rd, 0);
        put16(rd, 0);
        put16(rd, s.port);
        put_name(rd, host_fqdn_);
        rr(s.instance_fqdn, T_SRV, true, 120, rd);
        count++;
      }
      if (mask & bit(i, K_TXT)) {
        std::string rd;
        for (const auto &t : s.txt) {
          size_t len = std::min<size_t>(t.size(), 255);
          rd.push_back(char(len));
          rd.append(t, 0, len);
        }
        if (rd.empty()) rd.push_back(0);
        rr(s.instance_fqdn, T_TXT, true, 4500, rd);
        count++;
      }
      if (mask & bit(i, K_ENUM)) {
        std::string rd;
        put_name(rd, s.type_fqdn);
        rr(kServicesEnum, T_PTR, false, 4500, rd);
        count++;
      }
    }
    return count;
  };
  int an = emit(answers);
  int ar = emit(additional);
  std::string pkt;
  put16(pkt, id);
  put16(pkt, 0x8400);  // response, authoritative
  put16(pkt, question.empty() ? 0 : 1);
  put16(pkt, uint16_t(an));
  put16(pkt, 0);
  put16(pkt, uint16_t(ar));
  pkt += question;
  pkt += body;
  return pkt;
}

void MdnsResponder::send_to(const std::string &pkt, const sockaddr_in &to) {
  if (sendto(fd_, pkt.data(), pkt.size(), 0, reinterpret_cast<const sockaddr *>(&to), sizeof to) < 0)
    LOGD("mdns: sendto: %s", strerror(errno));
}

void MdnsResponder::announce(bool goodbye) {
  if (!ip_ && !goodbye) return;
  sockaddr_in to{};
  to.sin_family = AF_INET;
  to.sin_port = htons(kMdnsPort);
  inet_pton(AF_INET, kMdnsGroup, &to.sin_addr);
  send_to(build(0, all_records(), 0, kNoCap, goodbye, {}), to);
}

void MdnsResponder::on_readable() {
  uint8_t buf[9000];
  for (;;) {
    sockaddr_in from{};
    socklen_t fl = sizeof from;
    ssize_t n = recvfrom(fd_, buf, sizeof buf, 0, reinterpret_cast<sockaddr *>(&from), &fl);
    if (n < 0) {
      if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) LOGD("mdns: recv: %s", strerror(errno));
      if (errno == EINTR) continue;
      return;
    }
    if (n >= 12) handle_query(buf, size_t(n), from);
  }
}

void MdnsResponder::handle_query(const uint8_t *p, size_t n, const sockaddr_in &from) {
  uint16_t id = uint16_t(p[0] << 8 | p[1]);
  uint16_t flags = uint16_t(p[2] << 8 | p[3]);
  uint16_t qd = uint16_t(p[4] << 8 | p[5]);
  if (flags & 0x8000) return;  // a response, not a query
  const bool legacy = ntohs(from.sin_port) != kMdnsPort;
  const std::string host = lower(host_fqdn_);

  size_t pos = 12;
  uint32_t mc_answers = 0, mc_add = 0, uc_answers = 0, uc_add = 0;
  std::string name;
  for (unsigned q = 0; q < qd; q++) {
    if (!read_name(p, n, &pos, &name) || pos + 4 > n) return;
    uint16_t qtype = uint16_t(p[pos] << 8 | p[pos + 1]);
    uint16_t qclass = uint16_t(p[pos + 2] << 8 | p[pos + 3]);
    pos += 4;
    if ((qclass & 0x7fff) != 1 && (qclass & 0x7fff) != 255) continue;
    uint32_t a = 0, add = 0;
    bool any = qtype == T_ANY;
    for (size_t i = 0; i < services_.size(); i++) {
      const Service &s = services_[i];
      if (name == lower(s.type_fqdn) && (any || qtype == T_PTR)) {
        a |= bit(i, K_PTR);
        add |= bit(i, K_SRV) | bit(i, K_TXT) | kA;
      }
      if (name == kServicesEnum && (any || qtype == T_PTR)) a |= bit(i, K_ENUM);
      if (name == lower(s.instance_fqdn)) {
        if (any || qtype == T_SRV) {
          a |= bit(i, K_SRV);
          add |= kA;
        }
        if (any || qtype == T_TXT) a |= bit(i, K_TXT);
      }
    }
    if (name == host && (any || qtype == T_A)) a |= kA;
    if (!a) continue;
    if (legacy) {
      // One-shot resolver (e.g. plain DNS client sending to 5353): reply directly, echoing the question.
      std::string question;
      put_name(question, name);
      question.append(reinterpret_cast<const char *>(p + pos - 4), 2);
      put16(question, 1);
      send_to(build(id, a, add, 10, false, question), from);
    } else if (qclass & 0x8000) {
      uc_answers |= a;
      uc_add |= add;
    } else {
      mc_answers |= a;
      mc_add |= add;
    }
  }
  if (uc_answers) send_to(build(0, uc_answers, uc_add, kNoCap, false, {}), from);
  if (mc_answers) {
    sockaddr_in to{};
    to.sin_family = AF_INET;
    to.sin_port = htons(kMdnsPort);
    inet_pton(AF_INET, kMdnsGroup, &to.sin_addr);
    send_to(build(0, mc_answers, mc_add, kNoCap, false, {}), to);
  }
}

}  // namespace cs
