/********************************************************************************************************
 * Host-link pieces shared by pulsar's radio firmwares. Licensed under the Apache License 2.0 (see
 * board_cfg.h).
 *******************************************************************************************************/
#include "pulsar_hci.h"
#include "zb_api.h"
#include "zbhci.h"

extern u32 BIN_SIZE;  // linker symbol: image size

void hciAck(u16 type, u8 status)
{
    u8 ack[4];
    COPY_U16TOBUFFER_BE(ack, type);
    ack[2] = status;
    ack[3] = 0;
    zbhciTx(ZBHCI_CMD_ACKNOWLEDGE, sizeof ack, ack);
}

void hciAppInfo(void)
{
    hciAck(HCI_APP_INFO_REQ, ACK_OK);
    u8 b[12];
    COPY_U32TOBUFFER_BE(b, FILE_VERSION);
    COPY_U16TOBUFFER_BE(b + 4, MANUFACTURER_CODE);
    COPY_U16TOBUFFER_BE(b + 6, IMAGE_TYPE);
    COPY_U32TOBUFFER_BE(b + 8, (u32)&BIN_SIZE);
    zbhciTx(HCI_APP_INFO_RSP, sizeof b, b);
}

static void sysException(void)
{
    SYSTEM_RESET();
}

void hciStart(void (*cmd)(u16 type, u16 len, u8 *p))
{
    sys_exceptHandlerRegister(sysException);
    zbhciInit(cmd);
    ev_on_poll(EV_POLL_HCI, zbhciTask);
    u8 boot[4] = {0};
    zbhciTx(HCI_BOOT_IND, sizeof boot, boot);
}

bdb_commissionSetting_t g_commissionSetting = {
    .linkKey.tcLinkKey.keyType = SS_GLOBAL_LINK_KEY,
    .linkKey.tcLinkKey.key = (u8 *)tcLinkKeyCentralDefault,
    .linkKey.distributeLinkKey.keyType = MASTER_KEY,
    .linkKey.distributeLinkKey.key = (u8 *)linkKeyDistributedMaster,
    .linkKey.touchLinkKey.keyType = MASTER_KEY,
    .linkKey.touchLinkKey.key = (u8 *)touchLinkKeyMaster,
    .touchlinkEnable = 0,
};
