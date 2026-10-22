/********************************************************************************************************
 * Zigbee router for the Yandex Station Midi radio. Licensed under the Apache License 2.0 (see
 * common/board_cfg.h); derived from the Telink Zigbee SDK sample gateway (sampleGateway.c,
 * zb_appCb.c, sampleGatewayEpCfg.c).
 *
 * The router does nothing on its own: pulsar tells it over zbhci when to join (0x0002, network
 * steering) and to leave (0x0005, broadcasts a Leave and erases its network). Messages to the
 * host besides the stock zbhci replies and the common ones (common/pulsar_hci.h):
 *   0x8906 [23 bytes]         joined / rejoined a network; layout of the 0x8045 reply
 *   0x8920 [event][status]    event 1: start-up (status 0 = on a network, else none);
 *                             2: commissioning failed (BDB status); 3: left the network
 *******************************************************************************************************/
#include "tl_common.h"
#include "zb_api.h"
#include "zcl_include.h"
#include "bdb.h"
#include "zbhci.h"
#include "pulsar_hci.h"

#define ROUTER_ENDPOINT     0x01
#define HCI_NETWORK_UP      0x8906
#define HCI_EVENT           0x8920
#define EVENT_STARTED       1
#define EVENT_COMMISSIONING 2
#define EVENT_LEFT          3

/**********************************************************************
 * ZCL: Basic and Identify.
 */
typedef struct {
    u8 zclVersion;
    u8 appVersion;
    u8 stackVersion;
    u8 hwVersion;
    u8 manuName[ZCL_BASIC_MAX_LENGTH];
    u8 modelId[ZCL_BASIC_MAX_LENGTH];
    u8 powerSource;
    u8 deviceEnable;
} router_basicAttr_t;

static router_basicAttr_t basicAttrs = {
    .zclVersion   = 0x03,
    .appVersion   = APP_RELEASE,
    .stackVersion = 0x02,
    .hwVersion    = 0x00,
    .manuName     = {6, 'p', 'u', 'l', 's', 'a', 'r'},
    .modelId      = {11, 'M', 'i', 'd', 'i', ' ', 'r', 'o', 'u', 't', 'e', 'r'},
    .powerSource  = POWER_SOURCE_MAINS_1_PHASE,
    .deviceEnable = TRUE,
};

static const zclAttrInfo_t basicAttrTbl[] = {
    {ZCL_ATTRID_BASIC_ZCL_VER, ZCL_DATA_TYPE_UINT8, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.zclVersion},
    {ZCL_ATTRID_BASIC_APP_VER, ZCL_DATA_TYPE_UINT8, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.appVersion},
    {ZCL_ATTRID_BASIC_STACK_VER, ZCL_DATA_TYPE_UINT8, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.stackVersion},
    {ZCL_ATTRID_BASIC_HW_VER, ZCL_DATA_TYPE_UINT8, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.hwVersion},
    {ZCL_ATTRID_BASIC_MFR_NAME, ZCL_DATA_TYPE_CHAR_STR, ACCESS_CONTROL_READ, (u8 *)basicAttrs.manuName},
    {ZCL_ATTRID_BASIC_MODEL_ID, ZCL_DATA_TYPE_CHAR_STR, ACCESS_CONTROL_READ, (u8 *)basicAttrs.modelId},
    {ZCL_ATTRID_BASIC_POWER_SOURCE, ZCL_DATA_TYPE_ENUM8, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.powerSource},
    {ZCL_ATTRID_BASIC_DEV_ENABLED, ZCL_DATA_TYPE_BOOLEAN, ACCESS_CONTROL_READ, (u8 *)&basicAttrs.deviceEnable},
    {ZCL_ATTRID_GLOBAL_CLUSTER_REVISION, ZCL_DATA_TYPE_UINT16, ACCESS_CONTROL_READ, (u8 *)&zcl_attr_global_clusterRevision},
};

static u16 identifyTime;
static const zclAttrInfo_t identifyAttrTbl[] = {
    {ZCL_ATTRID_IDENTIFY_TIME, ZCL_DATA_TYPE_UINT16, ACCESS_CONTROL_READ | ACCESS_CONTROL_WRITE, (u8 *)&identifyTime},
    {ZCL_ATTRID_GLOBAL_CLUSTER_REVISION, ZCL_DATA_TYPE_UINT16, ACCESS_CONTROL_READ, (u8 *)&zcl_attr_global_clusterRevision},
};

static const zcl_specClusterInfo_t clusterList[] = {
    {ZCL_CLUSTER_GEN_BASIC, MANUFACTURER_CODE_NONE, sizeof(basicAttrTbl) / sizeof(zclAttrInfo_t), basicAttrTbl,
     zcl_basic_register, NULL},
    {ZCL_CLUSTER_GEN_IDENTIFY, MANUFACTURER_CODE_NONE, sizeof(identifyAttrTbl) / sizeof(zclAttrInfo_t),
     identifyAttrTbl, zcl_identify_register, NULL},
};

static const u16 inClusters[] = {ZCL_CLUSTER_GEN_BASIC, ZCL_CLUSTER_GEN_IDENTIFY};

static const af_simple_descriptor_t simpleDesc = {
    HA_PROFILE_ID,
    HA_DEV_RANGE_EXTENDER,
    ROUTER_ENDPOINT,
    0,
    0,
    sizeof(inClusters) / sizeof(inClusters[0]),
    0,
    (u16 *)inClusters,
    NULL,
};

static void zclIncoming(zclIncoming_t *msg) { (void)msg; }

/**********************************************************************
 * Host messages.
 */
static void hciEvent(u8 event, u8 status)
{
    u8 b[2] = {event, status};
    zbhciTx(HCI_EVENT, sizeof b, b);
}

// Same layout as the stock 0x8045 reply (zbhciCmdProcess.c), without the channel byte the
// gateway firmware leaves out too.
static void hciNetworkUp(void)
{
    u8 b[23];
    u8 *p = b;
    *p++ = af_nodeDevTypeGet();
    *p++ = af_nodeMacCapabilityGet();
    *p++ = g_bdbAttrs.nodeIsOnANetwork;
    COPY_U16TOBUFFER_BE(p, NIB_PAN_ID());
    p += 2;
    ZB_IEEE_ADDR_REVERT(p, NWK_NIB().extPANId);
    p += 8;
    COPY_U16TOBUFFER_BE(p, NIB_NETWORK_ADDRESS());
    p += 2;
    ZB_IEEE_ADDR_REVERT(p, NIB_IEEE_ADDRESS());
    zbhciTx(HCI_NETWORK_UP, sizeof b, b);
}

// zbhciCmdHandler ACKs the stock commands itself.
static void hciCmd(u16 type, u16 len, u8 *p)
{
    if (type == HCI_APP_INFO_REQ) {
        hciAppInfo();
        return;
    }
    zbhciCmdHandler(type, len, p);
}

/**********************************************************************
 * Stack callbacks.
 */
static void bdbInitCb(u8 status, u8 joinedNetwork)
{
    // Factory new: wait for the host's 0x0002. On a network: the stack rejoins by itself.
    hciEvent(EVENT_STARTED, (status == BDB_INIT_STATUS_SUCCESS && joinedNetwork) ? 0 : 1);
}

static void bdbCommissioningCb(u8 status, void *arg)
{
    (void)arg;
    if (status == BDB_COMMISSION_STA_SUCCESS) {
        hciNetworkUp();
    } else if (status != BDB_COMMISSION_STA_IN_PROGRESS) {
        hciEvent(EVENT_COMMISSIONING, status);
    }
}

static bdb_appCb_t bdbCb = {bdbInitCb, bdbCommissioningCb, NULL, NULL};

static void leaveCnf(nlme_leave_cnf_t *cnf)
{
    hciEvent(EVENT_LEFT, cnf->status);
}

static const zdo_appIndCb_t zdoCb = {
    bdb_zdoStartDevCnf, NULL, NULL, NULL, leaveCnf, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
};

void user_init(bool isRetention)
{
    (void)isRetention;
    zb_init();
    zb_zdoCbRegister((zdo_appIndCb_t *)&zdoCb);

    af_nodeDescManuCodeUpdate(MANUFACTURER_CODE);
    zcl_init(zclIncoming);
    af_endpointRegister(ROUTER_ENDPOINT, (af_simple_descriptor_t *)&simpleDesc, zcl_rx_handler, NULL);
    zcl_register(ROUTER_ENDPOINT, sizeof(clusterList) / sizeof(clusterList[0]), (zcl_specClusterInfo_t *)clusterList);

    hciStart(hciCmd);
    bdb_init((af_simple_descriptor_t *)&simpleDesc, &g_commissionSetting, &bdbCb, 1);
}
