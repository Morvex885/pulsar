# Cross-build for Yandex Stations with cargo-zigbuild: one binary per userland ABI.
#   ARCH=aarch64  64-bit userspace (Station Midi, glibc 2.36)
#   ARCH=armhf    32-bit userspace (Station Lite 2, Mini 2 and Mini 3, glibc 2.30; also Midi
#                  images with an armhf userland)
# `make` builds both daemons; `make ARCH=armhf` one of them. Output: build/<arch>/pulsar (stripped,
# the one deployed) and build/<arch>/pulsar.debug (line tables, for `addr2line -f -e` on the crash
# report; non-PIE, so the addresses in it are absolute). `make ARCH=armhf tools` builds every tool
# (crates/*/src/bin/*.rs) into build/<arch>/tools/, `make ARCH=armhf build/armhf/tools/<name>` one
# of them. `make check`: fmt, clippy and tests on the host (without sendspin-cpp); clippy also
# for both stations, through cargo-zigbuild.
#
# The Sendspin player links sendspin-cpp and its dependencies statically. They are fetched once
# (pinned commits/tags, see third_party/fetch.sh) into third_party/src/ and built per ARCH with
# CMake + Ninja through third_party/CMakeLists.txt. Host tools needed: rustup (installs the
# toolchain of rust-toolchain.toml), cargo-zigbuild 0.24.0, zig 0.14.1 (0.13 produces armhf C++
# binaries that crash before main), git, curl, cmake >= 3.16, ninja, patch, make (e.g.
# `uv tool install cmake`, `uv tool install ninja`). Use ZIG=/path/to/zig for another zig.
ZIG   ?= zig
CARGO ?= cargo
CARGO_ZIGBUILD ?= cargo-zigbuild
ARCHES := aarch64 armhf

# zig for cargo-zigbuild: arch builds and the station clippy of `make check`. cargo-zigbuild would
# prefer `python3 -m ziglang` (any version) over the zig checked here: point it at that zig and at
# a python that does not exist.
ZIG_PATH     := $(shell command -v $(ZIG))
ZIG_MISSING  := zig not found: '$(ZIG)'; install zig 0.14.1 and pass ZIG=/path/to/zig
ZIGBUILD_ENV := CARGO_ZIGBUILD_ZIG_PATH='$(abspath $(ZIG_PATH))' CARGO_ZIGBUILD_PYTHON_PATH=/nonexistent/python3

ifeq ($(ARCH),)

all: $(ARCHES)
$(ARCHES):
	+$(MAKE) --no-print-directory ARCH=$@ binary

# Host build: pulsar-sendspin compiles without its sendspin-cpp FFI here. The second clippy run
# lints the code under #[cfg(target_arch = ...)] for both stations (type-checks only, nothing is
# linked or run, so any zig does); build scripts compile C for those targets (ring), hence
# cargo-zigbuild.
# The third lints pulsar-sendspin's FFI side (cfg sendspin_ffi, normally set by its build.rs when
# the Makefile provides sendspin-cpp): type-checks only, nothing C++ compiled or linked; its own
# target dir keeps the changed RUSTFLAGS from rebuilding the others.
check:
	$(if $(ZIG_PATH),,$(error $(ZIG_MISSING)))
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	env $(ZIGBUILD_ENV) $(CARGO_ZIGBUILD) clippy --workspace --all-targets --locked \
	  --target aarch64-unknown-linux-gnu.2.36 --target armv7-unknown-linux-gnueabihf.2.30 -- -D warnings
	CARGO_TARGET_DIR=target/clippy-ffi RUSTFLAGS='--cfg sendspin_ffi' $(CARGO) clippy -p pulsar-sendspin \
	  --all-targets --locked --target aarch64-unknown-linux-gnu --target armv7-unknown-linux-gnueabihf \
	  -- -D warnings
	$(CARGO) test --workspace --locked

clean:
	rm -rf build target

# Also forgets the fetched dependency sources.
distclean: clean
	rm -rf third_party/src

.PHONY: all check clean distclean $(ARCHES)

else

ifeq ($(ARCH),aarch64)
TARGET      := aarch64-linux-gnu.2.36
CPU         :=
RUST_CPU    :=
RUST_TARGET := aarch64-unknown-linux-gnu
GLIBC       := 2.36
else ifeq ($(ARCH),armhf)
TARGET      := arm-linux-gnueabihf.2.30
CPU         := -mcpu=cortex_a53
# Same core as the C++ build: VFPv4 makes f32/f64 mul_add a vfma instruction (the generic armv7
# target calls libm fmaf instead).
RUST_CPU    := -C target-cpu=cortex-a53
RUST_TARGET := armv7-unknown-linux-gnueabihf
GLIBC       := 2.30
else
$(error ARCH must be one of: $(ARCHES))
endif

ifeq ($(ZIG_PATH),)
$(error $(ZIG_MISSING))
endif
ZIG_VERSION := $(shell $(ZIG) version 2>/dev/null)
ifneq ($(filter 0.11.% 0.12.% 0.13.%,$(ZIG_VERSION)),)
$(error zig $(ZIG_VERSION) is too old (armhf C++ binaries crash before main); use zig 0.14.1, e.g. make ZIG=/path/to/zig)
endif

export ZIG
BUILD := build/$(ARCH)

# sendspin-cpp (keep the SENDSPIN_ENABLE_* defines of crates/pulsar-sendspin/build.rs in sync with
# the role options in third_party/CMakeLists.txt)
SS_SRC   := third_party/src
SS_BUILD := $(BUILD)/third_party
SS_STAMP := $(SS_BUILD)/.built

# Reported by `pulsar --version` and Home Assistant. --tags: a release tagged without -a must still
# describe as its tag (the updater checks `--version` against it). --match 'v*': only release tags
# count (not e.g. rust-port-base), so the updater can parse vX.Y.Z[-N-g<hash>].
VERSION := $(shell git describe --tags --match 'v*' --always --dirty 2>/dev/null || echo unknown)
# HEAD's commit date in the format of C++ __DATE__ ", " __TIME__ (Home Assistant's
# compilation_time). Not the current time: cargo rebuilds a crate whenever an option_env! value
# changes, and the thin-LTO relink of pulsar would then run on every make, every deploy included.
BUILD_TIME := $(shell TZ=UTC0 LC_ALL=C git log -1 --format=%cd --date=format-local:'%b %e %Y, %H:%M:%S' 2>/dev/null || echo unknown)

# C/C++ in build scripts (ring, the Sendspin shim) goes through cargo-zigbuild's own zig wrappers:
# no CC_*/CXX_*/ZIG_TARGET/ZIG_CPU here (cc adds --target=<llvm triple>, which
# third_party/toolchain/zig-c++ does not filter; those wrappers serve the CMake build only).
# Non-PIE (relocation-model=static) keeps crash report addresses absolute; frame pointers feed
# the aarch64 crash report's fp: chain.
CARGO_ENV := PULSAR_VERSION='$(VERSION)' PULSAR_BUILD_TIME='$(BUILD_TIME)' \
             PULSAR_SS_SRC=$(CURDIR)/$(SS_SRC) PULSAR_SS_BUILD=$(CURDIR)/$(SS_BUILD) PULSAR_REQUIRE_SS=1 \
             RUSTFLAGS='-C relocation-model=static -C force-frame-pointers=yes $(RUST_CPU)' \
             $(ZIGBUILD_ENV)
ZIGBUILD  := env $(CARGO_ENV) $(CARGO) zigbuild --release --locked --target $(RUST_TARGET).$(GLIBC)
OUT       := target/$(RUST_TARGET)/release
TOOLS     := $(basename $(notdir $(wildcard crates/*/src/bin/*.rs)))

# cargo always runs (FORCE) and decides itself what is out of date; `+` hands it make's jobserver.
# Builds of both arches share target/ and wait for each other on its lock. The deployed binary is
# stripped (/data holds the binary, its .prev and the upload: ~80 MB on the Lite 2, ~20 MB on the
# Mini 2).
binary: $(SS_STAMP) FORCE
	+$(ZIGBUILD) -p pulsar --bin pulsar
	@mkdir -p $(BUILD)
	cp $(OUT)/pulsar $(BUILD)/pulsar.debug
	$(ZIG) objcopy --strip-all $(BUILD)/pulsar.debug $(BUILD)/pulsar

# Without pulsar: `make binary tools` copies target/.../pulsar outside cargo's lock. After binary
# when both are asked for: two cargo-zigbuild starting at once race on creating their zig wrapper
# files ("File exists (os error 17)" on a fresh CI runner); cargo serialized them on target/ anyway.
tools: $(SS_STAMP) FORCE | $(filter binary,$(MAKECMDGOALS))
	+$(ZIGBUILD) --workspace --exclude pulsar --bins
	@mkdir -p $(BUILD)/tools
	for t in $(TOOLS); do $(ZIG) objcopy --strip-all $(OUT)/$$t $(BUILD)/tools/$$t || exit 1; done

# Same ordering for a single tool asked together with binary.
$(BUILD)/tools/%: $(SS_STAMP) FORCE | $(filter binary,$(MAKECMDGOALS))
	+$(ZIGBUILD) --bin $*
	@mkdir -p $(dir $@)
	$(ZIG) objcopy --strip-all $(OUT)/$* $@

# Fetch (once) + configure + build the Sendspin library and its dependencies.
$(SS_STAMP): third_party/fetch.sh third_party/CMakeLists.txt third_party/toolchain/zig.cmake
	third_party/fetch.sh
	@mkdir -p $(SS_BUILD)
	# Configured from inside the build dir: CMake's compiler probe makes zig write a file named "-".
	cd $(SS_BUILD) && ZIG_TARGET=$(TARGET) ZIG_CPU="$(CPU)" cmake -S $(CURDIR)/third_party -B . -G Ninja \
	  -DCMAKE_BUILD_TYPE=Release -DCMAKE_TOOLCHAIN_FILE=$(CURDIR)/third_party/toolchain/zig.cmake \
	  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 >/dev/null
	env -u MAKEFLAGS -u MFLAGS ZIG_TARGET=$(TARGET) ZIG_CPU="$(CPU)" ninja -C $(SS_BUILD)
	touch $@

.PHONY: binary tools FORCE
FORCE:

endif
