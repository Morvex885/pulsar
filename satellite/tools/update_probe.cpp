// Updater probe: exercises the HTTPS client and the release updater without the daemon.
// usage: update_probe versions
//          release_newer() table for a few describe strings (no network)
//        update_probe get <https_url>
//          status, size and sha256 of the body (redirects followed)
//        update_probe check [owner/repo]
//          latest stable release as the daemon sees it (default Morvex885/pulsar)
//        update_probe install <version> <bin_url> <sums_url> <binary>
//          downloads, verifies and swaps <binary> (keeps <binary dir>/pulsar.prev), as the daemon
//          does for a release named <version>
#include <cstdio>
#include <cstring>
#include <string>

#include "core/loop.h"
#include "update/https.h"
#include "update/updater.h"

using namespace cs;

int main(int argc, char **argv) {
  std::string cmd = argc > 1 ? argv[1] : "";
  if (cmd == "versions") {
    const char *cases[][2] = {{"v1.2.4", "v1.2.3"},       {"v1.2.3", "v1.2.3"},   {"v1.2.3", "v1.2.3-4-gabc1234"},
                              {"v1.3.0", "v1.2.3-4-gabc"}, {"v1.2.3", "v1.10.0"}, {"v1.2.3", "6ac30be"},
                              {"v2.0.0", "v1.9.9-dirty"},  {"junk", "v1.0.0"}};
    for (auto &c : cases) {
      bool known = false;
      bool newer = release_newer(c[0], c[1], &known);
      printf("latest %-8s current %-20s newer=%d current_known=%d\n", c[0], c[1], newer, known);
    }
    return 0;
  }
  if (cmd == "get" && argc == 3) {
    HttpsResponse r = https_get(argv[2], {"User-Agent: pulsar-update-probe"}, 64u << 20);
    printf("status %d, %zu bytes, sha256 %s%s%s\n", r.status, r.body.size(), sha256_hex(r.body).c_str(),
           r.error.empty() ? "" : ", error: ", r.error.c_str());
    if (r.status != 200) printf("%.400s\n", r.body.c_str());
    return r.status == 200 ? 0 : 1;
  }
  Loop loop;
  int rc = 1;
  if (cmd == "check" && argc <= 3) {
    Updater up(loop, argc == 3 ? argv[2] : "Morvex885/pulsar", "probe");
    up.check([&](const std::string &err, const Release &r) {
      if (err.empty()) {
        printf("version %s\nurl %s\nbin %s\nsums %s\n", r.version.c_str(), r.url.c_str(), r.bin_url.c_str(),
               r.sums_url.c_str());
        rc = 0;
      } else {
        printf("error: %s\n", err.c_str());
      }
      loop.stop();
    });
    loop.run();
    return rc;
  }
  if (cmd == "install" && argc == 6) {
    Updater up(loop, "Morvex885/pulsar", "probe");
    Release r;
    r.version = argv[2];
    r.bin_url = argv[3];
    r.sums_url = argv[4];
    up.install(
        r, argv[5], [](float p) { printf("progress %.0f\n", p); },
        [&](const std::string &err, bool fatal) {
          printf(err.empty() ? "installed\n" : "error (fatal=%d): %s\n", fatal, err.c_str());
          rc = err.empty() ? 0 : 1;
          loop.stop();
        });
    loop.run();
    return rc;
  }
  fprintf(stderr, "usage: %s versions | get <url> | check [owner/repo] | install <version> <bin_url> <sums_url> <binary>\n",
          argv[0]);
  return 2;
}
