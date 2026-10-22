/********************************************************************************************************
 * Zigbee coordinator (network co-processor) for the Yandex Station Midi radio. Licensed under the
 * Apache License 2.0 (see common/board_cfg.h); derived from the Telink Zigbee SDK sample gateway
 * (sampleGateway.c, zb_appCb.c).
 *
 * The chip runs the Zigbee stack (formation, trust center, routing, ZDO answers for itself); the
 * host chooses the network and handles everything above APS. Framing, ACK rule and the 0x0900 /
 * 0x8907 messages: common/pulsar_hci.h. Multi-byte fields of the 0x0Axx / 0x8Axx messages are
 * little endian, as on air.
 *
 * Host -> chip:
 *   0x0A00                    → 0x8A00 network state
 *   0x0A01 [channel][PAN][ext PAN 8][key 16][IEEE 8][frame counter 4][update id]
 *                             form a network with these (no network may exist: 0x0A02 first);
 *                             IEEE 0 = the chip's own, PAN 0xFFFF / ext PAN 0 / key 0 = chosen by
 *                             the stack; the stack moves past a PAN ID it hears on the channel.
 *                             Accepted: 0x8A00 follows at once (state 1, or 0 with the BDB status
 *                             when the stack refused), then the result (state 2, or 0).
 *   0x0A02                    erase the network (NV), then 0x8A00
 *   0x0A03 [seconds]          permit joining on the coordinator itself (0 closes)
 *   0x0A04 [ep][profile][device][version][n in][in…][n out][out…]   add / replace an endpoint
 *   0x0A04 [ep][0xFF]         remove an endpoint
 *   0x0A05 [channel][update id]   move the network: broadcast Mgmt_NWK_Update_req, switch after 10 s
 *   0x0A06 [channel mask 4][duration exponent]   energy scan → 0x8A06 [count]([channel][energy])…
 *   0x0A10 [handle][dst mode][dst 2|8][dst ep unless group][profile][cluster][src ep][tx options]
 *          [radius][length][asdu]   APS data request (dst mode 1 group, 2 NWK, 3 IEEE; source
 *                             endpoint 0 or a registered one; tx options as APS: 0x04 ACK);
 *                             → exactly one 0x8A10 [handle][status]
 * Chip -> host:
 *   0x8A00 [state][status][channel][PAN][ext PAN 8][IEEE 8][NWK][update id][key 16][key seq]
 *          [frame counter 4]  state 0 none, 1 forming, 2 on the network; status: last BDB result.
 *                             Sent on request and on every change.
 *   0x8A11 [dst mode][dst][dst ep][src mode][src 2|8][src ep][profile][cluster][lqi][rssi]
 *          [security][length][asdu]   APS data indication: every frame for a registered endpoint,
 *                             every groupcast, and every ZDO frame (the stack answers ZDO
 *                             requests itself too)
 *   0x8A20 [event]...         1 device joined / rejoined [NWK][IEEE 8][parent IEEE 8][status],
 *                             2 device left [IEEE 8][rejoin], 3 joining permitted [seconds]
 *                             (requested by the host or by a Mgmt_Permit_Joining_req)
 *******************************************************************************************************/
#include "tl_common.h"
#include "zb_api.h"
#include "zcl_include.h"
#include "bdb.h"
#include "zbhci.h"
#include "pulsar_hci.h"

#define HCI_NET_GET         0x0A00
#define HCI_NET_FORM        0x0A01
#define HCI_NET_ERASE       0x0A02
#define HCI_PERMIT_JOIN     0x0A03
#define HCI_ENDPOINT        0x0A04
#define HCI_CHANNEL         0x0A05
#define HCI_ED_SCAN         0x0A06
#define HCI_DATA_REQ        0x0A10
#define HCI_NET_STATE       0x8A00
#define HCI_ED_SCAN_RSP     0x8A06
#define HCI_DATA_CNF        0x8A10
#define HCI_DATA_IND        0x8A11
#define HCI_EVENT           0x8A20

#define STATE_NONE          0
#define STATE_FORMING       1
#define STATE_UP            2

#define EVENT_JOINED        1
#define EVENT_LEFT          2
#define EVENT_PERMIT        3

#define MAX_ENDPOINTS       (MAX_ACTIVE_EP_NUMBER)
#define MAX_EP_CLUSTERS     32
#define MAX_PENDING         16
#define PENDING_MAX_AGE_US  (60 * 1000 * 1000)  /* the host gives up after 50 s */
#define NV_ITEM_IEEE        0x40    /* NV_MODULE_APP: host-chosen coordinator IEEE address */

// Stack library (no header): store a network key in material slot i / make slot i the active one.
// zb_preConfigNwkKey calls these too, but also flags the key as pre-configured
// (ss_ib.preConfiguredKeyType |= SS_PRECONFIGURED_NWKKEY): the trust center then never transports
// it, and joining devices never get it.
void ss_zdoNwkKeyConfigure(u8 *key, u8 i, u8 keyType);
void ss_zdoUseKey(u8 i);

static u8 *put16(u8 *p, u16 v)
{
    p[0] = (u8)v;
    p[1] = (u8)(v >> 8);
    return p + 2;
}

static u8 *put32(u8 *p, u32 v)
{
    p = put16(p, (u16)v);
    return put16(p, (u16)(v >> 16));
}

static void hciEvent(u8 event, const u8 *p, u8 n)
{
    u8 b[24];
    b[0] = event;
    memcpy(b + 1, p, n);
    zbhciTx(HCI_EVENT, 1 + n, b);
}

/**********************************************************************
 * Network state.
 */
static u8 lastStatus;   // last BDB init / commissioning status
static bool forming;
static u8 scanChannel;  // channel under energy scan, 0: none
static u8 scanSavedChannel;

static void sendState(void)
{
    u8 b[45];
    u8 *p = b;
    const bool up = g_bdbAttrs.nodeIsOnANetwork;
    *p++ = up ? STATE_UP : forming ? STATE_FORMING : STATE_NONE;
    *p++ = lastStatus;
    *p++ = scanChannel ? scanSavedChannel : MAC_IB()->phyChannelCur;
    p = put16(p, up ? NIB_PAN_ID() : 0xFFFF);
    memcpy(p, NWK_NIB().extPANId, 8);
    p += 8;
    memcpy(p, NIB_IEEE_ADDRESS(), 8);
    p += 8;
    p = put16(p, up ? NIB_NETWORK_ADDRESS() : 0xFFFE);
    *p++ = NWK_NIB().updateId;
    const ss_material_set_t *m = &ss_ib.nwkSecurMaterialSet[ss_ib.activeSecureMaterialIndex];
    memcpy(p, m->key, 16);
    p += 16;
    *p++ = m->keySeqNum;
    p = put32(p, ss_ib.outgoingFrameCounter);
    zbhciTx(HCI_NET_STATE, p - b, b);
}

/**********************************************************************
 * Endpoints: descriptors live here, in fixed slots (the stack keeps pointers to them).
 */
typedef struct {
    bool used;
    af_simple_descriptor_t desc;
    u16 clusters[MAX_EP_CLUSTERS];
} endpoint_t;

static endpoint_t endpoints[MAX_ENDPOINTS];

static void epRx(void *arg);
static void dataCnf(void *arg);

static endpoint_t *findEndpoint(u8 ep)
{
    for (u8 i = 0; i < MAX_ENDPOINTS; i++) {
        if (endpoints[i].used && endpoints[i].desc.endpoint == ep) {
            return &endpoints[i];
        }
    }
    return NULL;
}

static u8 setEndpoint(const u8 *p, u16 len)
{
    if (len < 2 || p[0] == 0 || p[0] == 0xFF) {
        return ACK_BAD_PARAMS;
    }
    const u8 ep = p[0];
    endpoint_t *e = findEndpoint(ep);
    if (len == 2 && p[1] == 0xFF) {
        if (e) {
            af_endpointUnregister(ep);
            e->used = FALSE;
        }
        return ACK_OK;
    }
    if (len < 8) {
        return ACK_BAD_PARAMS;
    }
    const u8 nin = p[6];
    if (len < 8 + 2 * nin) {
        return ACK_BAD_PARAMS;
    }
    const u8 nout = p[7 + 2 * nin];
    if (len < 8 + 2 * (nin + nout) || nin + nout > MAX_EP_CLUSTERS) {
        return ACK_BAD_PARAMS;
    }
    const bool added = e == NULL;
    for (u8 i = 0; i < MAX_ENDPOINTS && !e; i++) {
        if (!endpoints[i].used) {
            e = &endpoints[i];
        }
    }
    if (!e) {
        return ACK_BUSY;
    }
    af_simple_descriptor_t *d = &e->desc;
    d->endpoint = ep;
    d->app_profile_id = BUILD_U16(p[1], p[2]);
    d->app_dev_id = BUILD_U16(p[3], p[4]);
    d->app_dev_ver = p[5] & 0x0F;
    d->app_in_cluster_count = nin;
    d->app_out_cluster_count = nout;
    for (u8 i = 0; i < nin; i++) {
        e->clusters[i] = BUILD_U16(p[7 + 2 * i], p[8 + 2 * i]);
    }
    for (u8 i = 0; i < nout; i++) {
        e->clusters[nin + i] = BUILD_U16(p[8 + 2 * nin + 2 * i], p[9 + 2 * nin + 2 * i]);
    }
    d->app_in_cluster_lst = e->clusters;
    d->app_out_cluster_lst = e->clusters + nin;
    if (added) {
        if (!af_endpointRegister(ep, d, epRx, dataCnf)) {
            return ACK_BUSY;
        }
        e->used = TRUE;
    }
    return ACK_OK;
}

// Called by aps_group.c (sdk.patch, APS_GROUP_HOST_FALLBACK) for a group no endpoint is a member
// of: one endpoint gets it, so the host sees every groupcast once.
u8 *aps_groupHostFallbackEp(u8 *epNum)
{
    for (u8 i = 0; i < MAX_ENDPOINTS; i++) {
        if (endpoints[i].used) {
            *epNum = 1;
            return &endpoints[i].desc.endpoint;
        }
    }
    return NULL;
}

/**********************************************************************
 * APS data: requests in flight are matched to their confirm by (source endpoint, APS counter).
 * The stack confirms only to endpoint 0 and registered endpoints; entries whose confirm never came
 * are reused after a minute.
 */
typedef struct {
    bool used;
    u8 handle;
    u8 srcEp;
    u8 apsCnt;
    u32 tick;
} pending_t;

static pending_t pending[MAX_PENDING];

static void dataResult(u8 handle, u8 status)
{
    u8 b[2] = {handle, status};
    zbhciTx(HCI_DATA_CNF, sizeof b, b);
}

static void dataCnf(void *arg)
{
    apsdeDataConf_t *c = (apsdeDataConf_t *)arg;
    for (u8 i = 0; i < MAX_PENDING; i++) {
        pending_t *q = &pending[i];
        if (q->used && q->apsCnt == c->apsCnt && q->srcEp == c->srcEndpoint) {
            q->used = FALSE;
            dataResult(q->handle, c->status);
            return;
        }
    }
}

static af_dataCnf_cb_t zdoCnfOrig;
static void zdoCnf(void *arg)
{
    dataCnf(arg);
    if (zdoCnfOrig) {
        zdoCnfOrig(arg);
    }
}

static pending_t *freePending(void)
{
    for (u8 k = 0; k < MAX_PENDING; k++) {
        pending_t *q = &pending[k];
        if (!q->used || clock_time_exceed(q->tick, PENDING_MAX_AGE_US)) {
            return q;
        }
    }
    return NULL;
}

static void dataRequest(const u8 *p, u16 len)
{
    // [handle][mode][dst 2|8][ep unless group] then 8 bytes, then the ASDU
    const u8 mode = len >= 2 ? p[1] : 0;
    const u16 head = mode == APS_SHORT_GROUPADDR_NOEP ? 4 : mode == APS_SHORT_DSTADDR_WITHEP ? 5 : 11;
    if (len < 2 || mode < APS_SHORT_GROUPADDR_NOEP || mode > APS_LONG_DSTADDR_WITHEP || len < head + 8 ||
        len < head + 8 + p[head + 7]) {
        hciAck(HCI_DATA_REQ, ACK_BAD_PARAMS);
        return;
    }
    hciAck(HCI_DATA_REQ, ACK_OK);
    const u8 handle = p[0];
    epInfo_t dst;
    TL_SETSTRUCTCONTENT(dst, 0);
    dst.dstAddrMode = mode;
    if (mode == APS_LONG_DSTADDR_WITHEP) {
        memcpy(dst.dstAddr.extAddr, p + 2, 8);
    } else {
        dst.dstAddr.shortAddr = BUILD_U16(p[2], p[3]);
    }
    if (mode != APS_SHORT_GROUPADDR_NOEP) {
        dst.dstEp = p[head - 1];
    }
    const u8 *q8 = p + head;
    dst.profileId = BUILD_U16(q8[0], q8[1]);
    const u16 cluster = BUILD_U16(q8[2], q8[3]);
    const u8 srcEp = q8[4];
    dst.txOptions = q8[5];
    dst.radius = q8[6];
    const u8 n = q8[7];
    u8 *asdu = (u8 *)q8 + 8;

    if (!g_bdbAttrs.nodeIsOnANetwork || (srcEp != 0 && !findEndpoint(srcEp))) {
        dataResult(handle, APS_STATUS_ILLEGAL_REQUEST);
        return;
    }
    pending_t *q = freePending();
    if (!q) {
        dataResult(handle, APS_STATUS_INTERNAL_BUF_FULL);
        return;
    }
    u8 apsCnt = 0;
    const u8 ret = af_dataSend(srcEp, &dst, cluster, n, asdu, &apsCnt);
    if (ret != RET_OK) {
        dataResult(handle, ret);
        return;
    }
    q->used = TRUE;
    q->handle = handle;
    q->srcEp = srcEp;
    q->apsCnt = apsCnt;
    q->tick = clock_time();
}

static void sendIndication(const aps_data_ind_t *ind, const u8 *asdu, u16 len)
{
    u8 b[UART_TX_BUF_SIZE - 8];
    u8 *p = b;
    if (len > sizeof b - 24) {
        return;
    }
    *p++ = ind->dst_addr_mode;
    p = put16(p, ind->dst_addr);
    *p++ = ind->dst_ep;
    *p++ = ind->src_addr_mode;
    if (ind->src_addr_mode == APS_LONG_DSTADDR_WITHEP) {
        memcpy(p, ind->src_ext_addr, 8);
        p += 8;
    } else {
        p = put16(p, ind->src_short_addr);
    }
    *p++ = ind->src_ep;
    p = put16(p, ind->profile_id);
    p = put16(p, ind->cluster_id);
    *p++ = ind->lqi;
    *p++ = (u8)ind->rssi;
    *p++ = ind->security_status;
    *p++ = (u8)len;
    memcpy(p, asdu, len);
    p += len;
    zbhciTx(HCI_DATA_IND, p - b, b);
}

// Endpoints other than ZDO: an ev_buf apsdeDataInd_t, owned here.
static void epRx(void *arg)
{
    apsdeDataInd_t *ind = (apsdeDataInd_t *)arg;
    sendIndication(&ind->indInfo, ind->asdu, ind->asduLen);
    ev_buf_free((u8 *)arg);
}

// ZDO: a zb_buf aps_data_ind_t, passed on to the stack's ZDO handler, which frees it.
static af_endpoint_cb_t zdoRxOrig;
static void zdoRx(void *arg)
{
    aps_data_ind_t *ind = (aps_data_ind_t *)arg;
    sendIndication(ind, ind->asdu, ind->asduLength);
    zdoRxOrig(arg);
}

/**********************************************************************
 * Network.
 */
typedef struct {
    u8 channel;
    u16 pan;
    extPANId_t extPan;
    u8 key[16];
    addrExt_t ieee;
    u32 frameCounter;
    u8 updateId;
} formParams_t;

static formParams_t form;
static addrExt_t factoryIeee;
static ev_timer_event_t *channelTimer;

static void applyIeee(const addrExt_t ieee)
{
    memcpy(g_zbMacPib.extAddress, ieee, 8);
}

static void formNetwork(const u8 *p, u16 len)
{
    if (len < 40 || p[0] < 11 || p[0] > 26) {
        hciAck(HCI_NET_FORM, ACK_BAD_PARAMS);
        return;
    }
    if (g_bdbAttrs.nodeIsOnANetwork || forming || scanChannel) {
        hciAck(HCI_NET_FORM, ACK_BUSY);
        return;
    }
    hciAck(HCI_NET_FORM, ACK_OK);
    form.channel = p[0];
    form.pan = BUILD_U16(p[1], p[2]);
    memcpy(form.extPan, p + 3, 8);
    memcpy(form.key, p + 11, 16);
    memcpy(form.ieee, p + 27, 8);
    form.frameCounter = BUILD_U32(p[35], p[36], p[37], p[38]);
    form.updateId = p[39];

    static const addrExt_t zero = {0};
    if (memcmp(form.ieee, zero, 8) && memcmp(form.ieee, factoryIeee, 8)) {
        nv_flashWriteNew(1, NV_MODULE_APP, NV_ITEM_IEEE, 8, form.ieee);
        applyIeee(form.ieee);
    } else {
        nv_flashSingleItemRemove(NV_MODULE_APP, NV_ITEM_IEEE, 8);
        applyIeee(factoryIeee);
    }
    // Formation takes the NIB's PAN ID unless it is 0xFFFF (then random), moving past PAN IDs it
    // hears on the channel (nwk_formationScanCnfHandler in the stack library).
    NWK_NIB().panId = form.pan;
    zb_apsExtPanidSet(form.extPan);
    NWK_NIB().updateId = form.updateId;
    static const u8 noKey[16] = {0};
    if (memcmp(form.key, noKey, 16)) {  // all zero: the stack generates one
        ss_zdoNwkKeyConfigure(form.key, 0, 1);
        ss_zdoUseKey(0);
    }
    if (form.frameCounter > ss_ib.outgoingFrameCounter) {
        ss_ib.outgoingFrameCounter = form.frameCounter;
        nv_nwkFrameCountSaveToFlash(form.frameCounter);
    }
    const u32 mask = 1u << form.channel;
    g_bdbAttrs.primaryChannelSet = mask & BDBC_TL_PRIMARY_CHANNEL_SET;
    g_bdbAttrs.secondaryChannelSet = mask & BDBC_TL_SECONDARY_CHANNEL_SET;
    // Returns the BDB state: formation under way, or idle when the stack refused (busy).
    forming = bdb_networkFormationStart() == BDB_STATE_COMMISSIONING_NETWORK_FORMATION;
    if (!forming) {
        lastStatus = BDB_COMMISSION_STA_FORMATION_FAILURE;
    }
    sendState();
}

static void eraseNetwork(void)
{
    hciAck(HCI_NET_ERASE, ACK_OK);
    forming = FALSE;
    if (channelTimer) {
        TL_ZB_TIMER_CANCEL(&channelTimer);
    }
    // The stack restarts its APS counter: confirms of the old network will not come.
    memset(pending, 0, sizeof pending);
    // NV erased (the outgoing frame counter is kept), the stack and BDB restarted: bdbInitCb
    // reports the empty state.
    zb_resetDevice2FN();
}

static void permitJoin(const u8 *p, u16 len)
{
    if (len < 1) {
        hciAck(HCI_PERMIT_JOIN, ACK_BAD_PARAMS);
        return;
    }
    hciAck(HCI_PERMIT_JOIN, ACK_OK);
    zb_nlmePermitJoiningRequest(p[0]);
    hciEvent(EVENT_PERMIT, p, 1);  // the request is applied by the stack's task queue afterwards
}

static u8 newChannel;
static s32 channelSwitch(void *arg)
{
    (void)arg;
    channelTimer = NULL;
    if (g_bdbAttrs.nodeIsOnANetwork) {
        zdo_nlmeChannelShift(newChannel);
        sendState();
    }
    return -1;
}

static void changeChannel(const u8 *p, u16 len)
{
    if (len < 2 || p[0] < 11 || p[0] > 26 || !g_bdbAttrs.nodeIsOnANetwork) {
        hciAck(HCI_CHANNEL, ACK_BAD_PARAMS);
        return;
    }
    if (scanChannel) {
        hciAck(HCI_CHANNEL, ACK_BUSY);
        return;
    }
    hciAck(HCI_CHANNEL, ACK_OK);
    newChannel = p[0];
    NWK_NIB().updateId = p[1];
    zdo_mgmt_nwk_update_req_t req;
    TL_SETSTRUCTCONTENT(req, 0);
    req.scan_ch = 1u << newChannel;
    req.scan_duration = 0xFE;
    req.nwk_update_id = p[1];
    u8 sn = 0;
    zb_mgmtNwkUpdateReq(NWK_BROADCAST_RX_ON_WHEN_IDLE, &req, &sn, NULL);
    // Devices switch after the network's broadcast delivery time.
    if (channelTimer) {
        TL_ZB_TIMER_CANCEL(&channelTimer);
    }
    channelTimer = TL_ZB_TIMER_SCHEDULE(channelSwitch, NULL, 10 * 1000);
}

/**********************************************************************
 * Energy scan: the radio leaves the network channel for the scan (the Yandex gateway firmware does
 * the same), frames sent meanwhile go out on the scanned channel and are lost. rf_setChannel also
 * sets the MAC's current channel, which the stack saves to NV: it is put back right away.
 */
static u32 scanMask;
static u32 scanMs;
static u8 scanResult[1 + 2 * 16];

static void scanTune(u8 channel)
{
    rf_setChannel(channel);
    MAC_IB()->phyChannelCur = scanSavedChannel;
}

static s32 scanStep(void *arg)
{
    (void)arg;
    if (scanChannel) {
        u8 *r = scanResult + 1 + 2 * scanResult[0];
        r[0] = scanChannel;
        r[1] = rf_stopEDScan();
        scanResult[0]++;
    }
    u8 next = scanChannel ? scanChannel + 1 : 11;
    while (next <= 26 && !(scanMask & (1u << next))) {
        next++;
    }
    if (next > 26) {
        scanChannel = 0;
        rf_setChannel(scanSavedChannel);
        rf_setTrxState(RF_STATE_RX);
        zbhciTx(HCI_ED_SCAN_RSP, 1 + 2 * scanResult[0], scanResult);
        return -1;
    }
    scanChannel = next;
    scanTune(scanChannel);
    rf_startEDScan();
    return scanMs;
}

static void energyScan(const u8 *p, u16 len)
{
    if (len < 5 || p[4] > 14) {
        hciAck(HCI_ED_SCAN, ACK_BAD_PARAMS);
        return;
    }
    if (scanChannel || channelTimer) {
        hciAck(HCI_ED_SCAN, ACK_BUSY);
        return;
    }
    hciAck(HCI_ED_SCAN, ACK_OK);
    scanMask = BUILD_U32(p[0], p[1], p[2], p[3]) & 0x07FFF800;
    // aBaseSuperframeDuration (15.36 ms) * (2^n + 1)
    scanMs = (1536 * ((1u << p[4]) + 1)) / 100;
    scanResult[0] = 0;
    scanSavedChannel = MAC_IB()->phyChannelCur;
    scanChannel = 0;
    scanStep(NULL);
    if (scanChannel) {
        TL_ZB_TIMER_SCHEDULE(scanStep, NULL, scanMs);
    }
}

static void hciCmd(u16 type, u16 len, u8 *p)
{
    switch (type) {
    case HCI_APP_INFO_REQ:
        hciAppInfo();
        break;
    case HCI_NET_GET:
        hciAck(type, ACK_OK);
        sendState();
        break;
    case HCI_NET_FORM:
        formNetwork(p, len);
        break;
    case HCI_NET_ERASE:
        eraseNetwork();
        break;
    case HCI_PERMIT_JOIN:
        permitJoin(p, len);
        break;
    case HCI_ENDPOINT:
        hciAck(type, setEndpoint(p, len));
        break;
    case HCI_CHANNEL:
        changeChannel(p, len);
        break;
    case HCI_ED_SCAN:
        energyScan(p, len);
        break;
    case HCI_DATA_REQ:
        dataRequest(p, len);
        break;
    default:
        hciAck(type, ACK_UNKNOWN);
        break;
    }
}

/**********************************************************************
 * Stack callbacks.
 */
static void bdbInitCb(u8 status, u8 joinedNetwork)
{
    (void)joinedNetwork;
    // A Zigbee 3.0 trust center may require a joined device to replace the global link key within
    // 15 s and drop it otherwise; Zigbee 1.2 devices (e.g. Xiaomi) never do. deCONZ does not
    // require it either. BDB restores its default on every init (start, erase).
    g_bdbAttrs.tcRequireKeyExchange = 0;
    lastStatus = status;
    sendState();
}

static void bdbCommissioningCb(u8 status, void *arg)
{
    (void)arg;
    if (status == BDB_COMMISSION_STA_IN_PROGRESS) {
        return;
    }
    lastStatus = status;
    if (forming) {
        forming = FALSE;
        if (status == BDB_COMMISSION_STA_FORMATION_DONE) {
            NWK_NIB().updateId = form.updateId;
            if (form.frameCounter > ss_ib.outgoingFrameCounter) {
                ss_ib.outgoingFrameCounter = form.frameCounter;
            }
        }
    }
    sendState();
}

static bdb_appCb_t bdbCb = {bdbInitCb, bdbCommissioningCb, NULL, NULL};

static bool nwkUpdateInd(u16 newPanId)
{
    (void)newPanId;
    return FALSE;  // PAN ID conflicts: the PAN ID stays the one the host chose
}

static bool tcJoinInd(zdo_tc_join_ind_t *p)
{
    u8 b[19];
    put16(b, p->devShortAddr);
    memcpy(b + 2, p->devIeeeAddr, 8);
    memcpy(b + 10, p->parentIeeeAddr, 8);
    b[18] = p->sta;
    hciEvent(EVENT_JOINED, b, sizeof b);
    return TRUE;  // joining is gated by permit join alone
}

static void leaveInd(nlme_leave_ind_t *p)
{
    u8 b[9];
    memcpy(b, p->deviceAddr, 8);
    b[8] = p->rejoin;
    hciEvent(EVENT_LEFT, b, sizeof b);
}

static void permitJoinInd(nlme_permitJoining_req_t *p)
{
    hciEvent(EVENT_PERMIT, &p->permitDuration, 1);
}

static const zdo_appIndCb_t zdoCb = {
    bdb_zdoStartDevCnf, NULL, NULL, leaveInd, NULL, nwkUpdateInd, permitJoinInd, NULL, tcJoinInd, NULL, NULL, NULL,
};

void user_init(bool isRetention)
{
    (void)isRetention;
    zb_init();
    zb_zdoCbRegister((zdo_appIndCb_t *)&zdoCb);

    // The IEEE address: the chip's own (flash), unless the host chose another for its network.
    generateIEEEAddr();
    memcpy(factoryIeee, g_zbMacPib.extAddress, 8);
    addrExt_t ieee;
    if (nv_flashReadNew(1, NV_MODULE_APP, NV_ITEM_IEEE, 8, ieee) == NV_SUCC) {
        applyIeee(ieee);
    }

    af_nodeDescManuCodeUpdate(MANUFACTURER_CODE);
    af_endpoint_descriptor_t *zdo = af_zdoSimpleDescriptorGet();
    zdoRxOrig = zdo->cb_rx;
    zdo->cb_rx = zdoRx;
    zdoCnfOrig = zdo->cb_cnf;
    zdo->cb_cnf = zdoCnf;

    // Endpoint 1 (Home Automation) until the host describes its endpoints; BDB needs one.
    static const u8 ep1[] = {1, 0x04, 0x01, 0x05, 0x00, 0, 0, 0};
    setEndpoint(ep1, sizeof ep1);

    hciStart(hciCmd);
    bdb_init(&endpoints[0].desc, &g_commissionSetting, &bdbCb, 1);
}
