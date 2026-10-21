#include "core/loop.h"

#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <time.h>
#include <unistd.h>

#include <cerrno>
#include <vector>

#include "core/log.h"

namespace cs {

Loop::Loop() {
  epfd_ = epoll_create1(EPOLL_CLOEXEC);
  wakefd_ = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
  add_fd(wakefd_, EPOLLIN, [this](uint32_t) {
    uint64_t v;
    while (read(wakefd_, &v, sizeof v) == sizeof v) {
    }
    drain_posted();
  });
}

Loop::~Loop() {
  if (wakefd_ >= 0) close(wakefd_);
  if (epfd_ >= 0) close(epfd_);
}

int64_t Loop::now_ms() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return int64_t(ts.tv_sec) * 1000 + ts.tv_nsec / 1000000;
}

void Loop::add_fd(int fd, uint32_t events, FdCallback cb) {
  epoll_event ev{};
  ev.events = events;
  ev.data.fd = fd;
  fds_[fd] = std::move(cb);
  if (epoll_ctl(epfd_, EPOLL_CTL_ADD, fd, &ev) != 0) LOGE("epoll add fd=%d: %s", fd, strerror(errno));
}

void Loop::mod_fd(int fd, uint32_t events) {
  epoll_event ev{};
  ev.events = events;
  ev.data.fd = fd;
  epoll_ctl(epfd_, EPOLL_CTL_MOD, fd, &ev);
}

void Loop::del_fd(int fd) {
  epoll_ctl(epfd_, EPOLL_CTL_DEL, fd, nullptr);
  fds_.erase(fd);
}

uint64_t Loop::add_timer(int64_t delay_ms, Task cb, int64_t repeat_ms) {
  uint64_t id = next_timer_id_++;
  timers_[id] = Timer{now_ms() + delay_ms, repeat_ms, std::move(cb)};
  return id;
}

void Loop::cancel_timer(uint64_t id) { timers_.erase(id); }

void Loop::post(Task fn) {
  {
    std::lock_guard<std::mutex> lk(post_mu_);
    posted_.push_back(std::move(fn));
  }
  uint64_t one = 1;
  (void)!write(wakefd_, &one, sizeof one);
}

void Loop::drain_posted() {
  std::deque<Task> q;
  {
    std::lock_guard<std::mutex> lk(post_mu_);
    q.swap(posted_);
  }
  for (auto &fn : q) fn();
}

int64_t Loop::next_timeout() const {
  if (timers_.empty()) return -1;
  int64_t now = now_ms(), best = INT64_MAX;
  for (auto &[id, t] : timers_) best = std::min(best, t.due);
  return best <= now ? 0 : best - now;
}

void Loop::fire_timers() {
  int64_t now = now_ms();
  std::vector<uint64_t> due;
  for (auto &[id, t] : timers_)
    if (t.due <= now) due.push_back(id);
  for (uint64_t id : due) {
    auto it = timers_.find(id);
    if (it == timers_.end()) continue;  // cancelled by an earlier callback
    Task cb = it->second.cb;
    if (it->second.repeat > 0)
      it->second.due = now + it->second.repeat;
    else
      timers_.erase(it);
    cb();
  }
}

void Loop::run() {
  running_ = true;
  epoll_event evs[32];
  while (running_) {
    int64_t to = next_timeout();
    int n = epoll_wait(epfd_, evs, 32, to < 0 ? -1 : int(std::min<int64_t>(to, 60000)));
    if (n < 0 && errno != EINTR) {
      LOGE("epoll_wait: %s", strerror(errno));
      break;
    }
    for (int i = 0; i < n; i++) {
      auto it = fds_.find(evs[i].data.fd);
      if (it == fds_.end()) continue;
      FdCallback cb = it->second;  // copy: callback may del_fd itself
      cb(evs[i].events);
    }
    fire_timers();
  }
}

void Loop::stop() {
  running_ = false;
  uint64_t one = 1;
  (void)!write(wakefd_, &one, sizeof one);
}

}  // namespace cs
