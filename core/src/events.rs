//! Event bus: one place where background threads (remote WebSocket, status polling,
//! cast sessions, network checks) report state changes to the UIs.
//!
//! Subscribe with [`EventBus::subscribe`] (or [`crate::Tv::subscribe`]); every subscriber
//! gets its own `std::sync::mpsc::Receiver<Event>` with a copy of every event emitted
//! after subscribing. Dropped receivers are pruned automatically.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::cast::CastEvent;
use crate::rest::PowerState;

/// State of the remote-control WebSocket channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected and not trying (initial state, or after `disconnect`).
    Disconnected,
    /// A connection attempt is in progress.
    Connecting,
    /// Connected and `ms.channel.connect` received; keys can be sent.
    Connected,
    /// Connection lost or failed; the thread retries after a back-off.
    /// The string is a human-readable reason.
    Retrying(String),
    /// The TV refused the pairing (user denied, or prompt timed out).
    /// The thread keeps retrying with a long back-off.
    Unauthorized,
}

/// Something that happened in the background. Cheap to clone.
#[derive(Debug, Clone)]
pub enum Event {
    /// The remote channel connection state changed.
    Connection(ConnectionState),
    /// The TV power state changed (from status polling or after power on/off).
    Power(PowerState),
    /// The TV issued a new token on connect; it has been saved to the config file.
    /// (The token itself is deliberately not carried in the event.)
    TokenUpdated,
    /// An unsolicited event from the remote channel that the library does not
    /// interpret itself (e.g. `ms.remote.imeStart` when a text field gets focus).
    RemoteEvent {
        /// Event name, e.g. `ms.remote.imeStart`.
        name: String,
        /// Raw `data` field.
        data: serde_json::Value,
    },
    /// A cast session reported something (state, throughput, live-guard warnings).
    Cast(CastEvent),
    /// The configuration was changed and saved (host, SmartThings token, ...).
    ConfigChanged,
    /// SmartThings reachability of this TV changed (from [`crate::Fleet`] polling or
    /// [`crate::Tv::health`]).
    Cloud(CloudState),
    /// The list of TVs changed: added, removed, renamed, linked, active TV switched, or
    /// a scan updated hosts. Emitted on the fleet bus with an empty `tv` id.
    FleetChanged,
}

/// SmartThings view of one TV.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CloudState {
    /// No SmartThings device linked (or no token).
    NotLinked,
    /// SmartThings reports the device online.
    Online,
    /// SmartThings reports the device offline.
    Offline,
    /// The cloud could not be asked (network or token error); the reason.
    Unknown(String),
}

impl CloudState {
    /// True when SmartThings reports the device online.
    pub fn is_online(&self) -> bool {
        matches!(self, CloudState::Online)
    }
}

/// An [`Event`] tagged with the local id of the TV it belongs to (see
/// [`crate::Fleet::subscribe`]). `tv` is empty for fleet-wide events.
#[derive(Debug, Clone)]
pub struct TvEvent {
    /// Local TV id (`tv1`, …), or empty.
    pub tv: String,
    /// The event.
    pub event: Event,
}

/// Broadcast channel for any cloneable message (used for the fleet bus).
pub struct Broadcast<T> {
    subs: Arc<Mutex<Vec<Sender<T>>>>,
}

impl<T> Clone for Broadcast<T> {
    fn clone(&self) -> Self {
        Broadcast { subs: self.subs.clone() }
    }
}

impl<T> Default for Broadcast<T> {
    fn default() -> Self {
        Broadcast { subs: Arc::new(Mutex::new(Vec::new())) }
    }
}

impl<T: Clone> Broadcast<T> {
    /// Register a subscriber.
    pub fn subscribe(&self) -> Receiver<T> {
        let (tx, rx) = channel();
        if let Ok(mut subs) = self.subs.lock() {
            subs.push(tx);
        }
        rx
    }

    /// Send to all live subscribers (never blocks).
    pub fn emit(&self, v: T) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|s| s.send(v.clone()).is_ok());
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }
}

/// Broadcast channel for [`Event`]s of one TV. Clone it freely; all clones share
/// subscribers. A bus created by [`EventBus::tagged`] also forwards every event, tagged
/// with its TV id, to a fleet-wide [`Broadcast<TvEvent>`].
#[derive(Clone, Default)]
pub struct EventBus {
    subs: Broadcast<Event>,
    fleet: Option<(String, Broadcast<TvEvent>)>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus").finish_non_exhaustive()
    }
}

impl EventBus {
    /// New bus without subscribers.
    pub fn new() -> Self {
        Self::default()
    }

    /// A bus for TV `tv` that also forwards its events to `fleet`.
    pub fn tagged(tv: &str, fleet: Broadcast<TvEvent>) -> Self {
        EventBus { subs: Broadcast::default(), fleet: Some((tv.to_string(), fleet)) }
    }

    /// The TV id this bus tags its events with (`None` for a standalone bus).
    pub fn tv_id(&self) -> Option<&str> {
        self.fleet.as_ref().map(|(id, _)| id.as_str())
    }

    /// Register a new subscriber. The receiver gets every event emitted from now on.
    pub fn subscribe(&self) -> Receiver<Event> {
        self.subs.subscribe()
    }

    /// Send an event to all live subscribers (never blocks).
    pub fn emit(&self, ev: Event) {
        if let Some((tv, fleet)) = &self.fleet {
            fleet.emit(TvEvent { tv: tv.clone(), event: ev.clone() });
        }
        self.subs.emit(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_and_prune() {
        let bus = EventBus::new();
        let a = bus.subscribe();
        let b = bus.subscribe();
        bus.emit(Event::TokenUpdated);
        assert!(matches!(a.try_recv(), Ok(Event::TokenUpdated)));
        assert!(matches!(b.try_recv(), Ok(Event::TokenUpdated)));
        drop(b);
        bus.emit(Event::ConfigChanged);
        assert!(matches!(a.try_recv(), Ok(Event::ConfigChanged)));
        assert_eq!(bus.subs.len(), 1);
    }

    #[test]
    fn tagged_bus_forwards_to_the_fleet() {
        let fleet = Broadcast::<TvEvent>::default();
        let all = fleet.subscribe();
        let a = EventBus::tagged("tv1", fleet.clone());
        let b = EventBus::tagged("tv2", fleet.clone());
        let only_a = a.subscribe();
        a.emit(Event::TokenUpdated);
        b.emit(Event::Cloud(CloudState::Online));
        assert!(matches!(only_a.try_recv(), Ok(Event::TokenUpdated)));
        assert!(only_a.try_recv().is_err(), "per-TV subscribers only see their TV");
        let e1 = all.try_recv().unwrap();
        let e2 = all.try_recv().unwrap();
        assert_eq!((e1.tv.as_str(), e2.tv.as_str()), ("tv1", "tv2"));
        assert!(matches!(e2.event, Event::Cloud(CloudState::Online)));
        assert_eq!(a.tv_id(), Some("tv1"));
    }
}
