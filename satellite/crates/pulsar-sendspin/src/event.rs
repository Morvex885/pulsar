//! sendspin-cpp's main loop events as the service receives them from the shim, and the sink
//! request each one makes inside its callback.

#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamParams {
    /// sendspin::SendspinCodecFormat value.
    pub codec: u8,
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
}

/// Library events of its main loop thread.
#[derive(Debug)]
pub(crate) enum LibEvent {
    /// `playing`: None when the update carries no playback state.
    Group {
        playing: Option<bool>,
        name: Option<String>,
    },
    TimeSync(f32),
    PairingStarted(String),
    PairingSucceeded(String),
    /// `reason`: sendspin::SendspinPairAbortReason value.
    PairingFailed {
        server_id: String,
        reason: u8,
    },
    Trust {
        paired: bool,
    },
    PairingCode(String),
    PairingCodeCleared,
    PairingWindow(bool),
    /// None: the stream parameters are incomplete.
    StreamStart(Option<StreamParams>),
    StreamEnd,
    Volume(u8),
    Mute(bool),
    OutputDelay(u16),
    ControllerState {
        commands: usize,
        volume: u8,
        muted: bool,
    },
    Metadata {
        artist: Option<String>,
        title: Option<String>,
    },
    VisualizerStart {
        types: usize,
        rate_max: u16,
    },
    VisualizerEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SinkRequest {
    Configure { rate: u32, channels: u8, bits: u8 },
    Release,
}

/// The sink request a callback makes before it returns: right after on_stream_start
/// sendspin-cpp signals its sync task, which at once writes the priming silence
/// (player_role.cpp signal_stream_start, sync_task.cpp). Audio written before the configure
/// would be discarded unreported while the library counts it as buffered, shifting playback
/// timing. As in the C++ callbacks; the actor only logs and holds.
pub(crate) fn sink_request(ev: &LibEvent) -> Option<SinkRequest> {
    match ev {
        LibEvent::StreamStart(Some(p)) => Some(SinkRequest::Configure {
            rate: p.rate,
            channels: p.channels,
            bits: p.bits,
        }),
        // The audio stops at once; only the reported state is held.
        LibEvent::StreamEnd => Some(SinkRequest::Release),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_callbacks_request_the_sink() {
        let p = StreamParams {
            codec: 0,
            rate: 48000,
            channels: 2,
            bits: 16,
        };
        assert_eq!(
            sink_request(&LibEvent::StreamStart(Some(p))),
            Some(SinkRequest::Configure {
                rate: 48000,
                channels: 2,
                bits: 16
            })
        );
        assert_eq!(
            sink_request(&LibEvent::StreamEnd),
            Some(SinkRequest::Release)
        );
        // Incomplete parameters leave the sink alone, as in C++.
        assert_eq!(sink_request(&LibEvent::StreamStart(None)), None);
        assert_eq!(sink_request(&LibEvent::Volume(20)), None);
        assert_eq!(
            sink_request(&LibEvent::Group {
                playing: Some(false),
                name: None
            }),
            None
        );
    }
}
