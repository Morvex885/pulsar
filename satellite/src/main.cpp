// pulsar: Home Assistant voice satellite for Yandex Stations (Midi, Lite 2).
// usage: pulsar [--version | state_file]
#include <execinfo.h>
#include <signal.h>
#include <sys/signalfd.h>
#include <ucontext.h>
#include <unistd.h>

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <thread>

#include "controller.h"
#include "core/log.h"
#include "core/loop.h"

#ifndef PULSAR_VERSION
#define PULSAR_VERSION "unknown"
#endif

namespace {

// Teardown (Sendspin threads, GStreamer, ALSA) runs with SIGTERM blocked and has hung before:
// give it a few seconds, then leave with the status main() was about to return (0 = deliberate
// exit for supervise.sh).
constexpr int kTeardownSeconds = 5;

void arm_teardown_watchdog(int status) {
  std::thread([status] {
    sleep(kTeardownSeconds);
    static const char msg[] = "teardown did not finish in 5 s, _exit\n";
    if (write(2, msg, sizeof msg - 1) < 0) {}
    _exit(status);
  }).detach();
}

// Crash report straight to stderr (the log file): everything here is async-signal-safe. The binary
// is stripped; resolve the addresses with `addr2line -f -e build/<arch>/pulsar.debug`
// (non-PIE: addresses are absolute).
void put(const char *s) {
  if (write(2, s, strlen(s)) < 0) {}
}

void put_hex(const char *label, uintptr_t v) {
  char b[40];
  size_t n = 0;
  for (const char *p = label; *p; p++) b[n++] = *p;
  b[n++] = '0';
  b[n++] = 'x';
  bool started = false;
  for (int shift = int(sizeof v) * 8 - 4; shift >= 0; shift -= 4) {
    unsigned d = unsigned(v >> shift) & 0xf;
    if (!d && !started && shift) continue;
    started = true;
    b[n++] = "0123456789abcdef"[d];
  }
  b[n++] = ' ';
  if (write(2, b, n) < 0) {}
}

// glibc's backtrace() retries dlopen(libgcc_s) on every call while the library is missing, and a
// crash inside malloc/the loader leaves their locks held: only call it when the startup warm-up
// proved the unwinder loads. Never on armhf (the flag stays false): libgcc_s's ARM EHABI unwinder
// calls the personality routine of each frame's .ARM.exidx entry, for our C++ frames the binary's
// static __gxx_personality_v0 (zig's libc++abi), which unwinds with zig's static libunwind on
// libgcc's incompatible context and segfaults (__unw_set_reg reading address 0x7).
bool g_backtrace_ok = false;

void crash_handler(int sig, siginfo_t *si, void *uc_) {
  alarm(3);  // whatever below hangs, SIGALRM (default action) ends the process and the supervisor restarts it
  auto *uc = static_cast<ucontext_t *>(uc_);
  put("\n=== CRASH ");
  put(sig == SIGSEGV ? "SIGSEGV " : sig == SIGBUS ? "SIGBUS " : sig == SIGABRT ? "SIGABRT " : "signal ");
  put_hex("addr=", uintptr_t(si->si_addr));
#if defined(__aarch64__)
  put_hex("pc=", uintptr_t(uc->uc_mcontext.pc));
  put_hex("lr=", uintptr_t(uc->uc_mcontext.regs[30]));
#elif defined(__arm__)
  put_hex("pc=", uintptr_t(uc->uc_mcontext.arm_pc));
  put_hex("lr=", uintptr_t(uc->uc_mcontext.arm_lr));
#endif
  put_hex("handler=", uintptr_t(&crash_handler));
  put("\n");
  if (g_backtrace_ok) {
    void *fr[40];
    int n = backtrace(fr, 40);
    for (int i = 0; i < n; i++) put_hex(i ? "" : "bt: ", uintptr_t(fr[i]));
    put("\n");
  }
#if defined(__aarch64__)
  if (!g_backtrace_ok) {  // walk the frame-pointer chain (-fno-omit-frame-pointer)
    auto *fp = reinterpret_cast<uintptr_t *>(uc->uc_mcontext.regs[29]);
    put("fp: ");
    for (int i = 0; i < 40 && fp && !(uintptr_t(fp) & 15); i++) {
      uintptr_t ret = fp[1], next = fp[0];
      if (!ret) break;
      put_hex("", ret);
      if (next <= uintptr_t(fp)) break;
      fp = reinterpret_cast<uintptr_t *>(next);
    }
    put("\n");
  }
#endif
  signal(sig, SIG_DFL);
  raise(sig);
}

void install_crash_handler() {
  // Own stack: a stack overflow must still produce a report.
  static char altstack[64 * 1024];
  stack_t ss{};
  ss.ss_sp = altstack;
  ss.ss_size = sizeof altstack;
  sigaltstack(&ss, nullptr);
  // Handler before the warm-up: a fault in the warm-up itself is reported too (without backtrace()).
  struct sigaction sa{};
  sa.sa_sigaction = crash_handler;
  sa.sa_flags = SA_SIGINFO | SA_ONSTACK | SA_RESETHAND | SA_NODEFER;
  sigemptyset(&sa.sa_mask);
  for (int s : {SIGSEGV, SIGBUS, SIGABRT, SIGFPE, SIGILL}) sigaction(s, &sa, nullptr);
#if !defined(__arm__)
  // The backtrace unwinder loads libgcc_s lazily (not async-signal-safe): do it once now and
  // remember whether it worked.
  void *warm[4];
  g_backtrace_ok = backtrace(warm, 4) >= 3;
#endif
}

}  // namespace

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "--version")) {
    printf("pulsar %s\n", PULSAR_VERSION);
    return 0;
  }
  const char *state = argc > 1 ? argv[1] : "/data/pulsar/state.conf";
  signal(SIGPIPE, SIG_IGN);
  install_crash_handler();
  const cs::Model *model = cs::detect_model();
  if (!model) return 1;

  cs::Loop loop;
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGINT);
  sigaddset(&mask, SIGTERM);
  pthread_sigmask(SIG_BLOCK, &mask, nullptr);  // before any thread starts: workers inherit the mask
  int sfd = signalfd(-1, &mask, SFD_CLOEXEC | SFD_NONBLOCK);
  loop.add_fd(sfd, 0x001 /* EPOLLIN */, [&](uint32_t) {
    signalfd_siginfo si;
    while (read(sfd, &si, sizeof si) == sizeof si) {
      LOGI("signal %u, exiting", si.ssi_signo);
      loop.stop();
    }
  });

  cs::Controller ctl(loop, *model, state);
  if (!ctl.start()) {
    arm_teardown_watchdog(1);
    return 1;
  }
  LOGI("pulsar %s running on %s (%s)", PULSAR_VERSION, model->model_name.c_str(),
       model->codename.c_str());
  loop.run();
  arm_teardown_watchdog(0);
  return 0;  // ~Controller runs here, under the watchdog
}
