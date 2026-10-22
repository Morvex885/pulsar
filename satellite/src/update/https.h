// Minimal HTTPS GET on the firmware's OpenSSL (dlopen'ed: libssl.so.3 on the Midi, libssl.so.1.1
// on the Lite 2; the daemon links no TLS library). Certificates are verified against the firmware's
// CA bundle, host names with SSL_set1_host. Blocking: call from a worker thread.
#pragma once

#include <atomic>
#include <cstddef>
#include <functional>
#include <string>
#include <vector>

namespace cs {

struct HttpsResponse {
  int status = 0;        // HTTP status of the final response; 0 = transport/TLS error
  std::string body;
  std::string location;  // Location of a redirect that was not followed
  std::string error;     // set when status == 0 or the body exceeded max_bytes
};

// Follows up to max_redirects redirects (GitHub release assets redirect to another host); a
// redirect beyond that is returned with its Location. headers are extra "Name: value" lines.
// progress(received, total) runs on this thread; total is 0 when unknown. cancel is checked
// between reads.
HttpsResponse https_get(const std::string &url, const std::vector<std::string> &headers, size_t max_bytes,
                        const std::function<void(size_t, size_t)> &progress = {},
                        const std::atomic<bool> *cancel = nullptr, int max_redirects = 5);

// Lower-case hex SHA-256 via the firmware's libcrypto; "" if it is unavailable.
std::string sha256_hex(const std::string &data);

}  // namespace cs
