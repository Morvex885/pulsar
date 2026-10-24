# Cross-build for Yandex Stations with zig: one binary per userland ABI.
#   ARCH=aarch64  64-bit userspace (Station Midi, glibc 2.36)
#   ARCH=armhf    32-bit userspace (Station Lite 2, glibc 2.30; also Midi images with an armhf
#                  userland)
# `make` builds both; `make ARCH=armhf` one of them. Output: build/<arch>/pulsar.
#
# The Sendspin player links sendspin-cpp and its dependencies statically. They are fetched once
# (pinned commits/tags, see third_party/fetch.sh) into third_party/src/ and built per ARCH with
# CMake + Ninja through third_party/CMakeLists.txt. The link-time ALSA sysroot is built from the
# alsa-lib release tarball (third_party/sysroot.sh). Host tools needed: zig 0.14.1 (0.13 produces
# armhf C++ binaries that crash before main), git, curl, cmake >= 3.16, ninja, patch, make
# (e.g. `uv tool install cmake`, `uv tool install ninja`). Use ZIG=/path/to/zig for another zig.
ZIG  ?= zig
ARCHES := aarch64 armhf

ifeq ($(ARCH),)

all: $(ARCHES)
$(ARCHES):
	+$(MAKE) --no-print-directory ARCH=$@ binary

clean:
	rm -rf build

# Also forgets the fetched dependency sources.
distclean: clean
	rm -rf third_party/src

.PHONY: all clean distclean $(ARCHES)

else

ifeq ($(ARCH),aarch64)
TARGET := aarch64-linux-gnu.2.36
CPU    :=
HOST   := aarch64-linux-gnu
ALSA   :=
else ifeq ($(ARCH),armhf)
TARGET := arm-linux-gnueabihf.2.30
CPU    := -mcpu=cortex_a53
HOST   := arm-linux-gnueabihf
ALSA   := --with-versioned=no
else
$(error ARCH must be one of: $(ARCHES))
endif

ZIG_VERSION := $(shell $(ZIG) version 2>/dev/null)
ifneq ($(filter 0.11.% 0.12.% 0.13.%,$(ZIG_VERSION)),)
$(error zig $(ZIG_VERSION) is too old (armhf C++ binaries crash before main); use zig 0.14.1, e.g. make ZIG=/path/to/zig)
endif

export ZIG
CXX      := $(ZIG) c++ -target $(TARGET) $(CPU)
BUILD    := build/$(ARCH)
SYSROOT  := $(BUILD)/sysroot
SYSROOT_STAMP := $(SYSROOT)/.built

# sendspin-cpp (keep SS_DEFS in sync with the role options in third_party/CMakeLists.txt)
SS_SRC   := third_party/src
SS_BUILD := $(BUILD)/third_party
SS_STAMP := $(SS_BUILD)/.built
SS_DEFS  := -DSENDSPIN_ENABLE_PLAYER -DSENDSPIN_ENABLE_OPUS -DSENDSPIN_ENABLE_METADATA -DSENDSPIN_ENABLE_CONTROLLER \
            -DSENDSPIN_ENABLE_VISUALIZER
SS_LIBS  := $(SS_BUILD)/sendspin/libsendspin.a \
            $(SS_BUILD)/_deps/micro_flac-build/libmicro_flac.a \
            $(SS_BUILD)/_deps/micro_opus-build/libmicro_opus.a \
            $(SS_BUILD)/_deps/micro_opus-build/micro-ogg-demuxer/libmicro_ogg_demuxer.a \
            $(SS_BUILD)/_deps/ixwebsocket-build/libixwebsocket.a \
            $(SS_BUILD)/sendspin/libnoise_c.a

# The deployed binary is stripped (the Lite 2 has ~80 MB of /data for the binary, its .prev and the
# upload); the unstripped link, with line tables, is kept as build/<arch>/pulsar.debug
# for `addr2line -f -e` on the crash report that main.cpp writes to the log.
# -Wno-date-time: the build date is reported to Home Assistant as compilation_time.
CXXFLAGS := -std=c++20 -O2 -Wall -Wextra -Wno-unused-parameter -Wno-date-time -Isrc -I$(SYSROOT)/include \
            -isystem $(SS_SRC)/sendspin-cpp/include -isystem $(SS_SRC)/noise-c/include $(SS_DEFS) -fno-omit-frame-pointer -g1
LDFLAGS  := -L$(SYSROOT)/lib -Wl,--start-group $(SS_LIBS) -Wl,--end-group -lasound -ldl -lpthread

SRCS := $(shell find src -name '*.cpp')
OBJS := $(patsubst src/%.cpp,$(BUILD)/%.o,$(SRCS))
DEPS := $(OBJS:.o=.d)

# Reported by `pulsar --version` and Home Assistant. Only main.o and controller.o see it;
# the stamp changes (and they rebuild) only when the version string does. --tags: a release
# tagged without -a must still describe as its tag (the updater checks `--version` against it).
VERSION := $(shell git describe --tags --always --dirty 2>/dev/null || echo unknown)
VERSION_STAMP := $(BUILD)/version.stamp
$(BUILD)/main.o $(BUILD)/controller.o: CXXFLAGS += -DPULSAR_VERSION='"$(VERSION)"'
$(BUILD)/main.o $(BUILD)/controller.o: $(VERSION_STAMP)

$(VERSION_STAMP): FORCE
	@mkdir -p $(BUILD)
	@echo '$(VERSION)' | cmp -s - $@ || echo '$(VERSION)' > $@

binary: $(BUILD)/pulsar

$(BUILD)/pulsar.debug: $(OBJS) $(SS_STAMP)
	$(CXX) $(OBJS) -o $@ $(LDFLAGS)

$(BUILD)/pulsar: $(BUILD)/pulsar.debug
	$(ZIG) objcopy --strip-all $< $@

$(BUILD)/%.o: src/%.cpp | $(SS_STAMP) $(SYSROOT_STAMP)
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) -MMD -MP -c $< -o $@

# alsa-lib headers + libasound.so to compile and link against (runtime uses the firmware's copy).
$(SYSROOT_STAMP): third_party/sysroot.sh
	env -u MAKEFLAGS -u MFLAGS third_party/sysroot.sh $(SYSROOT) "$(TARGET) $(CPU)" $(HOST) "$(ALSA)"
	touch $@

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

# Standalone hardware/RE probes (tools/*.cpp), each linked with the core + module sources it needs.
$(BUILD)/tools/%: tools/%.cpp $(filter-out $(BUILD)/main.o,$(OBJS)) $(SS_STAMP)
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) $< $(filter-out $(BUILD)/main.o,$(OBJS)) -o $@ $(LDFLAGS)

.PHONY: binary FORCE
FORCE:
-include $(DEPS)

endif
