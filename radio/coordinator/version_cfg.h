/********************************************************************************************************
 * Image header and flash layout of the coordinator firmware. Licensed under the Apache License 2.0
 * (see common/board_cfg.h, which also has the radio's flash map).
 *******************************************************************************************************/
#pragma once

#include "comm_cfg.h"

#define CHIP_TYPE                       TLSR_B91
#define FLASH_CAP_SIZE_1M               1

/* Header fields the station's resident bootloader and maind check in a .ybl: Yandex's
 * manufacturer code and image type (B91, gateway, bootloader layout), as in gateway.ybl. The file
 * version differs from every gateway.ybl, so the Yandex stack, if it runs again, sees a mismatch
 * and puts its own firmware back. 0x51: pulsar coordinator (0x50 router). */
#define APP_RELEASE                     0x51
#define APP_BUILD                       0x09
#define STACK_RELEASE                   0x40
#define STACK_BUILD                     0x01
#define MANUFACTURER_CODE               0x140A
#define IMAGE_TYPE                      0x0580
#define FILE_VERSION                    ((APP_RELEASE << 24) | (APP_BUILD << 16) | (STACK_RELEASE << 8) | STACK_BUILD)

/* Linked behind the station's 32 KB resident bootloader. */
#define IS_BOOT_LOADER_IMAGE            0
#define RESV_FOR_APP_RAM_CODE_SIZE      0
#define IMAGE_OFFSET                    0x8000

/* NV of this firmware's network (see the flash map in common/board_cfg.h). */
#define NV_BASE_ADDRESS_ZB              0xA8000
#define CFG_FACTORY_RST_CNT             0xBE000
#define CFG_PRE_INSTALL_CODE            0xBF000
