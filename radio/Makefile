# Zigbee firmware for the Midi's Telink B91 radio, from the Telink Zigbee SDK (Apache-2.0): router/
# (a router in another network) and coordinator/ (network co-processor for pulsar's deCONZ server),
# both with common/ (board configuration, host-link pieces). Run through build.sh, which fetches the
# pinned SDK and toolchain and sets SDK / TC.
# Output: out/zigbee-router.ybl, out/zigbee-coordinator.ybl
SDK ?= src/telink_zigbee_sdk/tl_zigbee_sdk
TC  ?= toolchain/nds32le-elf-mculib-v5f/bin/riscv32-elf-
ZIG ?= zig
OUT := out

DIRS := $(SDK)/apps/common $(SDK)/apps/zigbee/sampleCommon $(SDK)/platform/chip_b91 $(SDK)/proj $(SDK)/stack/common \
        $(SDK)/stack/hci $(SDK)/stack/zigbee $(SDK)/stack/ieee802154
SDK_SRCS := $(shell find $(DIRS) -name '*.c') $(SDK)/platform/boot/b91/cstartup_b91.S $(SDK)/platform/boot/link_cfg.S \
            $(SDK)/platform/services/b91/irq_handler.c $(SDK)/platform/services/b91/plic_isr.c
INC := $(SDK)/apps/common $(SDK)/apps/zigbee/sampleCommon $(SDK)/platform/chip_b91 $(SDK)/platform/riscv \
       $(SDK)/platform $(SDK)/proj/common $(SDK)/proj $(SDK)/stack/common/includes $(SDK)/stack/ieee802154/mac/includes \
       $(SDK)/stack/ieee802154/phy $(SDK)/stack/zigbee/common/includes $(SDK)/stack/zigbee/mac $(SDK)/stack/zigbee/zbapi \
       $(SDK)/stack/zigbee/bdb/includes $(SDK)/stack/zigbee/gp $(SDK)/stack/zigbee/zcl $(SDK)/stack/zigbee/ota \
       $(SDK)/stack/zigbee/wwah $(SDK)/stack/hci $(SDK)/stack/zigbee/zb_hci
# Flags of the SDK's own B91 projects (build/*/b91/.cproject).
FLAGS := -mext-dsp -mabi=ilp32f -fomit-frame-pointer -fno-strict-aliasing -fuse-ld=bfd -fpack-struct -fshort-enums \
         -fshort-wchar -O2 -flto
CFLAGS := $(FLAGS) -ffunction-sections -fdata-sections -std=gnu99 -Wall -Wno-nonnull-compare -DMCU_CORE_B91=1 \
          -DZB_STACK_USED=1

all: $(OUT)/zigbee-router.ybl $(OUT)/zigbee-coordinator.ybl

$(OUT)/ybl: ybl.c
	@mkdir -p $(OUT)
	$(ZIG) cc -O2 -o $@ $<

# $(1): application directory, $(2): role define, $(3): stack library. The SDK is compiled per
# application: its sources depend on the role and on the application's configuration headers.
define image
$(1)_OBJS := $$(patsubst $$(SDK)/%,$$(OUT)/$(1)/sdk/%.o,$$(SDK_SRCS)) $$(patsubst %,$$(OUT)/%.o,$$(wildcard $(1)/*.c)) \
             $$(patsubst common/%,$$(OUT)/$(1)/common/%.o,$$(wildcard common/*.c))
$(1)_CFLAGS := $$(CFLAGS) -D$(2)=1 $$(addprefix -I,$(1) common $$(INC))
$(1)_HDRS := $$(wildcard $(1)/*.h common/*.h)

$$(OUT)/$(1)/sdk/%.o: $$(SDK)/% $$($(1)_HDRS)
	@mkdir -p $$(dir $$@)
	@$$(TC)gcc $$($(1)_CFLAGS) -c $$< -o $$@

$$(OUT)/$(1)/common/%.c.o: common/%.c $$($(1)_HDRS)
	@mkdir -p $$(dir $$@)
	$$(TC)gcc $$($(1)_CFLAGS) -c $$< -o $$@

$$(OUT)/$(1)/%.c.o: $(1)/%.c $$($(1)_HDRS)
	@mkdir -p $$(dir $$@)
	$$(TC)gcc $$($(1)_CFLAGS) -c $$< -o $$@

$$(OUT)/$(1).elf: $$($(1)_OBJS)
	$$(TC)gcc $$(FLAGS) -nostartfiles -Wl,--gc-sections -T $$(SDK)/platform/boot/b91/boot_b91.link $$^ \
	  -L$$(SDK)/platform/lib -L$$(SDK)/stack/zigbee/lib/riscv_gcc7_v5f \
	  -Wl,--start-group -l$(3) -ldrivers_b91 -Wl,--end-group -Wl,-Map=$$(OUT)/$(1).map -o $$@
	$$(TC)size $$@

$$(OUT)/$(1).bin: $$(OUT)/$(1).elf
	$$(TC)objcopy -O binary $$< $$@

$$(OUT)/zigbee-$(1).ybl: $$(OUT)/$(1).bin $$(OUT)/ybl
	$$(OUT)/ybl $$< $$@
endef

$(eval $(call image,router,ROUTER,zb_router))
$(eval $(call image,coordinator,COORDINATOR,zb_coordinator))

clean:
	rm -rf $(OUT)

.PHONY: all clean
