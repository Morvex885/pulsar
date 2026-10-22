// Player/Mixer probe.
// usage: player_probe <music_file_uri> <announce_file> <http_base_url> [https_url]
//  1. MUSIC plays <music_file_uri> (gain 0.3)
//  2. at +1.5 s ANNOUNCE plays [<announce_file>, <http_base>/announce.flac] (gain 0.3) with
//     MUSIC ducked to 0.08; restored when the announcement is done
//  3. MUSIC paused for 1 s and resumed
//  4. after MUSIC done: MUSIC plays <http_base>/tone.mp3, then <http_base>/missing.flac (expect
//     done(false)), then [https_url] for 4 s (state must reach PLAYING; then stop())
//  5. Mixer: set_volume(0.5) / mute / restore original
#include <cstdio>
#include <cstdlib>
#include <string>

#include "core/log.h"
#include "core/loop.h"
#include "media/player.h"
#include "model.h"

using namespace cs;

static const char *sname(PlayState s) {
  static const char *n[] = {"IDLE", "LOADING", "PLAYING", "PAUSED", "ERROR"};
  return n[int(s)];
}

int main(int argc, char **argv) {
  if (argc < 4) {
    fprintf(stderr, "usage: %s <music_uri> <announce_file> <http_base> [https_url]\n", argv[0]);
    return 2;
  }
  std::string music = argv[1], announce = argv[2], http = argv[3], https = argc > 4 ? argv[4] : "";
  Loop loop;
  int64_t t0 = Loop::now_ms();
  auto ts = [&] { return double(Loop::now_ms() - t0) / 1000.0; };

  const Model *model = detect_model();
  if (!model) return 1;
  Mixer mixer(*model);
  if (mixer.open()) {
    float orig = mixer.volume();
    printf("[%6.2f] mixer volume %.3f\n", ts(), orig);
    mixer.set_volume(0.5f);
    system("amixer -c0 cget name='Master Playback Volume' | grep ': values'");
    mixer.set_muted(true);
    system("amixer -c0 cget name='Master Playback Volume' | grep ': values'");
    mixer.set_muted(false);
    mixer.set_volume(orig);
    system("amixer -c0 cget name='Master Playback Volume' | grep ': values'");
  }

  Player player(loop);
  if (!player.init(model->playback_pcm)) {
    printf("player init failed\n");
    return 1;
  }
  player.on_state([&](Channel ch, PlayState s) {
    printf("[%6.2f] state %s -> %s\n", ts(), ch == Channel::MUSIC ? "MUSIC" : "ANNOUNCE", sname(s));
    fflush(stdout);
  });
  player.set_channel_gain(Channel::MUSIC, 0.3f);
  player.set_channel_gain(Channel::ANNOUNCE, 0.3f);

  auto phase_https = [&] {
    if (https.empty()) {
      loop.add_timer(500, [&] { loop.stop(); });
      return;
    }
    printf("[%6.2f] == HTTPS %s\n", ts(), https.c_str());
    player.play(Channel::MUSIC, {https}, [&](bool ok) {
      printf("[%6.2f] https done ok=%d\n", ts(), ok);
      loop.add_timer(300, [&] { loop.stop(); });
    });
    loop.add_timer(5000, [&] {
      printf("[%6.2f] https state after 5 s: %s -> stop()\n", ts(), sname(player.state(Channel::MUSIC)));
      player.stop(Channel::MUSIC);
      loop.add_timer(500, [&] { loop.stop(); });
    });
  };

  auto phase_http = [&] {
    printf("[%6.2f] == HTTP %s/tone.mp3\n", ts(), http.c_str());
    player.play(Channel::MUSIC, {http + "/tone.mp3"}, [&](bool ok) {
      printf("[%6.2f] http mp3 done ok=%d\n", ts(), ok);
      printf("[%6.2f] == HTTP 404 %s/missing.flac\n", ts(), http.c_str());
      player.play(Channel::MUSIC, {http + "/missing.flac"}, [&](bool ok2) {
        printf("[%6.2f] missing done ok=%d (expect 0)\n", ts(), ok2);
        phase_https();
      });
    });
  };

  printf("[%6.2f] == MUSIC %s\n", ts(), music.c_str());
  player.play(Channel::MUSIC, {music}, [&](bool ok) {
    printf("[%6.2f] music done ok=%d\n", ts(), ok);
    phase_http();
  });
  loop.add_timer(1500, [&] {
    printf("[%6.2f] == ANNOUNCE [%s, %s/announce.flac], duck music\n", ts(), announce.c_str(), http.c_str());
    player.set_channel_gain(Channel::MUSIC, 0.08f);
    player.play(Channel::ANNOUNCE, {announce, http + "/announce.flac"}, [&](bool ok) {
      printf("[%6.2f] announce done ok=%d, unduck\n", ts(), ok);
      player.set_channel_gain(Channel::MUSIC, 0.3f);
      loop.add_timer(500, [&] {
        printf("[%6.2f] == pause MUSIC\n", ts());
        player.pause(Channel::MUSIC);
        loop.add_timer(1000, [&] {
          printf("[%6.2f] == resume MUSIC (state %s)\n", ts(), sname(player.state(Channel::MUSIC)));
          player.resume(Channel::MUSIC);
        });
      });
    });
  });
  loop.add_timer(60000, [&] {
    printf("[%6.2f] TIMEOUT\n", ts());
    loop.stop();
  });
  loop.run();
  printf("[%6.2f] exit\n", ts());
  return 0;
}
