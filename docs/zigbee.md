# Zigbee radio reference (Midi)

Host implementation: `satellite/crates/pulsar-zigbee/`; firmware: `radio/`. Paths are relative
to the repository root unless absolute. Read [station safety rules](../AGENTS.md#device-safety)
and the [test protocol](stations.md) before working on the radio.

Telink B91 (RISC-V) on `/dev/ttyS2`, 115200 8N1, no flow control. It came with Yandex's
`gateway.ybl` (`/system/vendor/quasar/zigbee/`; plain Telink image + zlib CRC-32 trailer, header
version at 0x02, manufacturer 0x140A, image type 0x0580) and the network Yandex formed (channel
12). Reset / boot lines: the kernel's `zb_control` driver exports them as
`/sys/devices/platform/zigbee/zb-{reset,boot}/value` (GPIOZ_11 / GPIOZ_9, active low, 1 =
asserted); `/etc/init.d/S30zigbee` of other firmware builds uses raw GPIO numbers that on this
kernel belong to other pins (475 = GPIOX_0, Wi-Fi SDIO): never drive pins by number. After a reset
the application sends 0x8907 within ~0.3 s.

- Framing (stock Telink zbhci, all three firmwares): `0x55 | type BE16 | len BE16 | xor(type, len,
  payload) | payload | 0xAA`. One frame per DMA buffer: every frame is ACKed with 0x8000
  `[type BE16][status][0]` (2 = unknown type) and the next may only follow the ACK.
- Yandex's gateway firmware cannot read or set the network key, PAN ID or extended PAN ID, passes
  APS frames for endpoint 1 only and consumes ZDO responses. pulsar does not run it;
  `ctl.sh disable` hands the radio back to maind, which writes `gateway.ybl` again (its own update
  path, done when the 0x0900 version differs).

### Roles and radio firmware

State `zigbee_role=off|pulsar-coordinator|router` (HA select "Zigbee role": "Off", "Coordinator",
"Router"; `pulsar_zigbee::ROLES`). Default and anything else is off, including `coordinator`
(the Yandex gateway role of old builds): pulsar does not touch the radio, so an install never
writes it without a choice in HA. Choosing another role persists it and restarts the daemon.
The role service (`service.rs`) opens the link and asks for 0x0900 before building the role object
(the deCONZ server listens only then); a firmware other than the role's image
(`/data/pulsar/zigbee-{coordinator,router}.ybl`, which installs and updates bring), or a radio
silent for 10 s (interrupted upload), gets the role's image, as maind does for the gateway. At most
two uploads per start; an image the radio did not take is stored as
`zigbee_flash_failed=<version hex>` and not written on later starts (the role then keeps an older
build of its firmware if the radio has one) until the role changes or another image version
arrives. While an upload runs (~25 s; the 5 s shutdown watchdog would cut it), the HA Restart
button and update installs are refused; an upload cut short anyway (`ctl.sh restart`, power) leaves
a silent radio, written again on the next start. Updates stage the images next to the binary and
replace them together with it.

- `.ybl`: Telink image whose size field (0x18) is the length without the trailer, then zlib CRC-32
  LE. Header version at 0x02; `version >> 24` tells the family: gateway 0x15, pulsar router 0x50,
  pulsar coordinator 0x51. All three answer 0x0900 → 0x8900 `[version BE32][manufacturer
  BE16][image type BE16][size BE32]`.
- Bootloader ("Yandex Bootloader v1.0.02", resident at 0–0x7FFF; the app is linked at 0x20008000):
  boot=1, reset=1, 100 ms, reset=0, 100 ms, boot=0, then `\n` → menu ending in `BL > `; `1` =
  XMODEM upload (128-byte blocks, CRC-16 on 'C', checksum on NAK, last block padded 0x1A then 0x00,
  EOT), `2` = run the app. This is maind's own update path (`ncpmgr`). The flasher refuses a
  non-tty path.
- pulsar firmware (`radio/`, Telink Zigbee SDK @ pinned commit, Apache-2.0): `common/` has the
  board configuration and flash map shared by `router/` and `coordinator/`, and the 0x0900 /
  0x8907 / ACK pieces (`pulsar_hci.c`). UART0 PB2/PB3, `IMAGE_OFFSET` 0x8000, flash protection off
  (the bootloader must be able to write the next image), all other GPIOs left as inputs. Each
  relocates its NV inside the gateway's OTA staging area (0x77000..0xE5FFF) so every network
  survives every switch: router 0xC0000 (factory-reset counter 0xD6000, install code 0xD7000),
  coordinator 0xA8000 (0xBE000, 0xBF000); the gateway's NV is at 0xE6000, MAC/IEEE at 0xFF000 is
  shared. `sdk.patch` makes those addresses and the UART buffer size overridable, BDB's 180 s
  network-wide permit join after forming / joining optional (`BDB_NO_AUTO_PERMIT_JOIN`, both
  images: joining is the coordinator host's decision), and gives groupcasts for groups without a
  member endpoint to the host (`APS_GROUP_HOST_FALLBACK`, coordinator). `build.sh` fetches the SDK
  again when `sdk.patch` changes. Every frame gets exactly one 0x8000 ACK: `zbhciCmdHandler` ACKs
  the router's stock commands (and `hci_uart.c` parse errors), the apps their own. Neither steers
  or forms on its own, nor has the quick-power-cycle factory reset (pulsar resets the radio at
  every start).
- Router zbhci: 0x0002 = network steering (join), 0x0005 = leave (broadcast Leave) + erase its
  network, 0x0045 → 0x8045 network info (24 bytes, channel last); 0x8906 joined (0x8045 layout
  without the channel); 0x8920 `[event][status]`: 1 started (status 0 = on a network), 2
  commissioning failed (BDB status, 3 = no network), 3 left.

### Coordinator

`radio/coordinator/coordinator.c` (frame layouts in its header) is an NCP: the chip runs the stack
and the trust center, the host chooses the network and gets every APS frame. Commands 0x0Axx /
0x8Axx with little-endian fields. 0x0A00 → 0x8A00 state `[state][BDB status][channel][PAN][ext
PAN][IEEE][NWK][update id][key][key seq][frame counter]` (also on every change; the host asks
every 30 s for the frame counter hosts back up); 0x0A01 forms with the host's channel, PAN ID, ext
PAN, key, IEEE, frame counter and update id (only without a network: 0x0A02 erases first; a state
follows the ACK at once, forming or refused); 0x0A03 local permit join; 0x0A04 adds / replaces /
removes (`[ep][0xFF]`) an endpoint (up to 8, 32 clusters); 0x0A05 channel change (broadcast
Mgmt_NWK_Update_req, local switch after 10 s); 0x0A06 energy scan (`rf_startEDScan` per channel,
the radio leaves the network channel meanwhile, the MAC's current channel is kept); 0x0A10 APS data
request with a 1-byte handle (group / NWK / IEEE destination, source endpoint 0 or a registered
one, APS tx options) → exactly one 0x8A10 `[handle][status]`; 0x8A11 every APS indication of a
registered endpoint, every groupcast and every ZDO frame (the stack's ZDO handler is teed, so it
still answers requests itself); 0x8A20 events (device joined / left, joining permitted) for the
log.

Stack facts (prebuilt `libzb_coordinator.a`, checked with the toolchain's objdump):
- formation takes the NIB's PAN ID (`NWK_NIB().panId`, 0xFFFF = random; it moves past a PAN ID it
  hears on the channel; `TL_ZB_ASSOCJOIN_PERMIT_PANID` is ignored);
- `bdb_networkFormationStart` returns the BDB state (FORMATION when started);
- `zb_preConfigNwkKey` also marks the key pre-configured (`ss_ib.preConfiguredKeyType` NWKKEY),
  after which the trust center never transports it and joining devices associate over and over:
  the key goes in with `ss_zdoNwkKeyConfigure` + `ss_zdoUseKey` instead;
- BDB's default `tcRequireKeyExchange` would drop Zigbee 1.2 devices that never update their link
  key (cleared after every BDB init);
- an erase (`zb_resetDevice2FN`) restarts the APS counter and regenerates the factory IEEE; NV
  restores the frame counter + 1024 at boot; the IEEE chosen by the host lives in NV_MODULE_APP
  item 0x40 and is applied after `zb_init`;
- `af_dataCnfHandler` confirms only to endpoint 0 and registered endpoints; `rf_setChannel` also
  writes `g_mac_pib->phyChannelCur`, which the stack saves to NV.

`Coordinator` (`pulsar-zigbee/src/coordinator.rs`) wraps it; ZDO addressed to the coordinator
itself is answered on the host (`local_zdo`: addresses, node / simple descriptors, active
endpoints, permit join, energy scan). `DeconzServer` (`deconz.rs`) poses as ConBee III firmware
0x26780900, protocol 0x0106 (zigpy-deconz then scans energy with ZDO to the coordinator and skips
update_neighbor, watchdog TTL, source routes). It accepts no TCP client until the radio reported
its network and drops the client when the radio restarts (outside a formation): a host told "no
network" forms one, which erases the radio. A new network is formed (erase, then form) only after
the host wrote parameters the radio does not have (PAN ID, extended PAN ID, key, MAC address) and
switched the network state; the network to form is fixed before the erase, so parameters the host
did not write keep their values. deCONZ endpoint slots live in the server (zigbee-herdsman writes
endpoint 0xF2 into slot 1: the ZLL endpoint 2 is then removed). deCONZ tx option 0x02 (NWK
security) is dropped: APS bit 0x02 would disable NWK security. zigbee-herdsman sends NWK_addr_req
with NWK mode but an IEEE address (payload 6 bytes longer than declared): sent as a broadcast.


## Server limits

- deCONZ server limits (`pulsar-zigbee/src/deconz.rs`): a client with more than 64 KB of unread
  replies is disconnected; an APS request gets FAILURE at ≥ 8 pending confirms or > 16 frames
  queued on the link; at most 4 reads of 1024 bytes per wake-up; accepting pauses 500 ms on
  EMFILE / ENFILE / ENOBUFS / ENOMEM; the formation state becomes FORMING after `form()` returns.
