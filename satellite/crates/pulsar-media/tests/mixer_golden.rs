//! Mixer curves against golden vectors of the C++ reference (tests/golden/curves.txt; the
//! unit tests of src/player.rs read soup_status.txt and to_uri.txt from the same harness).
//!
//! How they were made: the functions were copied verbatim from
//! `git show rust-port-base:satellite/src/media/player.cpp` (lines 106-115 soup_status, 217-221
//! to_uri, 701-713 legacy_raw / legacy_volume, 727-754 Mixer::to_raw / from_raw / from_legacy /
//! to_legacy) into a throwaway harness (not committed) with a stub `Model { volume_map }` and the
//! maps below, built and run on the x86_64 host with
//! `zig c++ -std=c++20 -O2 -mfma -o harness main.cpp` (zig 0.14.1; -mfma because the station
//! builds, -ffp-contract=on with aarch64 fmadd / armhf vfma, fuse `0.1 + 0.9 * v` in
//! legacy_raw), then `./harness curves|soup|uri`. Outputs print f32 as %.9g (exact round trip).
//! The same harness built with the station flags (`-target aarch64-linux-gnu.2.36`,
//! `-target arm-linux-gnueabihf.2.30 -mcpu=cortex_a53`, -O2) printed identical files on a Midi and
//! a Lite 2, where this test, cross-built, also passed.

use pulsar_core::model::{Model, VolumePoint, model_from_compatible};
use pulsar_media::mixer;

const fn vp(level: i32, raw: i32) -> VolumePoint {
    VolumePoint { level, raw }
}

static MULTI: [VolumePoint; 6] = [
    vp(0, 0),
    vp(1, 20),
    vp(4, 90),
    vp(10, 170),
    vp(16, 230),
    vp(20, 255),
];
static OFFSET: [VolumePoint; 3] = [vp(0, 10), vp(5, 128), vp(10, 255)];
static SINGLE: [VolumePoint; 1] = [vp(0, 0)];

fn with_map(base: &Model, map: &'static [VolumePoint]) -> Model {
    Model {
        volume_map: map,
        ..*base
    }
}

#[test]
fn curves_match_cxx() {
    let midi = model_from_compatible(b"yandex,cucumber\0").unwrap();
    let lite2 = model_from_compatible(b"yandex,plum\0").unwrap();
    // Both stations have the stock map.
    assert_eq!(midi.volume_map, lite2.volume_map);
    let (multi, offset, single) = (
        with_map(midi, &MULTI),
        with_map(midi, &OFFSET),
        with_map(midi, &SINGLE),
    );
    let models = [
        ("stock", midi),
        ("multi", &multi),
        ("offset", &offset),
        ("single", &single),
    ];
    let model = |name: &str| models.iter().find(|(n, _)| *n == name).unwrap().1;
    let mut counts = [0usize; 4];
    for line in include_str!("golden/curves.txt").lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let [func, map, input, want] = f[..] else {
            panic!("bad golden line {line}");
        };
        let m = model(map);
        match func {
            "to_raw" => {
                let v: f32 = input.parse().unwrap();
                assert_eq!(mixer::to_raw(m, v), want.parse::<i64>().unwrap(), "{line}");
                counts[0] += 1;
            }
            "from_raw" => {
                let raw: i64 = input.parse().unwrap();
                let got = mixer::from_raw(m, raw);
                assert_eq!(
                    got.to_bits(),
                    want.parse::<f32>().unwrap().to_bits(),
                    "{line}: {got}"
                );
                counts[1] += 1;
            }
            "to_legacy" | "from_legacy" => {
                let v: f32 = input.parse().unwrap();
                let got = if func == "to_legacy" {
                    mixer::to_legacy(m, v)
                } else {
                    mixer::from_legacy(m, v)
                };
                assert_eq!(
                    got.to_bits(),
                    want.parse::<f32>().unwrap().to_bits(),
                    "{line}: {got}"
                );
                counts[if func == "to_legacy" { 2 } else { 3 }] += 1;
            }
            _ => panic!("bad golden line {line}"),
        }
    }
    // 111 + 49 + 12 volumes and raw -5..=300, for each of the 4 maps.
    assert_eq!(counts, [688, 1224, 688, 688]);
}

#[test]
fn stock_curve_landmarks() {
    let m = model_from_compatible(b"yandex,plum\0").unwrap();
    // The port's station tests cap the volume at 0.2, i.e. raw 82.
    assert_eq!(mixer::to_raw(m, 0.2), 82);
    assert_eq!(mixer::to_raw(m, 0.0), 0);
    assert_eq!(mixer::to_raw(m, 0.01), 49);
    assert_eq!(mixer::to_raw(m, 1.0), 255);
    assert_eq!(mixer::from_raw(m, 0), 0.0);
    assert_eq!(mixer::from_raw(m, 255), 1.0);
    assert_eq!(mixer::from_raw(m, 49), 0.05);
}

#[test]
fn mini3_curve_landmarks() {
    let m = model_from_compatible(b"yandex,bergamot\0").unwrap();
    // Mini 3 stock map: 0 -> 0, 1 -> 33, 20 -> 255 (not the Lite 2's).
    assert_eq!(mixer::to_raw(m, 0.0), 0);
    assert_eq!(mixer::to_raw(m, 0.01), 33);
    assert_eq!(mixer::to_raw(m, 0.05), 33);
    assert_eq!(mixer::to_raw(m, 0.2), 68);
    assert_eq!(mixer::to_raw(m, 1.0), 255);
    assert_eq!(mixer::from_raw(m, 0), 0.0);
    assert_eq!(mixer::from_raw(m, 33), 0.05);
    assert_eq!(mixer::from_raw(m, 255), 1.0);
}

#[test]
fn mini2_curve_has_no_lowest_step() {
    // Linear in percent, no jump to a lowest step: low HA volumes stay low (the C++ curve made
    // any volume of a two-point map full volume).
    let m = model_from_compatible(b"yandex,station-mini2\0").unwrap();
    assert_eq!(mixer::to_raw(m, 0.01), 3);
    assert_eq!(mixer::to_raw(m, 0.05), 13);
    assert_eq!(mixer::to_raw(m, 0.2), 51);
    assert_eq!(mixer::to_raw(m, 1.0), 255);
    assert_eq!(mixer::from_raw(m, 51), 0.2);
}
