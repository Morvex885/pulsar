#pragma once
#include <cstdint>
#include <deque>
#include <functional>
#include <map>
#include <vector>
namespace cs {
class Loop {
 public:
  using Task = std::function<void()>;
  void add_fd(int, uint32_t, std::function<void(uint32_t)>) {}
  void mod_fd(int, uint32_t) {}
  void del_fd(int) {}
  uint64_t add_timer(int64_t delay_ms, Task cb, int64_t repeat_ms = 0) {
    uint64_t id = next_++;
    timers_[id] = Timer{now_ + delay_ms, repeat_ms, std::move(cb)};
    return id;
  }
  void cancel_timer(uint64_t id) { timers_.erase(id); }
  void post(Task fn) { posted_.push_back(std::move(fn)); }
  static int64_t now_ms() { return now_; }
  static inline int64_t now_ = 1000000;
  struct Timer { int64_t due, repeat; Task cb; };
  std::map<uint64_t, Timer> timers_;
  uint64_t next_ = 1;
  std::deque<Task> posted_;
  bool drain() {
    if (posted_.empty()) return false;
    std::deque<Task> q;
    q.swap(posted_);
    for (auto &f : q) f();
    return true;
  }
  void fire() {
    std::vector<uint64_t> due;
    for (auto &[id, t] : timers_) if (t.due <= now_) due.push_back(id);
    for (uint64_t id : due) {
      auto it = timers_.find(id);
      if (it == timers_.end()) continue;
      Task cb = it->second.cb;
      if (it->second.repeat > 0) it->second.due = now_ + it->second.repeat; else timers_.erase(it);
      cb();
    }
  }
};
}  // namespace cs
