// Zigbee probe: runs the Midi's Zigbee coordinator (pulsar's coordinator firmware must be in the
// radio) and its deCONZ server without the daemon, for testing with zigpy / Zigbee2MQTT against
// another port while the service owns everything else.
// usage: zigbee_probe [port]            (default 6639; the daemon serves 6638)
//        zigbee_probe flash <image.ybl>  write a radio firmware image
// The daemon must not run its Zigbee role at the same time: both would drive /dev/ttyS2.
#include <signal.h>

#include <cstdlib>
#include <cstring>
#include <memory>

#include "core/log.h"
#include "core/loop.h"
#include "model.h"
#include "zigbee/coordinator.h"
#include "zigbee/deconz_server.h"
#include "zigbee/flasher.h"

using namespace cs;

int main(int argc, char **argv) {
  const Model *model = detect_model();
  if (!model || model->zigbee_tty.empty()) {
    LOGE("no Zigbee radio on this model");
    return 1;
  }
  if (argc == 3 && strcmp(argv[1], "flash") == 0) {
    std::vector<uint8_t> image;
    YblInfo info;
    std::string err;
    if (!ybl_load(argv[2], &image, &info, &err)) {
      LOGE("%s", err.c_str());
      return 1;
    }
    LOGI("writing %s (version 0x%08x)", argv[2], info.version);
    const bool ok = radio_flash({model->zigbee_tty, model->zigbee_reset, model->zigbee_boot}, image,
                                [](float p) { LOGD("%d %%", int(p * 100)); }, &err);
    if (!ok) LOGE("%s", err.c_str());
    return ok ? 0 : 1;
  }
  const uint16_t port = uint16_t(argc > 1 ? atoi(argv[1]) : 6639);
  signal(SIGPIPE, SIG_IGN);
  Loop loop;
  TelinkLink link(loop, model->zigbee_tty, model->zigbee_reset, model->zigbee_boot);
  ZigbeeCoordinator coord(loop, link);
  DeconzServer server(loop, coord, port);
  ZigbeeCoordinator::Hooks h;
  h.received = [&](const ApsFrame &f) {
    LOGI("rx 0x%04x:%u -> %s0x%04x:%u profile 0x%04x cluster 0x%04x %zu bytes lqi %u rssi %d", f.src, f.src_ep,
         f.group ? "group " : "", f.dst, f.dst_ep, f.profile, f.cluster, f.data.size(), f.lqi, f.rssi);
    server.on_received(f);
  };
  h.sent = [&](uint32_t handle, uint8_t status) {
    LOGI("sent %08x: status 0x%02x", handle, status);
    server.on_sent(handle, status);
  };
  h.changed = [&] {
    const ZigbeeNetwork &n = coord.network();
    LOGI("ready %d formed %d PAN 0x%04x channel %u IEEE %016llx", coord.ready(), n.formed, n.pan, n.channel,
         (unsigned long long)n.ieee);
    server.on_changed();
  };
  if (!server.start() || !link.open()) return 1;
  coord.start(std::move(h));
  loop.run();
  return 0;
}
