/********************************************************************************************************
 * Configuration shared by pulsar's radio firmwares (router/, coordinator/) for the Yandex Station
 * Midi radio: a Telink B91 on the station's /dev/ttyS2, driven by pulsar over zbhci. Based on the
 * Telink Zigbee SDK sample gateway (apps/zigbee/sampleGW).
 *
 * Copyright (c) 2021, Telink Semiconductor (Shanghai) Co., Ltd. ("TELINK"); pulsar changes are
 * under the same license.
 *
 *          Licensed under the Apache License, Version 2.0 (the "License");
 *          you may not use this file except in compliance with the License.
 *          You may obtain a copy of the License at
 *
 *              http://www.apache.org/licenses/LICENSE-2.0
 *
 *          Unless required by applicable law or agreed to in writing, software
 *          distributed under the License is distributed on an "AS IS" BASIS,
 *          WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *          See the License for the specific language governing permissions and
 *          limitations under the License.
 *
 * Flash layout (1 MB): Yandex bootloader 0..0x7FFF (never written), the application at 0x8000
 * (whichever image the bootloader got last), Yandex gateway OTA staging area 0x77000..0xE5FFF
 * (written by the gateway only when its host asks), in which the router's NV lives at
 * 0xC0000..0xD7FFF and the coordinator's at 0xA8000..0xBFFFF; the gateway's NV 0xE6000..0xFBFFF,
 * calibration 0xFE000, MAC address 0xFF000 (shared). Each firmware keeps its own network, so
 * switching images keeps all of them.
 *******************************************************************************************************/
#pragma once

#define ZB_TEST_ENABLE                          0

#define RF_ENABLE                               1
#define PA_ENABLE                               0

#define GSUART_PRINTF_MODE                      0
#define UART_PRINTF_MODE                        0

/* Host link: zbhci over UART0, the only interface the station wires to the chip. */
#define ZBHCI_UART                              1
#define ZBHCI_USB_CDC                           0
#define ZBHCI_USB_HID                           0
#define ZBHCI_EN                                1
#define MODULE_UART_ENABLE                      1

#define VOLTAGE_DETECT_ENABLE                   0

/* Off: the flash status-register protection outlives this firmware and could keep the station's
 * resident bootloader from writing the next image. */
#define FLASH_PROTECT_ENABLE                    0

#define MODULE_WATCHDOG_ENABLE                  0

/* BDB opens joining network-wide for 180 s after forming or joining a network (sdk.patch makes it
 * optional): joining is the decision of the network's coordinator host. */
#define BDB_NO_AUTO_PERMIT_JOIN                 1

/**********************************************************************
 * Board: the station. UART0 TX/RX on PB2/PB3 at 115200 (what the Yandex gateway firmware uses);
 * no LEDs, buttons or other pins are touched.
 */
#define CLOCK_SYS_CLOCK_HZ                      48000000
#define UART_IDX                                UART0
#define UART_TX_PIN                             UART0_TX_PB2
#define UART_RX_PIN                             UART0_RX_PB3
#define UART_PIN_CFG()                          drv_uart_pin_set(UART_TX_PIN, UART_RX_PIN)

#include "version_cfg.h"
#include "stack_cfg.h"

/**********************************************************************
 * ZCL: the cluster modules are compiled because the stock zbhci command code refers to them. The
 * host never sends those commands (nor 0x0210 HCI OTA, which would write the OTA area over the
 * NV).
 */
#define ZCL_ON_OFF_SUPPORT                      1
#define ZCL_LEVEL_CTRL_SUPPORT                  1
#define ZCL_LIGHT_COLOR_CONTROL_SUPPORT         1
#define ZCL_GROUP_SUPPORT                       1
#define ZCL_SCENE_SUPPORT                       1
#define ZCL_OTA_SUPPORT                         1
#define ZCL_GP_SUPPORT                          0
#define AF_TEST_ENABLE                          0

typedef enum {
    EV_POLL_ED_DETECT,
    EV_POLL_HCI,
    EV_POLL_IDLE,
    EV_POLL_MAX,
} ev_poll_e;
