use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::message::{Sequence, Type};
use zbus::{
    Connection, Message, MessageStream, ObjectPath, OwnedObjectPath, OwnedVariant, Variant,
};

use crate::Playback;

const BLUEZ: &str = "org.bluez";
const BLUEALSA: &str = "org.bluealsa";
const BLUEALSA_ROOT: &str = "/org/bluealsa";
const DBUS: &str = "org.freedesktop.DBus";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const OBJECTS: &str = "org.freedesktop.DBus.ObjectManager";
const ADAPTER: &str = "/org/bluez/hci0";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const PLAYER: &str = "org.bluez.MediaPlayer1";
const TRANSPORT: &str = "org.bluez.MediaTransport1";
const PCM: &str = "org.bluealsa.PCM1";
const A2DP_SINK: &str = "A2DP-sink";
const AUDIO_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";
const AUDIO_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const TIMEOUT: Duration = Duration::from_millis(750);

type Properties = HashMap<String, OwnedVariant>;
type Interfaces = HashMap<String, Properties>;
type Objects = HashMap<OwnedObjectPath, Interfaces>;

struct Registry {
    owner: String,
    baseline: Sequence,
    objects: HashMap<String, Interfaces>,
}

impl Registry {
    async fn get(conn: &Connection, service: &str, path: &str) -> zbus::Result<Self> {
        let msg = conn
            .call_method(Some(service), path, Some(OBJECTS), "GetManagedObjects", &())
            .await?;
        let owner = msg
            .header()
            .sender()
            .ok_or(zbus::Error::InvalidReply)?
            .as_str()
            .to_owned();
        let objects: Objects = msg.body().deserialize()?;
        let objects = objects
            .into_iter()
            .map(|(path, ifaces)| (path.as_str().to_owned(), ifaces))
            .collect();
        Ok(Self {
            owner,
            baseline: msg.recv_position(),
            objects,
        })
    }

    fn apply(&mut self, msg: &Message) -> zbus::Result<()> {
        if msg.recv_position() <= self.baseline {
            return Ok(());
        }
        let header = msg.header();
        if header.sender().is_none_or(|s| s.as_str() != self.owner) {
            return Ok(());
        }
        let iface = header.interface().map(|s| s.as_str());
        let member = header.member().map(|s| s.as_str());
        match (iface, member) {
            (Some(OBJECTS), Some("InterfacesAdded")) => {
                let (path, interfaces): (OwnedObjectPath, Interfaces) = msg.body().deserialize()?;
                self.objects
                    .entry(path.as_str().to_owned())
                    .or_default()
                    .extend(interfaces);
            }
            (Some(OBJECTS), Some("InterfacesRemoved")) => {
                let (path, interfaces): (OwnedObjectPath, Vec<String>) =
                    msg.body().deserialize()?;
                if let Some(obj) = self.objects.get_mut(path.as_str()) {
                    for interface in interfaces {
                        obj.remove(&interface);
                    }
                    if obj.is_empty() {
                        self.objects.remove(path.as_str());
                    }
                }
            }
            (Some(PROPS), Some("PropertiesChanged")) => {
                let Some(path) = header.path() else {
                    return Ok(());
                };
                let (iface, changed, invalidated): (String, Properties, Vec<String>) =
                    msg.body().deserialize()?;
                if let Some(props) = self
                    .objects
                    .get_mut(path.as_str())
                    .and_then(|obj| obj.get_mut(&iface))
                {
                    for name in invalidated {
                        props.remove(&name);
                    }
                    props.extend(changed);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn props(&self, path: &str, iface: &str) -> Option<&Properties> {
        self.objects.get(path)?.get(iface)
    }
}

fn prop<'a, T>(props: &'a Properties, key: &str) -> Option<T>
where
    T: TryFrom<&'a OwnedVariant>,
{
    props.get(key)?.try_into().ok()
}

fn object<'a>(props: &'a Properties, key: &str) -> Option<&'a str> {
    prop::<&ObjectPath<'_>>(props, key).map(|p| p.as_str())
}

fn manager_has_sink(props: &Properties) -> bool {
    let contains = |key, target| {
        prop::<&zbus::Array<'_>>(props, key).is_some_and(|values| {
            values
                .inner()
                .iter()
                .any(|v| matches!(v, Variant::Str(s) if s.as_str() == target))
        })
    };
    contains("Profiles", A2DP_SINK) && contains("Adapters", "hci0")
}

pub(crate) struct Candidate<'a> {
    pub device: &'a str,
    pub sequence: u32,
    pub address: &'a str,
    pub name: &'a str,
    pub player: Option<&'a str>,
    pub phase: Option<Playback>,
    pub phone_gain: [f32; 2],
}

pub(crate) struct Bus {
    stream: MessageStream,
    conn: Connection,
    bluez: Registry,
    alsa: Registry,
}

impl Bus {
    pub async fn connect() -> zbus::Result<Self> {
        let mut conn = Connection::system().await?;
        conn.set_max_queued(256.try_into().expect("positive capacity"));
        let stream = MessageStream::from(&conn);
        for service in [BLUEZ, BLUEALSA] {
            for rule in [
                format!("type='signal',sender='{service}',interface='{OBJECTS}'"),
                format!("type='signal',sender='{service}',interface='{PROPS}'"),
                format!(
                    "type='signal',sender='{DBUS}',interface='{DBUS}',member='NameOwnerChanged',arg0='{service}'"
                ),
            ] {
                conn.call_method(
                    Some(DBUS),
                    "/org/freedesktop/DBus",
                    Some(DBUS),
                    "AddMatch",
                    &(rule,),
                )
                .await?;
            }
        }
        let (bluez, alsa) = tokio::try_join!(
            Registry::get(&conn, BLUEZ, "/"),
            Registry::get(&conn, BLUEALSA, BLUEALSA_ROOT),
        )?;
        let bus = Self {
            conn,
            stream,
            bluez,
            alsa,
        };
        if bus.flags().is_none() || !bus.has_sink().await? {
            return Err(zbus::Error::Failure("no incoming A2DP adapter".into()));
        }
        Ok(bus)
    }

    async fn has_sink(&self) -> zbus::Result<bool> {
        let msg = self
            .conn
            .call_method(
                Some(self.alsa.owner.as_str()),
                BLUEALSA_ROOT,
                Some(PROPS),
                "GetAll",
                &("org.bluealsa.Manager1",),
            )
            .await?;
        let props: Properties = msg.body().deserialize()?;
        Ok(manager_has_sink(&props))
    }

    pub async fn next(&mut self) -> zbus::Result<()> {
        let msg = self
            .stream
            .next()
            .await
            .ok_or(zbus::Error::InvalidReply)??;
        if msg.message_type() != Type::Signal {
            return Ok(());
        }
        let header = msg.header();
        if header.sender().is_some_and(|s| s.as_str() == DBUS)
            && header.interface().is_some_and(|i| i.as_str() == DBUS)
            && header
                .member()
                .is_some_and(|m| m.as_str() == "NameOwnerChanged")
        {
            let body = msg.body();
            let (name, _, new): (&str, &str, &str) = body.deserialize()?;
            let r = match name {
                BLUEZ => Some(&self.bluez),
                BLUEALSA => Some(&self.alsa),
                _ => None,
            };
            if r.is_some_and(|r| msg.recv_position() > r.baseline && new != r.owner) {
                return Err(zbus::Error::Failure(
                    "Bluetooth service owner changed".into(),
                ));
            }
        }
        self.bluez.apply(&msg)?;
        self.alsa.apply(&msg)
    }

    pub fn flags(&self) -> Option<[bool; 2]> {
        let p = self.bluez.props(ADAPTER, ADAPTER_IFACE)?;
        Some([prop(p, "Pairable")?, prop(p, "Discoverable")?])
    }

    pub fn paired(&self) -> BTreeSet<String> {
        self.bluez
            .objects
            .iter()
            .filter_map(|(path, obj)| {
                let p = obj.get(DEVICE)?;
                (object(p, "Adapter") == Some(ADAPTER) && prop(p, "Paired") == Some(true))
                    .then(|| path.as_str().to_owned())
            })
            .collect()
    }

    pub fn candidates(&self) -> impl Iterator<Item = Candidate<'_>> {
        self.bluez.candidates(&self.alsa)
    }

    pub async fn visibility(&self, on: bool) -> zbus::Result<()> {
        let set = |name: &'static str| async move {
            self.call(
                ADAPTER,
                PROPS,
                "Set",
                &(ADAPTER_IFACE, name, Variant::Bool(on)),
            )
            .await
        };
        if on {
            set("Pairable").await?;
            set("Discoverable").await
        } else {
            // Отказ одного флага не должен оставить второй разрешённым.
            let (visible, pairable) = tokio::join!(set("Discoverable"), set("Pairable"));
            visible.and(pairable)
        }
    }

    pub async fn disconnect(&self, path: &str) -> zbus::Result<()> {
        self.call(path, DEVICE, "DisconnectProfile", &(AUDIO_SOURCE,))
            .await
    }

    pub async fn avr(&self, player: &str, method: &str) -> zbus::Result<()> {
        self.call(player, PLAYER, method, &()).await
    }

    async fn call<B: serde::Serialize + zbus::zvariant::DynamicType>(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        body: &B,
    ) -> zbus::Result<()> {
        tokio::time::timeout(
            TIMEOUT,
            self.conn.call_method(
                Some(self.bluez.owner.as_str()),
                path,
                Some(iface),
                method,
                body,
            ),
        )
        .await
        .map_err(|_| zbus::Error::Failure("D-Bus timeout".into()))??;
        Ok(())
    }
}

impl Registry {
    fn candidates<'a>(&'a self, alsa: &'a Self) -> impl Iterator<Item = Candidate<'a>> {
        alsa.objects.values().filter_map(|obj| {
            let pcm = obj.get(PCM)?;
            if prop::<&str>(pcm, "Transport") != Some(A2DP_SINK)
                || prop::<&str>(pcm, "Mode") != Some("source")
            {
                return None;
            }
            let device = object(pcm, "Device")?;
            let p = self.props(device, DEVICE)?;
            if object(p, "Adapter") != Some(ADAPTER) {
                return None;
            }
            let address = prop::<&str>(p, "Address")
                .filter(|v| valid_address(v))
                .unwrap_or_default();
            let name = ["Alias", "Name"]
                .into_iter()
                .filter_map(|k| prop::<&str>(p, k))
                .find(|v| {
                    !v.is_empty() && !valid_address(v) && !address_alias(v, address) && *v != device
                })
                .unwrap_or("device");
            let player = self.player(device);
            let phase = player
                .and_then(|path| self.props(path, PLAYER))
                .and_then(|p| match prop::<&str>(p, "Status") {
                    Some("playing") => Some(Playback::Playing),
                    Some("paused" | "stopped") => Some(Playback::Paused),
                    _ => None,
                });
            Some(Candidate {
                device,
                sequence: prop(pcm, "Sequence").unwrap_or(u32::MAX),
                address,
                name,
                player,
                phase,
                phone_gain: phone_gain(pcm, || self.transport_volume(device)),
            })
        })
    }

    fn player(&self, device: &str) -> Option<&str> {
        if let Some(path) = self
            .props(device, "org.bluez.MediaControl1")
            .and_then(|p| object(p, "Player"))
        {
            return self
                .props(path, PLAYER)
                .filter(|_| child_of(path, device))
                .map(|_| path);
        }
        self.objects
            .iter()
            .filter(|(p, interfaces)| child_of(p, device) && interfaces.contains_key(PLAYER))
            .map(|(p, _)| p.as_str())
            .min()
    }

    fn transport_volume(&self, device: &str) -> Option<u16> {
        self.objects
            .iter()
            .filter_map(|(path, obj)| {
                let p = obj.get(TRANSPORT)?;
                if object(p, "Device") != Some(device)
                    || prop::<&str>(p, "UUID") != Some(AUDIO_SINK)
                {
                    return None;
                }
                let inactive = !matches!(prop::<&str>(p, "State"), Some("active" | "pending"));
                Some(((inactive, path.as_str()), prop::<u16>(p, "Volume")?))
            })
            .min_by_key(|(order, _)| *order)
            .map(|(_, v)| v)
    }
}

fn child_of(path: &str, device: &str) -> bool {
    path.strip_prefix(device)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn valid_address(s: &str) -> bool {
    s.len() == 17
        && s.bytes().enumerate().all(|(n, c)| {
            if n % 3 == 2 {
                c == b':'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn address_alias(name: &str, address: &str) -> bool {
    !address.is_empty()
        && name.len() == address.len()
        && name
            .bytes()
            .zip(address.bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b) || ((a == b'-' || a == b'_') && b == b':'))
}

fn phone_gain(pcm: &Properties, transport: impl FnOnce() -> Option<u16>) -> [f32; 2] {
    if prop(pcm, "SoftVolume") != Some(false) {
        return [1.0; 2];
    }
    let transport = transport();
    let scale = |level: u16| f32::from(level.min(127)) / 127.0;
    let Some(volume) = prop::<u16>(pcm, "Volume") else {
        return [transport.map_or(1.0, scale); 2];
    };
    let [left, mut right] = volume.to_be_bytes();
    if prop(pcm, "Channels") == Some(1_u8) {
        right = left;
    }
    [left, right].map(|channel| {
        if channel & 0x80 != 0 {
            0.0
        } else {
            scale(transport.unwrap_or(u16::from(channel)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_recognizes_incoming_a2dp_without_connected_pcms() {
        let strings = |values: Vec<&str>| {
            Variant::Array(zbus::Array::from(values))
                .try_to_owned()
                .unwrap()
        };
        let mut manager = Properties::from([
            ("Adapters".into(), strings(vec!["hci0"])),
            ("Profiles".into(), strings(vec!["A2DP-source", "A2DP-sink"])),
        ]);
        assert!(manager_has_sink(&manager));

        manager.insert("Profiles".into(), strings(vec!["A2DP-source"]));
        assert!(!manager_has_sink(&manager));
        manager.insert("Profiles".into(), strings(vec!["A2DP-sink"]));
        manager.insert("Adapters".into(), strings(vec!["hci1"]));
        assert!(!manager_has_sink(&manager));
    }

    #[test]
    fn native_volume_keeps_channel_mute_and_does_not_repeat_soft_volume() {
        let mut pcm = Properties::from([
            ("Volume".into(), 0x4080_u16.into()),
            ("SoftVolume".into(), false.into()),
        ]);
        assert_eq!(phone_gain(&pcm, || None), [64.0 / 127.0, 0.0]);
        assert_eq!(phone_gain(&pcm, || Some(32)), [32.0 / 127.0, 0.0]);
        pcm.insert("SoftVolume".into(), true.into());
        assert_eq!(
            phone_gain(&pcm, || panic!("soft volume already applied")),
            [1.0; 2]
        );
    }

    #[test]
    fn incoming_only_and_selected_player_not_player_zero() {
        let registry = |objects| Registry {
            owner: ":1.1".into(),
            baseline: Message::signal("/", OBJECTS, "InterfacesAdded")
                .build(&())
                .unwrap()
                .recv_position(),
            objects,
        };
        let path = |s| OwnedVariant::from(ObjectPath::try_from(s).unwrap());
        let text = |s: &'static str| OwnedVariant::from(zbus::Str::from(s));
        let device = || {
            HashMap::from([
                ("Adapter".into(), path(ADAPTER)),
                ("Address".into(), text("02:00:00:00:00:01")),
                ("Alias".into(), text("02-00-00-00-00-01")),
            ])
        };
        let first = "/org/bluez/hci0/dev_first";
        let second = "/org/bluez/hci0/dev_second";
        let player7 = "/org/bluez/hci0/dev_first/player7";
        let bluez = registry(HashMap::from([
            (
                first.into(),
                HashMap::from([
                    (DEVICE.into(), device()),
                    (
                        "org.bluez.MediaControl1".into(),
                        HashMap::from([("Player".into(), path(player7))]),
                    ),
                ]),
            ),
            (second.into(), HashMap::from([(DEVICE.into(), device())])),
            (
                format!("{first}/player0"),
                HashMap::from([(
                    PLAYER.into(),
                    HashMap::from([("Status".into(), text("paused"))]),
                )]),
            ),
            (
                player7.into(),
                HashMap::from([(
                    PLAYER.into(),
                    HashMap::from([("Status".into(), text("playing"))]),
                )]),
            ),
        ]));
        let pcm = |dev, transport, mode, seq: u32| {
            HashMap::from([(
                PCM.into(),
                HashMap::from([
                    ("Device".into(), path(dev)),
                    ("Transport".into(), text(transport)),
                    ("Mode".into(), text(mode)),
                    ("Sequence".into(), seq.into()),
                ]),
            )])
        };
        let alsa = registry(HashMap::from([
            ("/first".into(), pcm(first, "A2DP-sink", "source", 3)),
            ("/second".into(), pcm(second, "A2DP-sink", "source", 4)),
            ("/outgoing".into(), pcm(second, "A2DP-source", "sink", 1)),
        ]));
        let mut incoming: Vec<_> = bluez.candidates(&alsa).collect();
        incoming.sort_by_key(|c| c.sequence);
        assert_eq!(incoming.len(), 2);
        assert_eq!(
            (
                incoming[0].device,
                incoming[0].name,
                incoming[0].player,
                incoming[0].phase
            ),
            (first, "device", Some(player7), Some(Playback::Playing))
        );
        assert_eq!(
            (incoming[1].device, incoming[1].player, incoming[1].phase),
            (second, None, None)
        );
    }
}
