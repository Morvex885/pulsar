// No audio is played: the null PCM exercises ALSA's near/get pointer ABI without hardware.
// Run on versioned Midi and unversioned Lite 2 firmware after cross-building this tool.
#include <cstdio>

#include "audio/alsa_abi.h"

int main() {
  snd_pcm_t *pcm = nullptr;
  int err = snd_pcm_open(&pcm, "null", SND_PCM_STREAM_PLAYBACK, 0);
  if (err < 0) {
    fprintf(stderr, "open null PCM: %s\n", snd_strerror(err));
    return 1;
  }
  snd_pcm_hw_params_t *hw;
  snd_pcm_hw_params_alloca(&hw);
  auto check = [&](int rc, const char *what) {
    if (rc < 0) fprintf(stderr, "%s: %s\n", what, snd_strerror(rc));
    return rc >= 0;
  };
  unsigned rate = 96000;
  snd_pcm_uframes_t period = 3072, buffer = 24576;
  bool ok = check(snd_pcm_hw_params_any(pcm, hw), "any") &&
            check(snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED), "access") &&
            check(snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE), "format") &&
            check(snd_pcm_hw_params_set_channels(pcm, hw, 2), "channels") &&
            check(cs::alsa::hw_params_set_rate_near(pcm, hw, &rate, nullptr), "rate") &&
            check(cs::alsa::hw_params_set_period_size_near(pcm, hw, &period, nullptr), "period") &&
            check(cs::alsa::hw_params_set_buffer_size_near(pcm, hw, &buffer), "buffer") &&
            check(snd_pcm_hw_params(pcm, hw), "apply");
  snd_pcm_uframes_t actual_period = 0, actual_buffer = 0;
  ok = ok && check(cs::alsa::hw_params_get_period_size(hw, &actual_period, nullptr), "get period") &&
       check(cs::alsa::hw_params_get_buffer_size(hw, &actual_buffer), "get buffer");
  ok = ok && rate == 96000 && period == 3072 && buffer == 24576 && actual_period == period && actual_buffer == buffer;
  // alias=no: the library has only the current ABI (unversioned, no __snd_* exports).
  bool alias = dlsym(RTLD_DEFAULT, "__snd_pcm_hw_params_set_rate_near") != nullptr;
  printf("rate=%u period=%lu/%lu buffer=%lu/%lu alias=%s: %s\n", rate, (unsigned long)period,
         (unsigned long)actual_period, (unsigned long)buffer, (unsigned long)actual_buffer,
         alias ? "yes" : "no", ok ? "PASS" : "FAIL");
  snd_pcm_close(pcm);
  return ok ? 0 : 1;
}
