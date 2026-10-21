// Single-threaded epoll event loop with timers and a thread-safe post queue.
#pragma once

#include <cstdint>
#include <deque>
#include <functional>
#include <map>
#include <mutex>
#include <unordered_map>

namespace cs {

class Loop {
 public:
  using FdCallback = std::function<void(uint32_t events)>;
  using Task = std::function<void()>;

  Loop();
  ~Loop();
  Loop(const Loop &) = delete;
  Loop &operator=(const Loop &) = delete;

  // fd callbacks run on the loop thread. events are EPOLL* bits.
  void add_fd(int fd, uint32_t events, FdCallback cb);
  void mod_fd(int fd, uint32_t events);
  void del_fd(int fd);

  // Timers run on the loop thread. Returns a non-zero id.
  uint64_t add_timer(int64_t delay_ms, Task cb, int64_t repeat_ms = 0);
  void cancel_timer(uint64_t id);

  // Thread-safe: queue fn to run on the loop thread.
  void post(Task fn);

  void run();
  void stop();

  static int64_t now_ms();  // CLOCK_MONOTONIC

 private:
  struct Timer {
    int64_t due;
    int64_t repeat;
    Task cb;
  };
  void drain_posted();
  int64_t next_timeout() const;
  void fire_timers();

  int epfd_ = -1;
  int wakefd_ = -1;
  bool running_ = false;
  std::unordered_map<int, FdCallback> fds_;
  std::map<uint64_t, Timer> timers_;
  uint64_t next_timer_id_ = 1;
  std::mutex post_mu_;
  std::deque<Task> posted_;
};

}  // namespace cs
