/********************************************************************************************************
 * Host-link pieces shared by pulsar's radio firmwares. Licensed under the Apache License 2.0 (see
 * board_cfg.h).
 *
 * Every frame from the host gets exactly one 0x8000 ACK [type BE16][status][0], as from the Yandex
 * gateway firmware (the host sends the next frame only after it). Messages common to all images,
 * big endian like the gateway's (the host identifies the running image with them):
 *   0x8907 [0 0 0 0]          application started
 *   0x0900                    → 0x8900 [version BE32][manufacturer BE16][image type BE16][size BE32]
 *******************************************************************************************************/
#pragma once

#include "tl_common.h"
#include "zb_api.h"
#include "bdb.h"

#define HCI_BOOT_IND        0x8907
#define HCI_APP_INFO_REQ    0x0900
#define HCI_APP_INFO_RSP    0x8900

#define ACK_OK              0
#define ACK_BAD_PARAMS      1
#define ACK_UNKNOWN         2
#define ACK_BUSY            3

void hciAck(u16 type, u8 status);
// ACKs 0x0900 and answers it.
void hciAppInfo(void);
// Exception handler, host link (cmd handles every frame) and the boot indication.
void hciStart(void (*cmd)(u16 type, u16 len, u8 *p));

// Global trust center link key "ZigBeeAlliance09", no touchlink.
extern bdb_commissionSetting_t g_commissionSetting;
