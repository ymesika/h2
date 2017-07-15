use {FrameSize, Peer};
use error::ConnectionError;
use error::Reason::*;
use error::User::*;
use proto::FlowController;

/// Represents the state of an H2 stream
///
/// ```not_rust
///                              +--------+
///                      send PP |        | recv PP
///                     ,--------|  idle  |--------.
///                    /         |        |         \
///                   v          +--------+          v
///            +----------+          |           +----------+
///            |          |          | send H /  |          |
///     ,------| reserved |          | recv H    | reserved |------.
///     |      | (local)  |          |           | (remote) |      |
///     |      +----------+          v           +----------+      |
///     |          |             +--------+             |          |
///     |          |     recv ES |        | send ES     |          |
///     |   send H |     ,-------|  open  |-------.     | recv H   |
///     |          |    /        |        |        \    |          |
///     |          v   v         +--------+         v   v          |
///     |      +----------+          |           +----------+      |
///     |      |   half   |          |           |   half   |      |
///     |      |  closed  |          | send R /  |  closed  |      |
///     |      | (remote) |          | recv R    | (local)  |      |
///     |      +----------+          |           +----------+      |
///     |           |                |                 |           |
///     |           | send ES /      |       recv ES / |           |
///     |           | send R /       v        send R / |           |
///     |           | recv R     +--------+   recv R   |           |
///     | send R /  `----------->|        |<-----------'  send R / |
///     | recv R                 | closed |               recv R   |
///     `----------------------->|        |<----------------------'
///                              +--------+
///
///        send:   endpoint sends this frame
///        recv:   endpoint receives this frame
///
///        H:  HEADERS frame (with implied CONTINUATIONs)
///        PP: PUSH_PROMISE frame (with implied CONTINUATIONs)
///        ES: END_STREAM flag
///        R:  RST_STREAM frame
/// ```
#[derive(Debug, Copy, Clone)]
pub enum StreamState {
    Idle,
    // TODO: these states shouldn't count against concurrency limits:
    //ReservedLocal,
    //ReservedRemote,
    Open {
        local: PeerState,
        remote: PeerState,
    },
    HalfClosedLocal(PeerState),
    HalfClosedRemote(PeerState),
    Closed,
}

impl StreamState {
    /// Updates the local flow controller so that the remote may send `incr` more bytes.
    ///
    /// Returns the amount of capacity created, accounting for window size changes. The
    /// caller should send the the returned window size increment to the remote.
    ///
    /// If the remote is closed, None is returned.
    pub fn grow_send_window(&mut self, incr: u32) {
        use self::StreamState::*;
        use self::PeerState::*;

        if incr == 0 {
            return;
        }

        match self {
            &mut Open { remote: Data(ref mut fc), .. } |
            &mut HalfClosedLocal(Data(ref mut fc)) => fc.grow_window(incr),
            _ => {},
        }
    }
 
    pub fn shrink_send_window(&mut self, decr: u32) {
        use self::StreamState::*;
        use self::PeerState::*;

        if decr == 0 {
            return;
        }

        match self {
            &mut Open { local: Data(ref mut fc), .. } |
            &mut HalfClosedLocal(Data(ref mut fc)) => fc.shrink_window(decr),
            _ => {},
        }
    }


    /// Consumes newly-advertised capacity to inform the local endpoint it may send more
    /// data.
    pub fn take_send_window_update(&mut self) -> Option<u32> {
        use self::StreamState::*;
        use self::PeerState::*;

        match self {
            &mut Open { remote: Data(ref mut fc), .. } |
            &mut HalfClosedLocal(Data(ref mut fc)) => fc.take_window_update(),
            _ => None,
        }
    }

    /// Updates the remote flow controller so that the remote may receive `incr`
    /// additional bytes.
    ///
    /// Returns the amount of capacity created, accounting for window size changes. The
    /// caller should send the the returned window size increment to the remote.
    pub fn grow_recv_window(&mut self, incr: u32) {
        use self::StreamState::*;
        use self::PeerState::*;

        if incr == 0 {
            return;
        }

        match self {
            &mut Open { local: Data(ref mut fc), .. } |
            &mut HalfClosedRemote(Data(ref mut fc)) => fc.grow_window(incr),
            _ => {},
        }
    }

    pub fn shrink_recv_window(&mut self, decr: u32) {
        use self::StreamState::*;
        use self::PeerState::*;

        if decr == 0 {
            return;
        }

        match self {
            &mut Open { local: Data(ref mut fc), .. } |
            &mut HalfClosedRemote(Data(ref mut fc)) => fc.shrink_window(decr),
            _ => {},
        }
    }

    /// Consumes newly-advertised capacity to inform the local endpoint it may send more
    /// data.
    pub fn take_recv_window_update(&mut self) -> Option<u32> {
        use self::StreamState::*;
        use self::PeerState::*;

        match self {
            &mut Open { local: Data(ref mut fc), .. } |
            &mut HalfClosedRemote(Data(ref mut fc)) => fc.take_window_update(),
            _ => None,
        }
    }

    /// Transition the state to represent headers being received.
    ///
    /// Returns true if this state transition results in iniitializing the
    /// stream id. `Err` is returned if this is an invalid state transition.
    pub fn recv_headers<P: Peer>(&mut self,
                                 eos: bool,
                                 initial_recv_window_size: u32)
        -> Result<bool, ConnectionError>
    {
        use self::StreamState::*;
        use self::PeerState::*;

        match *self {
            Idle => {
                let local = Headers;
                if eos {
                    *self = HalfClosedRemote(local);
                } else {
                    *self = Open { local, remote: Data(FlowController::new(initial_recv_window_size)) };
                }
                Ok(true)
            }

            Open { local, remote } => {
                try!(remote.check_is_headers(ProtocolError.into()));
                if !eos {
                    // Received non-trailers HEADERS on open remote.
                    return Err(ProtocolError.into());
                }
                *self = HalfClosedRemote(local);
                Ok(false)
            }

            HalfClosedLocal(headers) => {
                try!(headers.check_is_headers(ProtocolError.into()));
                if eos {
                    *self = Closed;
                } else {
                    *self = HalfClosedLocal(Data(FlowController::new(initial_recv_window_size)));
                };
                Ok(false)
            }

            Closed | HalfClosedRemote(..) => {
                Err(ProtocolError.into())
            }
        }
    }

    pub fn recv_data(&mut self, eos: bool, len: FrameSize) -> Result<(), ConnectionError> {
        use self::StreamState::*;

        match *self {
            Open { local, mut remote } => {
                try!(remote.check_is_data(ProtocolError.into()));
                try!(remote.claim_window_size(len, FlowControlError.into()));
                if eos {
                    *self = HalfClosedRemote(local);
                }
                Ok(())
            }

            HalfClosedLocal(mut remote) => {
                try!(remote.check_is_data(ProtocolError.into()));
                try!(remote.claim_window_size(len, FlowControlError.into()));
                if eos {
                    *self = Closed;
                }
                Ok(())
            }

            Closed | HalfClosedRemote(..) => {
                Err(ProtocolError.into())
            }

            _ => unimplemented!(),
        }
    }

    /// Transition the state to represent headers being sent.
    ///
    /// Returns true if this state transition results in initializing the stream
    /// id. `Err` is returned if this is an invalid state transition.
    pub fn send_headers<P: Peer>(&mut self, 
                                 eos: bool,
                                 initial_window_size: u32)
        -> Result<bool, ConnectionError>
    {
        use self::StreamState::*;
        use self::PeerState::*;

        match *self {
            Idle => {
                *self = if eos {
                    HalfClosedLocal(Headers)
                } else {
                    Open {
                        local: Data(FlowController::new(initial_window_size)),
                        remote: Headers,
                    }
                };

                Ok(true)
            }

            Open { local, remote } => {
                try!(local.check_is_headers(UnexpectedFrameType.into()));

                *self = if eos {
                    HalfClosedLocal(remote)
                } else {
                    let local = Data(FlowController::new(initial_window_size));
                    Open { local, remote }
                };

                Ok(false)
            }

            HalfClosedRemote(local) => {
                try!(local.check_is_headers(UnexpectedFrameType.into()));

                *self = if eos {
                    Closed
                } else {
                    HalfClosedRemote(Data(FlowController::new(initial_window_size)))
                };

                Ok(false)
            }

            Closed | HalfClosedLocal(..) => {
                Err(UnexpectedFrameType.into())
            }
        }
    }

    pub fn send_data(&mut self, eos: bool, len: FrameSize) -> Result<(), ConnectionError> {
        use self::StreamState::*;

        match *self {
            Open { mut local, remote } => {
                try!(local.check_is_data(UnexpectedFrameType.into()));
                try!(local.claim_window_size(len, FlowControlViolation.into()));
                if eos {
                    *self = HalfClosedLocal(remote);
                }
                Ok(())
            }

            HalfClosedRemote(mut local) => {
                try!(local.check_is_data(UnexpectedFrameType.into()));
                try!(local.claim_window_size(len, FlowControlViolation.into()));
                if eos {
                    *self = Closed;
                }
                Ok(())
            }

            Closed | HalfClosedLocal(..) => {
                Err(UnexpectedFrameType.into())
            }

            _ => unimplemented!(),
        }
    }
}

impl Default for StreamState {
    fn default() -> StreamState {
        StreamState::Idle
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PeerState {
    Headers,
    /// Contains a FlowController representing the _receiver_ of this this data stream.
    Data(FlowController),
}

impl PeerState {
    #[inline]
    fn check_is_headers(&self, err: ConnectionError) -> Result<(), ConnectionError> {
        use self::PeerState::*;
        match self {
            &Headers => Ok(()),
            _ => Err(err),
        }
    }

    #[inline]
    fn check_is_data(&self, err: ConnectionError) -> Result<(), ConnectionError> {
        use self::PeerState::*;
        match self {
            &Data(_) => Ok(()),
            _ => Err(err),
        }
    }

    #[inline]
    fn claim_window_size(&mut self, sz: FrameSize, err: ConnectionError) -> Result<(), ConnectionError> {
        use self::PeerState::*;
        match self {
            &mut Data(ref mut fc) => fc.claim_window(sz).map_err(|_| err),
            _ => Err(err),
        }
    }
}