use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

use futures_util::StreamExt;
use pulsar_core::actors;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zbus::message::{Sequence, Type};
use zbus::{
    Connection, Message, MessageStream, ObjectPath, OwnedObjectPath, OwnedVariant, Variant,
};

use crate::Playback;

const BLUEZ: &str = "org.bluez";
const BLUEALSA: &str = "org.bluealsa";
const BLUEALSA_ROOT: &str = "/org/bluealsa";
const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const OBJECTS: &str = "org.freedesktop.DBus.ObjectManager";
const ADAPTER: &str = "/org/bluez/hci0";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const MEDIA_CONTROL: &str = "org.bluez.MediaControl1";
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
    baseline: Sequence,
    snapshot_order: u64,
    playing_edges: HashMap<String, u64>,
    objects: HashMap<String, Interfaces>,
}

impl Registry {
    async fn get(conn: &Connection, owner: &str, path: &str) -> zbus::Result<Self> {
        let msg = conn
            .call_method(Some(owner), path, Some(OBJECTS), "GetManagedObjects", &())
            .await?;
        let snapshot_order = pulsar_core::audio_start_order();
        if msg
            .header()
            .sender()
            .is_none_or(|name| name.as_str() != owner)
        {
            return Err(zbus::Error::InvalidReply);
        }
        let objects: Objects = msg.body().deserialize()?;
        let objects = objects
            .into_iter()
            .map(|(path, ifaces)| (path.as_str().to_owned(), ifaces))
            .collect();
        Ok(Self {
            baseline: msg.recv_position(),
            snapshot_order,
            playing_edges: HashMap::new(),
            objects,
        })
    }

    fn apply(&mut self, msg: &Message, order: u64) -> zbus::Result<Option<String>> {
        let header = msg.header();
        let iface = header.interface().map(|s| s.as_str());
        let member = header.member().map(|s| s.as_str());
        match (iface, member) {
            (Some(OBJECTS), Some("InterfacesAdded")) => {
                let (path, interfaces): (OwnedObjectPath, Interfaces) = msg.body().deserialize()?;
                let adds_player = interfaces.contains_key(PLAYER);
                let was_playing = adds_player && self.playing(path.as_str());
                let selection = self.selection_before(
                    path.as_str(),
                    interfaces.contains_key(MEDIA_CONTROL),
                    adds_player,
                );
                let pairs_device = interfaces.contains_key(DEVICE) && !self.paired(path.as_str());
                self.objects
                    .entry(path.as_str().to_owned())
                    .or_default()
                    .extend(interfaces);
                if adds_player {
                    self.update_playing_edge(path.as_str(), was_playing, order);
                }
                self.update_selected_edge(selection, order);
                if pairs_device && self.paired(path.as_str()) {
                    return Ok(Some(path.as_str().to_owned()));
                }
            }
            (Some(OBJECTS), Some("InterfacesRemoved")) => {
                let (path, interfaces): (OwnedObjectPath, Vec<String>) =
                    msg.body().deserialize()?;
                let selection = self.selection_before(
                    path.as_str(),
                    interfaces.iter().any(|iface| iface == MEDIA_CONTROL),
                    interfaces.iter().any(|iface| iface == PLAYER),
                );
                if let Some(obj) = self.objects.get_mut(path.as_str()) {
                    for interface in interfaces {
                        obj.remove(&interface);
                        if interface == PLAYER {
                            self.playing_edges.remove(path.as_str());
                        }
                    }
                    if obj.is_empty() {
                        self.objects.remove(path.as_str());
                    }
                }
                self.update_selected_edge(selection, order);
            }
            (Some(PROPS), Some("PropertiesChanged")) => {
                let Some(path) = header.path() else {
                    return Ok(None);
                };
                let (iface, changed, invalidated): (String, Properties, Vec<String>) =
                    msg.body().deserialize()?;
                let was_playing = iface == PLAYER && self.playing(path.as_str());
                let selection = self.selection_before(
                    path.as_str(),
                    iface == MEDIA_CONTROL
                        && (changed.contains_key("Player")
                            || invalidated.iter().any(|n| n == "Player")),
                    false,
                );
                let pairs_device = iface == DEVICE
                    && prop(&changed, "Paired") == Some(true)
                    && !self.paired(path.as_str());
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
                if iface == PLAYER {
                    self.update_playing_edge(path.as_str(), was_playing, order);
                }
                self.update_selected_edge(selection, order);
                if pairs_device && self.paired(path.as_str()) {
                    return Ok(Some(path.as_str().to_owned()));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn paired(&self, path: &str) -> bool {
        self.props(path, DEVICE).is_some_and(|p| {
            object(p, "Adapter") == Some(ADAPTER) && prop(p, "Paired") == Some(true)
        })
    }

    fn selection_before<'a>(
        &self,
        path: &'a str,
        media_control: bool,
        player: bool,
    ) -> Option<(&'a str, Option<String>)> {
        let device = if media_control {
            path
        } else if player {
            path.rmatch_indices('/')
                .map(|(i, _)| &path[..i])
                .find(|path| self.props(path, DEVICE).is_some())?
        } else {
            return None;
        };
        Some((device, self.player(device).map(str::to_owned)))
    }

    fn update_selected_edge(&mut self, before: Option<(&str, Option<String>)>, order: u64) {
        if let Some((device, old)) = before
            && let Some(player) = self.player(device)
            && Some(player) != old.as_deref()
            && self.playing(player)
        {
            self.playing_edges.insert(player.to_owned(), order);
        }
    }

    fn playing(&self, path: &str) -> bool {
        self.props(path, PLAYER)
            .and_then(|p| prop::<&str>(p, "Status"))
            == Some("playing")
    }

    fn update_playing_edge(&mut self, path: &str, was_playing: bool, order: u64) {
        match (was_playing, self.playing(path)) {
            (false, true) => {
                self.playing_edges.insert(path.to_owned(), order);
            }
            (true, false) => {
                self.playing_edges.remove(path);
            }
            _ => {}
        }
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
    pub phase: Option<(Playback, Option<u64>)>,
    pub phone_gain: [f32; 2],
}

struct Owner {
    unique: String,
    since: Sequence,
}

impl Owner {
    async fn get(conn: &Connection, service: &str) -> zbus::Result<Self> {
        let msg = conn
            .call_method(
                Some(DBUS),
                DBUS_PATH,
                Some(DBUS),
                "GetNameOwner",
                &(service,),
            )
            .await?;
        Ok(Self {
            unique: msg.body().deserialize()?,
            since: msg.recv_position(),
        })
    }
}

struct Owners {
    bluez: Owner,
    alsa: Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignalKind {
    Bluez,
    Alsa,
    OwnerChanged,
}

impl Owners {
    fn classify(&self, msg: &Message) -> Option<SignalKind> {
        if msg.message_type() != Type::Signal {
            return None;
        }
        let header = msg.header();
        let sender = header.sender()?.as_str();
        match (header.interface()?.as_str(), header.member()?.as_str()) {
            (DBUS, "NameOwnerChanged")
                if sender == DBUS && header.path()?.as_str() == DBUS_PATH =>
            {
                let body = msg.body();
                let (name, _, new): (&str, &str, &str) = body.deserialize().ok()?;
                let owner = match name {
                    BLUEZ => &self.bluez,
                    BLUEALSA => &self.alsa,
                    _ => return None,
                };
                (msg.recv_position() > owner.since && new != owner.unique)
                    .then_some(SignalKind::OwnerChanged)
            }
            (interface @ OBJECTS, "InterfacesAdded" | "InterfacesRemoved")
            | (interface @ PROPS, "PropertiesChanged") => {
                let kind = if sender == self.bluez.unique {
                    SignalKind::Bluez
                } else if sender == self.alsa.unique {
                    SignalKind::Alsa
                } else {
                    return None;
                };
                if interface == PROPS {
                    let body = msg.body();
                    let changed: &str = body.deserialize_ignore_signature().ok()?;
                    let relevant = match kind {
                        SignalKind::Bluez => {
                            matches!(
                                changed,
                                ADAPTER_IFACE | DEVICE | MEDIA_CONTROL | PLAYER | TRANSPORT
                            )
                        }
                        SignalKind::Alsa => changed == PCM,
                        SignalKind::OwnerChanged => unreachable!(),
                    };
                    if !relevant {
                        return None;
                    }
                }
                Some(kind)
            }
            _ => None,
        }
    }
}

struct Signal {
    message: Message,
    kind: SignalKind,
    received: Instant,
    order: u64,
}

async fn collect_signals(
    mut stream: impl futures_util::Stream<Item = zbus::Result<Message>> + Unpin,
    owners: Rc<Owners>,
    tx: mpsc::Sender<Signal>,
    mut startup: Option<watch::Sender<Option<(Sequence, bool)>>>,
) {
    // Отмечаем сигналы независимо от ожидания исходящих вызовов актором.
    let mut owner_changed = false;
    loop {
        let Some(Ok(message)) = (tokio::select! {
            biased;
            () = tx.closed() => return,
            msg = stream.next() => msg,
        }) else {
            return;
        };
        let seq = message.recv_position();
        if let Some(kind) = owners.classify(&message) {
            owner_changed |= kind == SignalKind::OwnerChanged;
            let signal = Signal {
                message,
                kind,
                received: Instant::now(),
                order: pulsar_core::audio_start_order(),
            };
            if tx.try_send(signal).is_err() {
                // При переполнении переснять состояние вместо отложенной маркировки.
                return;
            }
        }
        if let Some(progress) = &startup {
            if progress.is_closed() {
                startup = None;
            } else {
                progress.send_replace(Some((seq, owner_changed)));
            }
        }
    }
}

pub(crate) struct Bus {
    signals: mpsc::Receiver<Signal>,
    owners: Rc<Owners>,
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
                conn.call_method(Some(DBUS), DBUS_PATH, Some(DBUS), "AddMatch", &(rule,))
                    .await?;
            }
        }
        let (bluez_owner, alsa_owner) =
            tokio::try_join!(Owner::get(&conn, BLUEZ), Owner::get(&conn, BLUEALSA))?;
        let owners = Rc::new(Owners {
            bluez: bluez_owner,
            alsa: alsa_owner,
        });
        let (tx, signals) = mpsc::channel(256);
        let (progress, mut startup) = watch::channel(None);
        actors::spawn_actor(
            "bluetooth D-Bus signals",
            collect_signals(stream, owners.clone(), tx, Some(progress)),
        );
        let (bluez, alsa) = tokio::try_join!(
            Registry::get(&conn, &owners.bluez.unique, "/"),
            Registry::get(&conn, &owners.alsa.unique, BLUEALSA_ROOT),
        )?;
        let bus = Self {
            conn,
            signals,
            owners,
            bluez,
            alsa,
        };
        if bus.flags().is_none() || !bus.has_sink().await? {
            return Err(zbus::Error::Failure("no incoming A2DP adapter".into()));
        }
        let (bluez, alsa) = tokio::try_join!(
            Owner::get(&bus.conn, BLUEZ),
            Owner::get(&bus.conn, BLUEALSA)
        )?;
        let barrier = bluez.since.max(alsa.since);
        // Проверяем также краткую смену владельца с возвратом между снимками и проверкой.
        let settled = startup
            .wait_for(|progress| progress.is_some_and(|(seq, _)| seq >= barrier))
            .await
            .map_err(|_| zbus::Error::InvalidReply)?;
        if bluez.unique != bus.owners.bluez.unique
            || alsa.unique != bus.owners.alsa.unique
            || settled.is_some_and(|(_, changed)| changed)
            || bus.signals.is_closed()
        {
            return Err(owner_changed());
        }
        Ok(bus)
    }

    async fn has_sink(&self) -> zbus::Result<bool> {
        let msg = self
            .conn
            .call_method(
                Some(self.owners.alsa.unique.as_str()),
                BLUEALSA_ROOT,
                Some(PROPS),
                "GetAll",
                &("org.bluealsa.Manager1",),
            )
            .await?;
        let props: Properties = msg.body().deserialize()?;
        Ok(manager_has_sink(&props))
    }

    pub async fn next(&mut self) -> zbus::Result<Option<(String, Instant)>> {
        let signal = self.signals.recv().await.ok_or(zbus::Error::InvalidReply)?;
        if self.signals.is_closed() {
            return Err(zbus::Error::InvalidReply);
        }
        let seq = signal.message.recv_position();
        match signal.kind {
            SignalKind::Bluez if seq > self.bluez.baseline => Ok(self
                .bluez
                .apply(&signal.message, signal.order)?
                .map(|path| (path, signal.received))),
            SignalKind::Alsa if seq > self.alsa.baseline => {
                self.alsa.apply(&signal.message, signal.order)?;
                Ok(None)
            }
            SignalKind::OwnerChanged => Err(owner_changed()),
            _ => Ok(None),
        }
    }

    pub fn snapshot_order(&self) -> u64 {
        self.bluez.snapshot_order
    }

    pub fn flags(&self) -> Option<[bool; 2]> {
        let p = self.bluez.props(ADAPTER, ADAPTER_IFACE)?;
        Some([prop(p, "Pairable")?, prop(p, "Discoverable")?])
    }

    pub fn paired(&self) -> BTreeSet<String> {
        self.bluez
            .objects
            .keys()
            .filter(|path| self.bluez.paired(path))
            .cloned()
            .collect()
    }

    pub fn is_paired(&self, path: &str) -> bool {
        self.bluez.paired(path)
    }

    pub fn candidates(&self) -> impl Iterator<Item = Candidate<'_>> {
        self.bluez.candidates(&self.alsa)
    }

    pub async fn visibility(&self, on: bool) -> zbus::Result<()> {
        visibility(&self.conn, &self.owners.bluez.unique, on).await
    }

    pub async fn close_pairing(&self) -> zbus::Result<()> {
        self.visibility(false).await?;
        check_bluez_owner(&self.conn, &self.owners.bluez.unique).await
    }

    pub async fn disconnect(&self, path: &str) -> zbus::Result<()> {
        call(
            &self.conn,
            &self.owners.bluez.unique,
            path,
            DEVICE,
            "DisconnectProfile",
            &(AUDIO_SOURCE,),
        )
        .await
    }

    pub async fn avr(&self, player: &str, method: &str) -> zbus::Result<()> {
        call(
            &self.conn,
            &self.owners.bluez.unique,
            player,
            PLAYER,
            method,
            &(),
        )
        .await
    }
}

pub(crate) async fn close_pairing() -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let owner = Owner::get(&conn, BLUEZ).await?;
    visibility(&conn, &owner.unique, false).await?;
    check_bluez_owner(&conn, &owner.unique).await
}

async fn check_bluez_owner(conn: &Connection, pinned: &str) -> zbus::Result<()> {
    if Owner::get(conn, BLUEZ).await?.unique != pinned {
        return Err(owner_changed());
    }
    Ok(())
}

fn owner_changed() -> zbus::Error {
    zbus::Error::Failure("Bluetooth service owner changed".into())
}

async fn visibility(conn: &Connection, owner: &str, on: bool) -> zbus::Result<()> {
    let set = |name: &'static str| async move {
        call(
            conn,
            owner,
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

async fn call<B: serde::Serialize + zbus::zvariant::DynamicType>(
    conn: &Connection,
    owner: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<()> {
    tokio::time::timeout(
        TIMEOUT,
        conn.call_method(Some(owner), path, Some(iface), method, body),
    )
    .await
    .map_err(|_| zbus::Error::Failure("D-Bus timeout".into()))??;
    Ok(())
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
            let phase = player.and_then(|path| {
                let phase = match prop::<&str>(self.props(path, PLAYER)?, "Status") {
                    Some("playing") => Playback::Playing,
                    Some("paused" | "stopped") => Playback::Paused,
                    _ => return None,
                };
                let order = self.playing_edges.get(path).copied();
                Some((phase, order))
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
            .props(device, MEDIA_CONTROL)
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

    fn owners(baseline: Sequence) -> Rc<Owners> {
        Rc::new(Owners {
            bluez: Owner {
                unique: ":1.1".into(),
                since: baseline,
            },
            alsa: Owner {
                unique: ":1.2".into(),
                since: baseline,
            },
        })
    }

    #[tokio::test]
    async fn inbound_signal_is_stamped_before_delayed_service_consumption() {
        let props_changed = |sender: &'static str, interface: &'static str| {
            Message::signal("/player", PROPS, "PropertiesChanged")
                .sender(sender)
                .build(&(
                    interface,
                    Properties::from([("Status".into(), zbus::Str::from("playing").into())]),
                    Vec::<String>::new(),
                ))
                .unwrap()
        };
        let signal = props_changed(":1.1", PLAYER);
        let owners = owners(signal.recv_position());
        let unrelated = props_changed(":1.3", PLAYER);
        let unrelated_interface = props_changed(":1.1", "org.bluez.Battery1");
        let forged_owner_change = Message::signal(DBUS_PATH, DBUS, "NameOwnerChanged")
            .sender(":1.3")
            .build(&(BLUEZ, ":1.1", ""))
            .unwrap();
        let unrelated_member = Message::signal("/player", PROPS, "Other")
            .sender(":1.1")
            .build(&())
            .unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let before_receive = Instant::now();
        // Чужие сообщения не занимают очередь даже при исчерпании её ёмкости.
        collect_signals(
            futures_util::stream::iter([
                Ok(unrelated),
                Ok(unrelated_interface),
                Ok(unrelated_member),
                Ok(forged_owner_change),
                Ok(signal),
            ]),
            owners,
            tx,
            None,
        )
        .await;
        let music_order = pulsar_core::audio_start_order();
        let signal = rx.recv().await.unwrap();
        assert_eq!(signal.kind, SignalKind::Bluez);
        assert!(signal.order < music_order);
        assert!(signal.received >= before_receive);
    }

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
            baseline: Message::signal("/", OBJECTS, "InterfacesAdded")
                .build(&())
                .unwrap()
                .recv_position(),
            snapshot_order: 1,
            playing_edges: HashMap::new(),
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
        let mut bluez = registry(HashMap::from([
            (
                first.into(),
                HashMap::from([
                    (DEVICE.into(), device()),
                    (
                        MEDIA_CONTROL.into(),
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
            (
                first,
                "device",
                Some(player7),
                Some((Playback::Playing, None))
            )
        );
        assert_eq!(
            (incoming[1].device, incoming[1].player, incoming[1].phase),
            (second, None, None)
        );

        drop(incoming);
        let second_player = "/org/bluez/hci0/dev_first/player0";
        bluez
            .objects
            .get_mut(second_player)
            .unwrap()
            .get_mut(PLAYER)
            .unwrap()
            .insert("Status".into(), text("playing"));
        let switch = Message::signal(first, PROPS, "PropertiesChanged")
            .build(&(
                MEDIA_CONTROL,
                Properties::from([("Player".into(), path(second_player))]),
                Vec::<String>::new(),
            ))
            .unwrap();
        bluez.apply(&switch, 10).unwrap();
        bluez.apply(&switch, 11).unwrap();
        let selected = bluez.candidates(&alsa).find(|c| c.device == first).unwrap();
        assert_eq!(selected.player, Some(second_player));
        assert_eq!(selected.phase, Some((Playback::Playing, Some(10))));

        let changed = |device, props| {
            Message::signal(device, PROPS, "PropertiesChanged")
                .build(&(DEVICE, props, Vec::<String>::new()))
                .unwrap()
        };
        let alias = changed(first, Properties::from([("Alias".into(), text("Phone"))]));
        assert_eq!(bluez.apply(&alias, 12).unwrap(), None);
        let pair = changed(first, Properties::from([("Paired".into(), true.into())]));
        assert_eq!(bluez.apply(&pair, 13).unwrap().as_deref(), Some(first));
        assert_eq!(bluez.apply(&pair, 14).unwrap(), None);
        bluez
            .objects
            .get_mut(second)
            .unwrap()
            .get_mut(DEVICE)
            .unwrap()
            .insert("Adapter".into(), path("/org/bluez/hci1"));
        let wrong_adapter = changed(second, Properties::from([("Paired".into(), true.into())]));
        assert_eq!(bluez.apply(&wrong_adapter, 15).unwrap(), None);
    }
}
