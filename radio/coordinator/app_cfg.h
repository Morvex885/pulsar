/********************************************************************************************************
 * Zigbee coordinator (network co-processor) for the Yandex Station Midi radio. Licensed under the
 * Apache License 2.0 (see common/board_cfg.h). No ZCL in the chip: the host answers ZCL for the
 * coordinator's endpoints.
 *******************************************************************************************************/
#pragma once

/* One zbhci frame per buffer: an APS data request or indication with a full-size ASDU and its
 * addressing does not fit the stock 128 bytes. uart_txMsg takes payloads up to 256 - 7; the host
 * (TelinkLink kMaxFrame) sends at most 0xF9. */
#define UART_TX_BUF_SIZE                        256
#define UART_RX_BUF_SIZE                        256

/* Groupcasts to groups the stack knows no endpoint for go to the host anyway (sdk.patch,
 * aps_group.c), as from a ConBee: hosts keep the coordinator's group memberships themselves. */
#define APS_GROUP_HOST_FALLBACK                 1

#include "board_cfg.h"
