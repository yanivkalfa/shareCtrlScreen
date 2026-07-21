//! WebSocket signaling client to the Cloudflare Worker (Plan 04 §4/§9, contract
//! §3). Ported from `app/renderer/signaling.js`: register-first, the byte-exact
//! ping/pong watchdog (25 s ping, 10 s pong timeout, 2 misses ⇒ dead), and
//! exponential-backoff reconnect (1,2,4,…,30 s) with re-register on every
//! reconnect.
//!
//! Async (tokio + tokio-tungstenite). The engine drives it through two channels:
//! it sends outbound [`SignalMsg`]s and receives inbound [`Event`]s. Routing of
//! inbound messages to host/viewer roles lives in the engine, mirroring how the
//! JS `route()` dispatched to `App.Host`/`App.Viewer`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::signaling::{PING_LITERAL, PONG_LITERAL};
use protocol::SignalMsg;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const PING_INTERVAL: Duration = Duration::from_secs(25);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff schedule in seconds (contract §3.3). Duplicate registration is a
/// standing condition — retried at the 30 s cap.
const BACKOFF_SECS: &[u64] = &[1, 2, 4, 8, 16, 30];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("websocket: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("channel closed")]
    ChannelClosed,
}

/// Inbound events surfaced to the engine (mirrors signaling.js `route`).
#[derive(Debug, Clone)]
pub enum Event {
    /// Registered successfully — the socket is live.
    Registered,
    /// Registration rejected (e.g. `"duplicate"`).
    RegisterError(String),
    /// A relayed protocol message (connect-request/response, signal, …).
    Message(SignalMsg),
    /// The socket dropped; the client is retrying with backoff. If a session is
    /// active the WebRTC leg continues independently (§8 in signaling.js).
    Disconnected,
}

/// Install the process-level rustls `CryptoProvider` exactly once.
///
/// rustls 0.23 refuses to pick a provider implicitly unless exactly one backend
/// feature is on; `tokio-tungstenite` pulls rustls without selecting one, so
/// without this the first `wss://` connect panics on the tokio worker.
fn ensure_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err just means another component already installed one — fine.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Handle the engine keeps to talk to the signaling task.
pub struct SignalingClient {
    outbound: mpsc::UnboundedSender<SignalMsg>,
}

impl SignalingClient {
    /// Spawn the reconnecting signaling loop. Returns the handle plus a receiver
    /// of inbound [`Event`]s. `uuid` is registered on every (re)connect.
    pub fn spawn(server_url: String, uuid: String) -> (Self, mpsc::UnboundedReceiver<Event>) {
        ensure_crypto();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMsg>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Event>();
        tokio::spawn(reconnect_loop(server_url, uuid, out_rx, ev_tx));
        (Self { outbound: out_tx }, ev_rx)
    }

    /// Queue an outbound message (relayed to a peer if it carries `to`).
    // Error wraps tungstenite's large error type; the send path is not hot.
    #[allow(clippy::result_large_err)]
    pub fn send(&self, msg: SignalMsg) -> Result<(), Error> {
        self.outbound.send(msg).map_err(|_| Error::ChannelClosed)
    }
}

async fn reconnect_loop(
    server_url: String,
    uuid: String,
    mut out_rx: mpsc::UnboundedReceiver<SignalMsg>,
    ev_tx: mpsc::UnboundedSender<Event>,
) {
    let mut attempt = 0usize;
    let mut duplicate = false;
    loop {
        match connect_once(&server_url, &uuid, &mut out_rx, &ev_tx, &mut duplicate).await {
            Ok(()) => { /* graceful close from our side */ }
            Err(e) => tracing::warn!("signaling connection ended: {e}"),
        }
        let _ = ev_tx.send(Event::Disconnected);
        if ev_tx.is_closed() {
            return; // engine gone
        }
        // Duplicate is a standing condition — slow retry (30 s), else backoff.
        let delay = if duplicate {
            30
        } else {
            BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)]
        };
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

/// One connection lifetime: connect, register, then drive the ping watchdog and
/// pump messages until the socket closes or is deemed dead.
async fn connect_once(
    server_url: &str,
    uuid: &str,
    out_rx: &mut mpsc::UnboundedReceiver<SignalMsg>,
    ev_tx: &mpsc::UnboundedSender<Event>,
    duplicate: &mut bool,
) -> Result<(), Error> {
    let (ws, _resp) = tokio_tungstenite::connect_async(server_url).await?;
    let (mut sink, mut stream) = ws.split();

    // Contract §3.1: register is the first message after connecting.
    let reg = serde_json::to_string(&SignalMsg::Register {
        uuid: uuid.to_string(),
        v: protocol::PROTOCOL_VERSION,
    })
    .expect("register serializes");
    sink.send(Message::Text(reg)).await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick.
    ping.tick().await;

    let mut missed_pongs = 0u32;
    let mut pong_deadline: Option<tokio::time::Instant> = None;

    loop {
        // Compute the pong-timeout sleep target if a ping is outstanding.
        let pong_timeout = async {
            match pong_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;

            // Outbound from the engine.
            Some(msg) = out_rx.recv() => {
                let txt = serde_json::to_string(&msg).unwrap_or_default();
                sink.send(Message::Text(txt)).await?;
            }

            // Ping cadence (§3.3 — send the byte-exact literal, not JSON.stringify).
            _ = ping.tick() => {
                sink.send(Message::Text(PING_LITERAL.into())).await?;
                if pong_deadline.is_none() {
                    pong_deadline = Some(tokio::time::Instant::now() + PONG_TIMEOUT);
                }
            }

            // Pong watchdog: 2 consecutive misses ⇒ assume dead, drop the socket.
            _ = pong_timeout => {
                missed_pongs += 1;
                pong_deadline = None;
                if missed_pongs >= 2 {
                    tracing::warn!("2 missed pongs — closing socket");
                    return Ok(());
                }
            }

            // Inbound.
            item = stream.next() => {
                let Some(item) = item else { return Ok(()); }; // stream ended
                let msg = item?;
                match msg {
                    Message::Text(t) => {
                        // Cheapest match first: raw pong literal (§3.3).
                        if t == PONG_LITERAL {
                            missed_pongs = 0;
                            pong_deadline = None;
                            continue;
                        }
                        handle_text(&t, ev_tx, duplicate);
                    }
                    Message::Ping(p) => sink.send(Message::Pong(p)).await?,
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

fn handle_text(text: &str, ev_tx: &mpsc::UnboundedSender<Event>, duplicate: &mut bool) {
    let Ok(msg) = serde_json::from_str::<SignalMsg>(text) else {
        return; // ignore non-JSON / unknown shapes (contract §3 forward-compat)
    };
    match msg {
        SignalMsg::Pong => {}
        SignalMsg::Registered { .. } => {
            *duplicate = false;
            let _ = ev_tx.send(Event::Registered);
        }
        SignalMsg::RegisterError { reason } => {
            if reason == "duplicate" {
                *duplicate = true;
            }
            let _ = ev_tx.send(Event::RegisterError(reason));
        }
        other => {
            let _ = ev_tx.send(Event::Message(other));
        }
    }
}
