//! The radio's roles: `state.conf` `zigbee_role=` value, Home Assistant select option and
//! firmware family (top byte of the image version, `radio/*/version_cfg.h`).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// pulsar leaves the radio alone (whatever firmware it has keeps running); the default, so
    /// an install never writes the radio on its own.
    Off,
    /// pulsar's coordinator firmware, a deCONZ coordinator for ZHA and Zigbee2MQTT.
    Coordinator,
    /// pulsar's router firmware, a repeater in another network.
    Router,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Role {
    pub mode: Mode,
    pub name: &'static str,
    pub label: &'static str,
    pub family: u8,
}

pub const ROLES: [Role; 3] = [
    Role {
        mode: Mode::Off,
        name: "off",
        label: "Off",
        family: 0,
    },
    Role {
        mode: Mode::Coordinator,
        name: "pulsar-coordinator",
        label: "Coordinator",
        family: 0x51,
    },
    Role {
        mode: Mode::Router,
        name: "router",
        label: "Router",
        family: 0x50,
    },
];

// A static copy: references into a const would point at temporaries.
static TABLE: [Role; 3] = ROLES;

pub fn role(m: Mode) -> &'static Role {
    TABLE.iter().find(|r| r.mode == m).unwrap_or(&TABLE[0])
}

/// `state.conf` value → mode. Anything else, including "coordinator" (Yandex's gateway firmware,
/// which pulsar no longer drives), is `Off`: the radio is never written without a choice in HA.
pub fn parse_mode(s: &str) -> Mode {
    TABLE
        .iter()
        .find(|r| r.name == s)
        .map_or(Mode::Off, |r| r.mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_modes_and_families() {
        assert_eq!(parse_mode("off"), Mode::Off);
        assert_eq!(parse_mode("pulsar-coordinator"), Mode::Coordinator);
        assert_eq!(parse_mode("router"), Mode::Router);
        for other in ["coordinator", "", "Router", "router ", "on"] {
            assert_eq!(parse_mode(other), Mode::Off, "{other:?}");
        }
        assert_eq!(role(Mode::Off).label, "Off");
        assert_eq!(role(Mode::Coordinator).family, 0x51);
        assert_eq!(role(Mode::Coordinator).label, "Coordinator");
        assert_eq!(role(Mode::Router).family, 0x50);
        assert_eq!(role(Mode::Router).name, "router");
    }
}
