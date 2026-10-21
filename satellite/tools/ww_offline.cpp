// Offline evaluation of a stock spotter model on a recording made on the device:
// raw capture WAV (S16, layout of the detected model, e.g. `arecord -D hw:0,1 -c 8 -r 48000`
// on the Lite 2) -> Yandex VQE -> spotter. The spotter channel layout is derived from the model
// like the daemon does (audio/frontend.cpp).
// usage: ww_offline <wav> [model_dir]   (default: .../spotter_models/activation/alisa)
// Prints every spotter event with its time; "HIT" lines are confirmations (events 2 and 5), and
// per second the RMS of the speaker reference and of the VQE output (echo cancellation check).
// Experiments: VT=<type> feeds every spotter channel from that VQE output; SPK=<ch>[,<ch>...]
// overrides the speaker reference capture channels.
#include <dlfcn.h>

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "model.h"

typedef void *P;
static void *lib;
template <class F>
F sym(const char *n) {
  void *p = dlsym(lib, n);
  if (!p) {
    printf("missing %s\n", n);
    exit(1);
  }
  return (F)p;
}
static const char *serr(P e) { return e ? sym<const char *(*)(P)>("spotter_error_get_message")(e) : "ok"; }
static const char *verr(P e) { return e ? sym<const char *(*)(P)>("YandexVqeC_Error_GetMessage")(e) : "ok"; }

// Same mapping as audio/frontend.cpp: spotter channel type -> VQE output type.
static const int kSpotterToVqe[6] = {9, 2, 9, 2, 6, 6};

int main(int argc, char **argv) {
  const cs::Model *model_hw = cs::detect_model();
  if (!model_hw) return 1;
  cs::Model m = *model_hw;
  if (const char *s = getenv("SPK")) {
    m.spk_channels.clear();
    for (const char *p = s; *p;) {
      m.spk_channels.push_back(unsigned(strtoul(p, const_cast<char **>(&p), 10)));
      if (*p == ',') p++;
    }
  }
  setvbuf(stdout, nullptr, _IONBF, 0);
  if (argc < 2) {
    fprintf(stderr, "usage: %s wav [model_dir]\n", argv[0]);
    return 2;
  }
  const char *model = argc > 2 ? argv[2] : "/system/vendor/quasar/spotter_models/activation/alisa";
  dlopen("/system/vendor/quasar/libs/libtflite.so", RTLD_NOW | RTLD_GLOBAL);
  lib = dlopen("/system/vendor/quasar/libs/libyandex_evil.so", RTLD_NOW | RTLD_GLOBAL);
  if (!lib) return printf("dlopen %s\n", dlerror()), 1;
  FILE *f = fopen(argv[1], "rb");
  if (!f) return perror("wav"), 1;
  fseek(f, 44, SEEK_SET);

  P cfg = nullptr, vqe = nullptr;
  P e = sym<P (*)(const char *, P *)>("YandexVqeC_Config_CreateFromPreset")(m.vqe_preset.c_str(), &cfg);
  const size_t frames = size_t(sym<int (*)(P)>("YandexVqeC_Config_GetInputChunkSize")(cfg));
  if (e) return printf("cfg: %s\n", verr(e)), 1;
  e = sym<P (*)(P, const char *, size_t, const void *, P *)>("YandexVqeC_Create")(cfg, "/system/vendor/quasar/vqe_models",
                                                                                 0, nullptr, &vqe);
  if (e) return printf("vqe: %s\n", verr(e)), 1;
  auto process = sym<P (*)(P, const float *, size_t, const float *, size_t)>("YandexVqeC_Process");
  auto out = sym<P (*)(P, int, size_t, const float **, size_t *)>("YandexVqeC_GetOutputChannelData");

  P se = nullptr;
  P conf = sym<P (*)(const char *, P *)>("spotter_conf_read")(model, &se);
  if (!conf) return printf("conf: %s\n", serr(se)), 1;
  std::vector<int> types, vt;
  for (int t = 0; t < 6; t++) {
    long n = sym<long (*)(P, long, P *)>("spotter_conf_get_required_channel_count")(conf, t, &se);
    for (long i = 0; i < n && n < 8; i++) {
      types.push_back(t);
      vt.push_back(getenv("VT") ? atoi(getenv("VT")) : kSpotterToVqe[t]);  // VT=<type>: override for experiments
    }
  }
  P sp = sym<P (*)(P, int, int, const int *, bool, P *)>("spotter_create_multichannel")(conf, 16000, (int)types.size(),
                                                                                        types.data(), false, &se);
  if (!sp) return printf("spotter: %s\n", serr(se)), 1;
  printf("model %s: %zu channels\n", model, types.size());
  auto submit = sym<void (*)(P, const void *, size_t, P *)>("spotter_submit_raw_data");
  auto next = sym<int (*)(P, P *)>("spotter_get_next_event");
  auto evname = sym<const char *(*)(int)>("event_id_to_str");
  auto phrase = sym<const char *(*)(P, P *)>("spotter_get_current_phrase");

  const size_t nch = types.size(), cap = m.capture_channels;
  const size_t nmic = m.mic_channels.size(), nspk = m.spk_channels.size();
  std::vector<int16_t> in(frames * cap), sbuf(512 * nch);
  std::vector<float> mics(frames * nmic), spk(frames * nspk);
  double t = 0, spk_e = 0, out_e = 0, next_report = 1;
  size_t spk_n = 0, out_n = 0;
  int hits = 0;
  while (fread(in.data(), 2, in.size(), f) == in.size()) {
    for (size_t i = 0; i < frames; i++) {
      for (size_t c = 0; c < nmic; c++) mics[i * nmic + c] = in[i * cap + m.mic_channels[c]] / 32768.f;
      for (size_t c = 0; c < nspk; c++) {
        spk[i * nspk + c] = in[i * cap + m.spk_channels[c]] / 32768.f;
        spk_e += double(spk[i * nspk + c]) * spk[i * nspk + c];
        spk_n++;
      }
    }
    if ((e = process(vqe, mics.data(), mics.size(), spk.data(), spk.size()))) return printf("process: %s\n", verr(e)), 1;
    for (size_t k = 0; k < nch; k++) {
      const float *d;
      size_t n;
      if ((e = out(vqe, vt[k], 0, &d, &n))) return printf("out %d: %s\n", vt[k], verr(e)), 1;
      for (size_t i = 0; i < 512 && i < n; i++) {
        float v = d[i] * 32768.f;
        sbuf[i * nch + k] = (int16_t)(v > 32767 ? 32767 : v < -32768 ? -32768 : v);
        if (k == 0) {
          out_e += double(d[i]) * d[i];
          out_n++;
        }
      }
    }
    submit(sp, sbuf.data(), sbuf.size() * 2, &se);
    if (se) return printf("submit: %s\n", serr(se)), 1;
    for (int guard = 0; guard < 16; guard++) {
      int ev = next(sp, &se);
      if (se || ev == 0 || ev == 4 || ev == 7 || ev == 9) break;
      const char *ph = phrase(sp, &se);
      bool hit = ev == 2 || ev == 5;
      hits += hit;
      printf("t=%6.2fs event %d %s phrase=%s%s\n", t, ev, evname(ev), ph ? ph : "", hit ? "  HIT" : "");
    }
    t += double(frames) / m.capture_rate;
    if (t >= next_report) {
      printf("t=%6.2fs rms dBFS: speaker ref %6.1f, vqe out %6.1f\n", t, 10 * std::log10(spk_e / std::max<size_t>(spk_n, 1) + 1e-12),
             10 * std::log10(out_e / std::max<size_t>(out_n, 1) + 1e-12));
      spk_e = out_e = 0;
      spk_n = out_n = 0;
      next_report += 1;
    }
  }
  printf("done: %.1fs, hits=%d\n", t, hits);
  return 0;
}
