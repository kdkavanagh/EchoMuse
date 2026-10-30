//! A live session's tasks. The control task reads the control socket and owns the
//! session's liveness, heartbeats, clock replies and acknowledgement waiters; it never
//! waits for the runtime (spec §5.2). Writer tasks own each socket's sending half, and
//! the audio reader hands packets to the ordered ingress channel.

use std::sync::Arc;

use echomuse_core::types::DeviceId;
use echomuse_protocol::ema::Packet;
use echomuse_protocol::envelope::{ControlFrame, DeviceMessage, Inbound};
use echomuse_protocol::session::{ClockReply, ClockRequest, Heartbeat};
use echomuse_protocol::{
    ControlBody, MessageType, ProtocolError, ProtocolErrorCode, check_live_session, parse_envelope,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;

use crate::auth::Transport;
use crate::commands::{AckTable, Registration, mark_written};
use crate::config::{LinkLimits, utc_ms};
use crate::ingress::{
    Ingress, IngressProducer, PrivacyChange, TelemetryKind, TelemetryProducer, UrgentEvent,
    ingress_channel, telemetry_channel, urgent_channel,
};
use crate::server::Shared;
use crate::session::{
    CloseReason, ControlEgress, ControlOut, LinkSession, LinkStatus, QueueKind, RenderEgress,
    RenderOut, SendError, SessionCore, SessionLink, SocketKind,
};

/// An upgraded device WebSocket.
pub(crate) type Ws = WebSocketStream<TokioIo<Upgraded>>;
pub(crate) type WsSink = SplitSink<Ws, Message>;
pub(crate) type WsStream = SplitStream<Ws>;

/// Ends the session if the task holding it unwinds, so a panic is never lost in an
/// unobserved join handle (spec §5.5).
pub(crate) struct PanicGuard<'a> {
    pub(crate) core: &'a SessionCore,
    pub(crate) task: &'static str,
}

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.core.end(CloseReason::TaskPanicked(self.task));
        }
    }
}

/// Everything a session's tasks own, before they start.
pub(crate) struct SessionRunner {
    core: Arc<SessionCore>,
    control: ControlEgress,
    control_rx: mpsc::Receiver<ControlOut>,
    registrations: mpsc::Receiver<Registration>,
    render_rx: mpsc::Receiver<RenderOut>,
    ingress: IngressProducer,
    urgent: mpsc::Sender<UrgentEvent>,
    telemetry: TelemetryProducer,
    status: watch::Sender<LinkStatus>,
}

/// Builds a session's channels: the half the runtime gets and the half its tasks run.
/// The session's token is a child of the server's, so shutdown stops it.
pub(crate) fn new_session(
    device: DeviceId,
    transport: Transport,
    limits: &LinkLimits,
    server_stop: &CancellationToken,
) -> (LinkSession, SessionRunner) {
    let core = SessionCore::new(device, transport, server_stop.child_token());
    let (control, control_rx) =
        ControlEgress::new(limits.control_egress.messages, limits.control_egress.bytes);
    let (registrations_tx, registrations) = mpsc::channel(limits.control_egress.messages);
    let (render, render_rx) =
        RenderEgress::new(limits.render_egress.messages, limits.render_egress.bytes);
    let (ingress, ingress_rx) = ingress_channel(
        &core,
        limits.ingress.messages,
        limits.ingress.bytes,
        limits.ingress.max_age,
    );
    let (urgent, urgent_rx) = urgent_channel(&core, limits.urgent_events);
    let (telemetry, telemetry_rx) = telemetry_channel(&core, limits.telemetry_events);
    let (status, status_rx) = watch::channel(LinkStatus::default());
    let link = SessionLink::new(
        Arc::clone(&core),
        control.clone(),
        registrations_tx,
        render,
        status_rx,
    );
    let session = LinkSession {
        link,
        ingress: ingress_rx,
        urgent: urgent_rx,
        telemetry: telemetry_rx,
    };
    let runner = SessionRunner {
        core,
        control,
        control_rx,
        registrations,
        render_rx,
        ingress,
        urgent,
        telemetry,
        status,
    };
    (session, runner)
}

impl SessionRunner {
    pub(crate) fn core(&self) -> &Arc<SessionCore> {
        &self.core
    }

    /// Runs the session until it ends: starts the control writer, then reads control
    /// in this task. `session.ready` must already be written.
    pub(crate) async fn run(
        self,
        shared: &Shared,
        sink: WsSink,
        stream: WsStream,
        audio_join: oneshot::Receiver<Ws>,
    ) {
        let SessionRunner {
            core,
            control,
            control_rx,
            registrations,
            render_rx,
            ingress,
            urgent,
            telemetry,
            status,
        } = self;
        let timings = shared.config.timings;
        shared.tracker.spawn(control_writer(
            Arc::clone(&core),
            sink,
            control_rx,
            timings.write,
        ));
        let now = Instant::now();
        let mut task = ControlTask {
            shared,
            core: &core,
            control,
            registrations,
            table: AckTable::new(),
            ingress,
            urgent,
            telemetry,
            status,
            render_rx: Some(render_rx),
            last_rx: now,
            degraded: false,
        };
        let _guard = PanicGuard {
            core: &core,
            task: "control",
        };
        task.run(stream, audio_join, now).await;
    }
}

struct ControlTask<'a> {
    shared: &'a Shared,
    core: &'a Arc<SessionCore>,
    control: ControlEgress,
    registrations: mpsc::Receiver<Registration>,
    table: AckTable,
    ingress: IngressProducer,
    urgent: mpsc::Sender<UrgentEvent>,
    telemetry: TelemetryProducer,
    status: watch::Sender<LinkStatus>,
    render_rx: Option<mpsc::Receiver<RenderOut>>,
    last_rx: Instant,
    degraded: bool,
}

impl ControlTask<'_> {
    async fn run(
        &mut self,
        mut stream: WsStream,
        mut audio_join: oneshot::Receiver<Ws>,
        start: Instant,
    ) {
        let timings = self.shared.config.timings;
        let core = Arc::clone(self.core);
        let audio_deadline = start + timings.audio_join;
        let mut audio_pending = true;
        let mut join_open = true;
        let mut heartbeat = interval_at(start + timings.heartbeat, timings.heartbeat);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let silence_limit = if self.degraded {
                timings.lost
            } else {
                timings.degraded
            };
            let liveness = self.last_rx + silence_limit;
            let step = tokio::select! {
                biased;
                _ = core.stop.cancelled() => break,
                registration = self.registrations.recv() => match registration {
                    Some(registration) => {
                        self.table.insert(registration);
                        Ok(())
                    }
                    // Every SessionLink clone is gone: nobody can drive the session.
                    None => Err(CloseReason::ClosedByController),
                },
                () = self.table.expire_next(), if !self.table.is_empty() => Ok(()),
                joined = &mut audio_join, if join_open => {
                    join_open = false;
                    if let Ok(ws) = joined {
                        audio_pending = false;
                        self.start_audio(ws);
                    }
                    Ok(())
                }
                () = sleep_until(audio_deadline), if audio_pending => Err(CloseReason::AudioNotJoined),
                message = stream.next() => self.on_message(message),
                _ = heartbeat.tick() => self.send_heartbeat(),
                () = sleep_until(liveness) => self.on_silence(),
            };
            if let Err(reason) = step {
                core.end(reason);
                break;
            }
        }
        let reason = core.ended_reason();
        // Stopped by the server's token: record the reason and revoke capture too.
        core.end(reason.clone());
        self.table.fail_all(&reason, &mut self.registrations);
    }

    fn start_audio(&mut self, ws: Ws) {
        let Some(render_rx) = self.render_rx.take() else {
            return;
        };
        let (sink, stream) = ws.split();
        let timings = self.shared.config.timings;
        self.shared.tracker.spawn(audio_reader(
            Arc::clone(self.core),
            stream,
            self.ingress.clone(),
        ));
        self.shared.tracker.spawn(audio_writer(
            Arc::clone(self.core),
            sink,
            render_rx,
            timings.write,
        ));
        self.status.send_modify(|status| status.audio_joined = true);
    }

    fn on_silence(&mut self) -> Result<(), CloseReason> {
        if self.degraded {
            return Err(CloseReason::HeartbeatLost);
        }
        self.degraded = true;
        self.status.send_modify(|status| status.degraded = true);
        Ok(())
    }

    fn on_message(&mut self, message: Option<Result<Message, WsError>>) -> Result<(), CloseReason> {
        let closed = |detail: String| CloseReason::SocketClosed {
            socket: SocketKind::Control,
            detail,
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                self.saw_control();
                self.on_text(text.as_str())
            }
            Some(Ok(Message::Binary(bytes))) => {
                self.saw_control();
                let error = match parse_envelope(ControlFrame::Binary(&bytes)) {
                    Err(error) => error,
                    Ok(_) => unreachable!("binary control frames never parse"),
                };
                Err(CloseReason::ProtocolViolation(error))
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => Ok(()),
            Some(Ok(Message::Close(_))) | None => Err(closed("closed by the device".to_owned())),
            Some(Err(error)) => Err(closed(error.to_string())),
        }
    }

    /// Any control message is a sign the link is alive.
    fn saw_control(&mut self) {
        self.last_rx = Instant::now();
        if self.degraded {
            self.degraded = false;
            self.status.send_modify(|status| status.degraded = false);
        }
    }

    fn on_text(&mut self, text: &str) -> Result<(), CloseReason> {
        let inbound =
            parse_envelope(ControlFrame::Text(text)).map_err(CloseReason::ProtocolViolation)?;
        check_live_session(&inbound, &self.core.session_id, &self.core.device)
            .map_err(CloseReason::ProtocolViolation)?;
        // An unknown type counts as liveness and is otherwise ignored.
        let Inbound::Known(envelope) = inbound else {
            return Ok(());
        };
        let message_type = envelope.message.message_type();
        match envelope.message {
            DeviceMessage::Heartbeat(heartbeat) => {
                let sample = crate::session::HeartbeatSample {
                    device_mono_ns: heartbeat.mono_ns,
                    received_at: Instant::now(),
                };
                self.status
                    .send_modify(|status| status.last_heartbeat = Some(sample));
                Ok(())
            }
            DeviceMessage::ClockRequest(request) => self.reply_clock(request),
            DeviceMessage::CommandAck(ack) => {
                self.table.resolve(ack, &mut self.registrations);
                Ok(())
            }
            DeviceMessage::ProtocolError(report) => Err(CloseReason::DeviceProtocolError(report)),
            DeviceMessage::SessionHello(_) => {
                Err(CloseReason::ProtocolViolation(ProtocolError::new(
                    ProtocolErrorCode::UnexpectedMessage,
                    "session.hello on a live session",
                )))
            }
            DeviceMessage::Other {
                message_type: MessageType::PrivacyChanged,
                ref body,
            } => {
                let change = PrivacyChange::parse(body).map_err(CloseReason::ProtocolViolation)?;
                self.on_privacy(envelope.header, change)
            }
            _ => match TelemetryKind::of(message_type) {
                Some(kind) => {
                    self.telemetry.push(kind, envelope);
                    Ok(())
                }
                None => self.ingress.push(Ingress::Control(envelope), text.len()),
            },
        }
    }

    fn on_privacy(
        &mut self,
        header: echomuse_protocol::EnvelopeHeader,
        change: PrivacyChange,
    ) -> Result<(), CloseReason> {
        // Revoke first, so capture work stops before the runtime even hears of it.
        if change.muted {
            self.core.revoke_capture();
        }
        match self
            .urgent
            .try_send(UrgentEvent::Privacy { header, change })
        {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(CloseReason::Overloaded {
                queue: QueueKind::Urgent,
                detail: "every urgent slot is taken".to_owned(),
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(CloseReason::ClosedByController),
        }
    }

    fn reply_clock(&mut self, request: ClockRequest) -> Result<(), CloseReason> {
        let Some(utc_ms) = utc_ms(self.shared.config.wall_clock.as_ref()) else {
            tracing::warn!(device = %self.core.device, "wall clock reads before 1970; clock.request left unanswered");
            return Ok(());
        };
        self.send(&ClockReply::to(request, utc_ms))
    }

    fn send_heartbeat(&mut self) -> Result<(), CloseReason> {
        // The controller's own monotonic clock, counted from the server's start. The
        // firmware does not read it; only its arrival matters.
        let mono_ns =
            u64::try_from(self.shared.mono_epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.send(&Heartbeat { mono_ns })
    }

    fn send<B: ControlBody>(&mut self, body: &B) -> Result<(), CloseReason> {
        let encoded = match self.core.encode(body, 0) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::error!(%error, "a transport message did not encode");
                return Ok(());
            }
        };
        match self.control.push(self.core, encoded.text, None) {
            Ok(()) => Ok(()),
            Err(SendError::SessionEnded(reason)) => Err(reason),
            Err(SendError::Overloaded | SendError::Encode(_)) => Err(self.core.ended_reason()),
        }
    }
}

/// Writes queued control frames. When the session ends it discards the queue, writes
/// the controller's `protocol.error` if a violation ended it, and closes the socket.
async fn control_writer(
    core: Arc<SessionCore>,
    mut sink: WsSink,
    mut rx: mpsc::Receiver<ControlOut>,
    write: std::time::Duration,
) {
    let _guard = PanicGuard {
        core: &core,
        task: "control writer",
    };
    loop {
        let out = tokio::select! {
            biased;
            _ = core.stop.cancelled() => break,
            out = rx.recv() => out,
        };
        let Some(out) = out else { break };
        match timeout(write, sink.send(Message::text(out.text))).await {
            Ok(Ok(())) => {
                if let Some(progress) = &out.progress {
                    mark_written(progress);
                }
            }
            Ok(Err(error)) => {
                core.end(CloseReason::SocketClosed {
                    socket: SocketKind::Control,
                    detail: error.to_string(),
                });
                break;
            }
            Err(_) => {
                core.end(CloseReason::SocketClosed {
                    socket: SocketKind::Control,
                    detail: "a write stalled past the write deadline".to_owned(),
                });
                break;
            }
        }
    }
    rx.close();
    if let Some(CloseReason::ProtocolViolation(error)) = core.reason()
        && let Ok(encoded) = core.encode(&error, 0)
    {
        let _ = timeout(write, sink.send(Message::text(encoded.text))).await;
    }
    let _ = timeout(write, sink.send(Message::Close(None))).await;
}

/// Writes queued render frames, skipping those of cancelled playbacks.
async fn audio_writer(
    core: Arc<SessionCore>,
    mut sink: WsSink,
    mut rx: mpsc::Receiver<RenderOut>,
    write: std::time::Duration,
) {
    let _guard = PanicGuard {
        core: &core,
        task: "audio writer",
    };
    loop {
        let out = tokio::select! {
            biased;
            _ = core.stop.cancelled() => break,
            out = rx.recv() => out,
        };
        let Some(out) = out else { break };
        if out.playback.is_cancelled() {
            continue;
        }
        let failure = match timeout(write, sink.send(Message::Binary(out.frame))).await {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => error.to_string(),
            Err(_) => "a write stalled past the write deadline".to_owned(),
        };
        core.end(CloseReason::SocketClosed {
            socket: SocketKind::Audio,
            detail: failure,
        });
        break;
    }
    rx.close();
    let _ = timeout(write, sink.send(Message::Close(None))).await;
}

/// Reads uplink EMA1 frames into the ordered ingress channel.
async fn audio_reader(core: Arc<SessionCore>, mut stream: WsStream, ingress: IngressProducer) {
    let _guard = PanicGuard {
        core: &core,
        task: "audio reader",
    };
    loop {
        let message = tokio::select! {
            biased;
            _ = core.stop.cancelled() => break,
            message = stream.next() => message,
        };
        let outcome = match message {
            Some(Ok(Message::Binary(frame))) => {
                let bytes = frame.len();
                match Packet::parse(frame) {
                    Ok(packet) => ingress.push(Ingress::Audio(packet), bytes),
                    Err(error) => Err(CloseReason::ProtocolViolation(error.into())),
                }
            }
            Some(Ok(Message::Text(_))) => Err(CloseReason::ProtocolViolation(ProtocolError::new(
                ProtocolErrorCode::MalformedFrame,
                "audio frames are binary",
            ))),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => Ok(()),
            Some(Ok(Message::Close(_))) | None => Err(CloseReason::SocketClosed {
                socket: SocketKind::Audio,
                detail: "closed by the device".to_owned(),
            }),
            Some(Err(error)) => Err(CloseReason::SocketClosed {
                socket: SocketKind::Audio,
                detail: error.to_string(),
            }),
        };
        if let Err(reason) = outcome {
            core.end(reason);
            break;
        }
    }
}
