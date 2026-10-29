//! The nodes as Home Assistant sees them: a client built from the same
//! generated messages connects over loopback and does what Home Assistant's
//! ESPHome integration (aioesphomeapi 45.6.1) does.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_esphome::api::{self, ApiMessage};
use echomuse_esphome::bluetooth::{
    BleAddressType, BleAdvertisement, BluetoothProxy, BluetoothProxyConfig, start_bluetooth_proxy,
};
use echomuse_esphome::entities::{
    ButtonEvent, DeviceVolume, MediaState, Playback, VoiceEntities, VoiceState,
};
use echomuse_esphome::frame::{self, BAD_INDICATOR_REPLY, Frame, FrameDecoder};
use echomuse_esphome::voice::{
    Announcement, FinishError, MediaRequest, RunClosed, RunEvent, SatelliteRequest, StartRunError,
    TimerEvent, TimerEventKind, VoiceSatellite, VoiceSatelliteConfig, start_voice_satellite,
};
use echomuse_esphome::{CloseReason, ServerStatus};
use echomuse_protocol::Platform;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

const DEVICE: &str = "G090LF0965260F1J";
const WAIT: Duration = Duration::from_secs(5);

fn satellite_with(
    entities: VoiceEntities,
    request_capacity: usize,
) -> (VoiceSatellite, mpsc::Receiver<SatelliteRequest>) {
    start_voice_satellite(VoiceSatelliteConfig {
        device: DeviceId(DEVICE.to_owned()),
        label: "Kitchen".to_owned(),
        platform: Platform::FireOs6,
        project_version: "2.0.0".to_owned(),
        address: "127.0.0.1:0".parse().unwrap(),
        entities,
        request_capacity,
    })
    .unwrap()
}

fn satellite() -> (VoiceSatellite, mpsc::Receiver<SatelliteRequest>) {
    satellite_with(
        VoiceEntities {
            action_button: true,
            ambient_light: true,
        },
        64,
    )
}

fn proxy() -> BluetoothProxy {
    start_bluetooth_proxy(BluetoothProxyConfig {
        device: DeviceId(DEVICE.to_owned()),
        label: "Kitchen".to_owned(),
        platform: Platform::FireOs6,
        project_version: "2.0.0".to_owned(),
        address: "127.0.0.1:0".parse().unwrap(),
    })
    .unwrap()
}

async fn wait_status(
    status: &mut watch::Receiver<ServerStatus>,
    check: impl FnMut(&ServerStatus) -> bool,
) {
    timeout(WAIT, status.wait_for(check))
        .await
        .expect("status reached in time")
        .expect("node running");
}

async fn next_request(requests: &mut mpsc::Receiver<SatelliteRequest>) -> SatelliteRequest {
    timeout(WAIT, requests.recv())
        .await
        .expect("a request in time")
        .expect("node running")
}

/// A Home Assistant stand-in.
struct Client {
    stream: TcpStream,
    decoder: FrameDecoder,
}

impl Client {
    async fn connect(address: SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
            decoder: FrameDecoder::new(),
        }
    }

    /// Connects and completes the hello, as Home Assistant does first.
    async fn connected(address: SocketAddr) -> Self {
        let mut client = Self::connect(address).await;
        client.hello().await;
        client
    }

    async fn send<M: ApiMessage>(&mut self, message: &M) {
        self.stream
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }

    /// The next frame, or `None` once the node has closed the connection.
    async fn next_frame_within(&mut self, limit: Duration) -> Option<Frame> {
        timeout(limit, async {
            loop {
                if let Some(frame) = self.decoder.next_frame().expect("valid frames") {
                    return Some(frame);
                }
                match self.stream.read_buf(self.decoder.buffer_mut()).await {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {}
                }
            }
        })
        .await
        .expect("a frame or the close in time")
    }

    async fn next_frame(&mut self) -> Option<Frame> {
        self.next_frame_within(WAIT).await
    }

    /// The next message, which must be an `M`.
    async fn recv<M: ApiMessage>(&mut self) -> M {
        let frame = self.next_frame().await.expect("connection open");
        assert_eq!(
            frame.message_type,
            M::ID,
            "expected {} but got {:?}",
            M::NAME,
            api::message_name(frame.message_type)
        );
        frame.decode::<M>().unwrap()
    }

    async fn hello(&mut self) -> api::HelloResponse {
        self.send(&api::HelloRequest {
            client_info: "Home Assistant 2026.8.1".to_owned(),
            api_version_major: 1,
            api_version_minor: 14,
        })
        .await;
        self.recv::<api::HelloResponse>().await
    }

    /// Reads to the end of the stream, returning every byte that arrived.
    async fn read_to_end(&mut self) -> Vec<u8> {
        let mut bytes = self.decoder.buffer_mut().split().to_vec();
        timeout(WAIT, self.stream.read_to_end(&mut bytes))
            .await
            .expect("closed in time")
            .unwrap();
        bytes
    }

    /// Asserts the node closes the connection without sending anything more.
    async fn expect_closed(&mut self) {
        assert_eq!(self.read_to_end().await, Vec::<u8>::new());
    }

    /// Collects the entity list up to its end marker.
    async fn list_entities(&mut self) -> Vec<Frame> {
        self.send(&api::ListEntitiesRequest {}).await;
        let mut frames = Vec::new();
        loop {
            let frame = self.next_frame().await.expect("connection open");
            if frame.message_type == api::ListEntitiesDoneResponse::ID {
                return frames;
            }
            frames.push(frame);
        }
    }
}

#[tokio::test]
async fn a_noise_client_is_told_the_node_is_plaintext_and_closed() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();
    let mut client = Client::connect(satellite.local_addr()).await;
    // A Noise client hello: indicator 0x01, a 16-bit length, then the frame.
    client.send_raw(&[0x01, 0x00, 0x01, 0x00]).await;
    assert_eq!(client.read_to_end().await, BAD_INDICATOR_REPLY.to_vec());
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::BadIndicator)
    })
    .await;
}

#[tokio::test]
async fn frames_over_65535_bytes_are_refused_and_65535_is_accepted() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();

    let mut client = Client::connected(satellite.local_addr()).await;
    // Exactly the limit, of a message type this node does not handle.
    let payload = vec![0u8; 65_535];
    client
        .send_raw(&frame::encode_raw(9_999, &payload).unwrap())
        .await;
    client.send(&api::PingRequest {}).await;
    client.recv::<api::PingResponse>().await;

    // One byte over: the header alone ends the connection.
    client.send_raw(&[0x00, 0x80, 0x80, 0x04, 0x07]).await;
    client.expect_closed().await;
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::FrameTooLarge)
    })
    .await;
}

#[tokio::test]
async fn messages_before_the_hello_close_the_connection() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();
    let mut client = Client::connect(satellite.local_addr()).await;
    // A ping is allowed before the hello.
    client.send(&api::PingRequest {}).await;
    client.recv::<api::PingResponse>().await;
    client.send(&api::DeviceInfoRequest {}).await;
    client.expect_closed().await;
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::MessageBeforeHello)
    })
    .await;
}

#[tokio::test]
async fn hello_and_device_info_carry_the_satellite_identity() {
    let (satellite, _requests) = satellite();
    let mut client = Client::connect(satellite.local_addr()).await;
    assert_eq!(
        client.hello().await,
        api::HelloResponse {
            api_version_major: 1,
            api_version_minor: 10,
            server_info: "EchoMuse".to_owned(),
            name: "echomuse-lf0965260f1j".to_owned(),
        }
    );
    // No password: a no-password authentication request gets no answer, and
    // the connection carries on.
    client.send(&api::AuthenticationRequest::default()).await;
    client.send(&api::DeviceInfoRequest {}).await;
    assert_eq!(
        client.recv::<api::DeviceInfoResponse>().await,
        api::DeviceInfoResponse {
            name: "echomuse-lf0965260f1j".to_owned(),
            mac_address: "90:F0:96:52:60:F1".to_owned(),
            model: "Echo Dot Gen 2 (biscuit)".to_owned(),
            project_name: "EchoMuse.Echo Dot Gen 2 (biscuit)".to_owned(),
            project_version: "2.0.0".to_owned(),
            manufacturer: "EchoMuse".to_owned(),
            friendly_name: "Kitchen Voice Assistant".to_owned(),
            voice_assistant_feature_flags: 61,
            ..Default::default()
        }
    );
}

fn media_player_info() -> api::ListEntitiesMediaPlayerResponse {
    let format = |purpose: api::MediaPlayerFormatPurpose| api::MediaPlayerSupportedFormat {
        format: "flac".to_owned(),
        sample_rate: 48_000,
        num_channels: 1,
        purpose: purpose as i32,
        sample_bytes: 2,
    };
    api::ListEntitiesMediaPlayerResponse {
        object_id: "media_player".to_owned(),
        key: 1,
        name: "Kitchen".to_owned(),
        supports_pause: true,
        supported_formats: vec![
            format(api::MediaPlayerFormatPurpose::Default),
            format(api::MediaPlayerFormatPurpose::Announcement),
        ],
        feature_flags: 1_200_653,
        ..Default::default()
    }
}

#[tokio::test]
async fn the_entity_list_follows_the_capabilities() {
    let (satellite, _requests) = satellite();
    let mut client = Client::connected(satellite.local_addr()).await;
    let frames = client.list_entities().await;
    let types: Vec<u16> = frames.iter().map(|f| f.message_type).collect();
    assert_eq!(
        types,
        [
            api::ListEntitiesMediaPlayerResponse::ID,
            api::ListEntitiesEventResponse::ID,
            api::ListEntitiesSensorResponse::ID,
            api::ListEntitiesBinarySensorResponse::ID,
            api::ListEntitiesButtonResponse::ID,
            api::ListEntitiesTextSensorResponse::ID,
        ]
    );
    assert_eq!(
        frames[0]
            .decode::<api::ListEntitiesMediaPlayerResponse>()
            .unwrap(),
        media_player_info()
    );
    assert_eq!(
        frames[1]
            .decode::<api::ListEntitiesEventResponse>()
            .unwrap(),
        api::ListEntitiesEventResponse {
            object_id: "action_button".to_owned(),
            key: 2,
            name: "Kitchen Action Button".to_owned(),
            device_class: "button".to_owned(),
            event_types: ["long", "single", "double", "triple"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        }
    );
    assert_eq!(
        frames[2]
            .decode::<api::ListEntitiesSensorResponse>()
            .unwrap(),
        api::ListEntitiesSensorResponse {
            object_id: "ambient_light".to_owned(),
            key: 3,
            name: "Kitchen Ambient Light".to_owned(),
            unit_of_measurement: "lx".to_owned(),
            device_class: "illuminance".to_owned(),
            state_class: api::SensorStateClass::StateClassMeasurement as i32,
            ..Default::default()
        }
    );
    assert_eq!(
        frames[3]
            .decode::<api::ListEntitiesBinarySensorResponse>()
            .unwrap(),
        api::ListEntitiesBinarySensorResponse {
            object_id: "alert_ringing".to_owned(),
            key: 4,
            name: "Kitchen Alert ringing".to_owned(),
            device_class: "sound".to_owned(),
            ..Default::default()
        }
    );
    assert_eq!(
        frames[4]
            .decode::<api::ListEntitiesButtonResponse>()
            .unwrap(),
        api::ListEntitiesButtonResponse {
            object_id: "stop_alert".to_owned(),
            key: 5,
            name: "Kitchen Stop alert".to_owned(),
            device_class: "restart".to_owned(),
            ..Default::default()
        }
    );
    assert_eq!(
        frames[5]
            .decode::<api::ListEntitiesTextSensorResponse>()
            .unwrap(),
        api::ListEntitiesTextSensorResponse {
            object_id: "voice_state".to_owned(),
            key: 6,
            name: "Kitchen Voice state".to_owned(),
            icon: "mdi:account-voice".to_owned(),
            ..Default::default()
        }
    );

    // Without button_hold or a readable light sensor, those two are absent.
    let (plain, _requests) = satellite_with(VoiceEntities::default(), 8);
    let mut client = Client::connected(plain.local_addr()).await;
    let keys: Vec<(u16, u32)> = client
        .list_entities()
        .await
        .iter()
        .map(|frame| {
            // Every entity description has its key in field 2.
            let key = match frame.message_type {
                api::ListEntitiesMediaPlayerResponse::ID => {
                    frame
                        .decode::<api::ListEntitiesMediaPlayerResponse>()
                        .unwrap()
                        .key
                }
                api::ListEntitiesBinarySensorResponse::ID => {
                    frame
                        .decode::<api::ListEntitiesBinarySensorResponse>()
                        .unwrap()
                        .key
                }
                api::ListEntitiesButtonResponse::ID => {
                    frame
                        .decode::<api::ListEntitiesButtonResponse>()
                        .unwrap()
                        .key
                }
                api::ListEntitiesTextSensorResponse::ID => {
                    frame
                        .decode::<api::ListEntitiesTextSensorResponse>()
                        .unwrap()
                        .key
                }
                other => panic!("unexpected entity type {other}"),
            };
            (frame.message_type, key)
        })
        .collect();
    assert_eq!(
        keys,
        [
            (api::ListEntitiesMediaPlayerResponse::ID, 1),
            (api::ListEntitiesBinarySensorResponse::ID, 4),
            (api::ListEntitiesButtonResponse::ID, 5),
            (api::ListEntitiesTextSensorResponse::ID, 6),
        ]
    );
    assert_eq!(
        plain.set_ambient_light(Some(3.0)),
        Err(echomuse_esphome::voice::EntityAbsent("ambient_light"))
    );
    assert!(plain.fire_button_event(ButtonEvent::Single).await.is_err());
}

#[tokio::test]
async fn states_are_sent_on_subscription_and_pushed_when_they_change() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();
    satellite.set_media(Some(MediaState {
        playback: Playback::Idle,
        volume: DeviceVolume::new(127).unwrap(),
        muted: false,
    }));
    satellite.set_alert_ringing(Some(false));

    let mut client = Client::connected(satellite.local_addr()).await;
    client.send(&api::SubscribeStatesRequest {}).await;
    assert_eq!(
        client.recv::<api::MediaPlayerStateResponse>().await,
        api::MediaPlayerStateResponse {
            key: 1,
            state: api::MediaPlayerState::Idle as i32,
            volume: 1.0,
            muted: false,
            device_id: 0,
        }
    );
    // Unknown values go out as missing, never as zero.
    assert_eq!(
        client.recv::<api::SensorStateResponse>().await,
        api::SensorStateResponse {
            key: 3,
            state: 0.0,
            missing_state: true,
            device_id: 0,
        }
    );
    assert_eq!(
        client.recv::<api::BinarySensorStateResponse>().await,
        api::BinarySensorStateResponse {
            key: 4,
            state: false,
            missing_state: false,
            device_id: 0,
        }
    );
    assert_eq!(
        client.recv::<api::TextSensorStateResponse>().await,
        api::TextSensorStateResponse {
            key: 6,
            state: String::new(),
            missing_state: true,
            device_id: 0,
        }
    );
    wait_status(&mut status, |s| s.state_subscribers == 1).await;

    satellite.set_voice_state(Some(VoiceState::Listening));
    let state = client.recv::<api::TextSensorStateResponse>().await;
    assert_eq!(
        (state.key, state.state.as_str(), state.missing_state),
        (6, "listening", false)
    );

    // Setting a state to what it already is sends nothing: the next frame is
    // the next real change.
    satellite.set_voice_state(Some(VoiceState::Listening));
    satellite.set_media(Some(MediaState {
        playback: Playback::Announcing,
        volume: DeviceVolume::new(64).unwrap(),
        muted: true,
    }));
    let media = client.recv::<api::MediaPlayerStateResponse>().await;
    assert_eq!(media.state, api::MediaPlayerState::Playing as i32);
    assert_eq!(media.volume, 64.0 / 127.0);
    assert!(media.muted);

    satellite.set_ambient_light(Some(8.0)).unwrap();
    let light = client.recv::<api::SensorStateResponse>().await;
    assert_eq!(
        (light.key, light.state, light.missing_state),
        (3, 8.0, false)
    );
    satellite.set_ambient_light(None).unwrap();
    let light = client.recv::<api::SensorStateResponse>().await;
    assert!(light.missing_state);

    satellite.set_alert_ringing(Some(true));
    let ringing = client.recv::<api::BinarySensorStateResponse>().await;
    assert_eq!((ringing.key, ringing.state), (4, true));

    satellite
        .fire_button_event(ButtonEvent::Long)
        .await
        .unwrap();
    assert_eq!(
        client.recv::<api::EventResponse>().await,
        api::EventResponse {
            key: 2,
            event_type: "long".to_owned(),
            device_id: 0,
        }
    );
}

#[tokio::test]
async fn media_and_button_commands_arrive_typed() {
    let (satellite, mut requests) = satellite();
    let mut client = Client::connected(satellite.local_addr()).await;
    client
        .send(&api::MediaPlayerCommandRequest {
            key: 1,
            has_media_url: true,
            media_url: "http://ha:8123/api/esphome/ffmpeg_proxy/abc.flac".to_owned(),
            has_announcement: true,
            announcement: true,
            ..Default::default()
        })
        .await;
    client
        .send(&api::MediaPlayerCommandRequest {
            key: 1,
            has_volume: true,
            volume: 0.5,
            ..Default::default()
        })
        .await;
    for command in [
        api::MediaPlayerCommand::Pause,
        api::MediaPlayerCommand::Play,
        api::MediaPlayerCommand::Stop,
        api::MediaPlayerCommand::Mute,
        api::MediaPlayerCommand::Unmute,
    ] {
        client
            .send(&api::MediaPlayerCommandRequest {
                key: 1,
                has_command: true,
                command: command as i32,
                ..Default::default()
            })
            .await;
    }
    // A key this node does not have is ignored.
    client
        .send(&api::MediaPlayerCommandRequest {
            key: 7,
            has_command: true,
            command: api::MediaPlayerCommand::Stop as i32,
            ..Default::default()
        })
        .await;
    client
        .send(&api::ButtonCommandRequest {
            key: 99,
            device_id: 0,
        })
        .await;
    client
        .send(&api::ButtonCommandRequest {
            key: 5,
            device_id: 0,
        })
        .await;

    let expected = [
        SatelliteRequest::Media(MediaRequest::PlayUrl {
            url: "http://ha:8123/api/esphome/ffmpeg_proxy/abc.flac".to_owned(),
            announcement: true,
        }),
        SatelliteRequest::Media(MediaRequest::SetVolume(DeviceVolume::new(64).unwrap())),
        SatelliteRequest::Media(MediaRequest::Pause),
        SatelliteRequest::Media(MediaRequest::Play),
        SatelliteRequest::Media(MediaRequest::Stop),
        SatelliteRequest::Media(MediaRequest::Mute),
        SatelliteRequest::Media(MediaRequest::Unmute),
        SatelliteRequest::StopAlert,
    ];
    for request in expected {
        assert_eq!(next_request(&mut requests).await, request);
    }
}

#[tokio::test]
async fn announcements_wait_for_the_runtime_to_finish_them() {
    let (satellite, mut requests) = satellite();
    let mut client = Client::connected(satellite.local_addr()).await;
    client
        .send(&api::VoiceAssistantAnnounceRequest {
            media_id: "http://ha:8123/api/tts_proxy/x.flac".to_owned(),
            text: "Dinner is ready".to_owned(),
            preannounce_media_id: "http://ha:8123/chime.flac".to_owned(),
            start_conversation: true,
        })
        .await;
    let SatelliteRequest::Announce(announcement) = next_request(&mut requests).await else {
        panic!("expected an announcement");
    };
    assert_eq!(
        announcement,
        Announcement {
            id: announcement.id,
            media_url: "http://ha:8123/api/tts_proxy/x.flac".to_owned(),
            text: Some("Dinner is ready".to_owned()),
            preannounce_url: Some("http://ha:8123/chime.flac".to_owned()),
            start_conversation: true,
        }
    );
    satellite
        .finish_announcement(announcement.id, true)
        .await
        .unwrap();
    assert_eq!(
        client.recv::<api::VoiceAssistantAnnounceFinished>().await,
        api::VoiceAssistantAnnounceFinished { success: true }
    );
    assert_eq!(
        satellite.finish_announcement(announcement.id, true).await,
        Err(FinishError::NotPending)
    );

    // Without a preannounce chime or text, those are absent.
    client
        .send(&api::VoiceAssistantAnnounceRequest {
            media_id: "http://ha:8123/b.flac".to_owned(),
            ..Default::default()
        })
        .await;
    let SatelliteRequest::Announce(plain) = next_request(&mut requests).await else {
        panic!("expected an announcement");
    };
    assert_ne!(plain.id, announcement.id);
    assert_eq!(
        (plain.text, plain.preannounce_url, plain.start_conversation),
        (None, None, false)
    );

    // Home Assistant asks for the satellite configuration before it offers
    // announcements; the satellite has no wake words for it to choose.
    client
        .send(&api::VoiceAssistantConfigurationRequest::default())
        .await;
    assert_eq!(
        client
            .recv::<api::VoiceAssistantConfigurationResponse>()
            .await,
        api::VoiceAssistantConfigurationResponse::default()
    );
}

#[tokio::test]
async fn timer_events_arrive_typed() {
    let (satellite, mut requests) = satellite();
    let mut client = Client::connected(satellite.local_addr()).await;
    let kinds = [
        (
            api::VoiceAssistantTimerEvent::VoiceAssistantTimerStarted,
            TimerEventKind::Started,
        ),
        (
            api::VoiceAssistantTimerEvent::VoiceAssistantTimerUpdated,
            TimerEventKind::Updated,
        ),
        (
            api::VoiceAssistantTimerEvent::VoiceAssistantTimerCancelled,
            TimerEventKind::Cancelled,
        ),
        (
            api::VoiceAssistantTimerEvent::VoiceAssistantTimerFinished,
            TimerEventKind::Finished,
        ),
    ];
    for (wire, _) in kinds {
        client
            .send(&api::VoiceAssistantTimerEventResponse {
                event_type: wire as i32,
                timer_id: "01JTIMER".to_owned(),
                name: if wire == api::VoiceAssistantTimerEvent::VoiceAssistantTimerStarted {
                    "pasta".to_owned()
                } else {
                    String::new()
                },
                total_seconds: 600,
                seconds_left: 420,
                is_active: wire != api::VoiceAssistantTimerEvent::VoiceAssistantTimerUpdated,
            })
            .await;
    }
    for (wire, kind) in kinds {
        assert_eq!(
            next_request(&mut requests).await,
            SatelliteRequest::Timer(TimerEvent {
                kind,
                timer_id: "01JTIMER".to_owned(),
                name: (kind == TimerEventKind::Started).then(|| "pasta".to_owned()),
                total_seconds: 600,
                seconds_left: 420,
                is_active: wire != api::VoiceAssistantTimerEvent::VoiceAssistantTimerUpdated,
            })
        );
    }
}

fn pipeline_event(
    kind: api::VoiceAssistantEvent,
    data: &[(&str, &str)],
) -> api::VoiceAssistantEventResponse {
    api::VoiceAssistantEventResponse {
        event_type: kind as i32,
        data: data
            .iter()
            .map(|(name, value)| api::VoiceAssistantEventData {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

async fn subscribed_voice_client(satellite: &VoiceSatellite) -> Client {
    let mut status = satellite.status();
    let mut client = Client::connected(satellite.local_addr()).await;
    // Home Assistant's client sends flag 4 for audio over the API.
    client
        .send(&api::SubscribeVoiceAssistantRequest {
            subscribe: true,
            flags: 4,
        })
        .await;
    wait_status(&mut status, |s| s.subscribed).await;
    client
}

#[tokio::test]
async fn a_controller_started_run_streams_audio_and_maps_the_pipeline() {
    let (satellite, mut requests) = satellite();
    assert_eq!(
        satellite.start_run(None).await,
        Err(StartRunError::NotSubscribed)
    );
    let mut client = subscribed_voice_client(&satellite).await;

    let run = satellite
        .start_run(Some("01JCONV".to_owned()))
        .await
        .unwrap();
    assert_eq!(
        client.recv::<api::VoiceAssistantRequest>().await,
        api::VoiceAssistantRequest {
            start: true,
            conversation_id: "01JCONV".to_owned(),
            flags: 0,
            audio_settings: Some(api::VoiceAssistantAudioSettings {
                noise_suppression_level: 0,
                auto_gain: 0,
                volume_multiplier: 1.0,
            }),
            wake_word_phrase: String::new(),
        }
    );

    // Audio before Home Assistant answers waits for the answer.
    let samples: Vec<i16> = (0..1000).map(|i| i as i16).collect();
    satellite.send_run_audio(run, &samples).await.unwrap();
    client
        .send(&api::VoiceAssistantResponse {
            port: 0,
            error: false,
        })
        .await;
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run,
            event: RunEvent::Accepted
        }
    );
    let mut received = Vec::new();
    for _ in 0..3 {
        let chunk = client.recv::<api::VoiceAssistantAudio>().await;
        assert_eq!(chunk.data.len(), 640);
        assert!(!chunk.end);
        received.extend_from_slice(&chunk.data);
    }
    // 2000 bytes: three whole chunks out, 80 bytes held until the end.
    satellite.end_run_audio(run).await.unwrap();
    let tail = client.recv::<api::VoiceAssistantAudio>().await;
    assert_eq!((tail.data.len(), tail.end), (80, false));
    received.extend_from_slice(&tail.data);
    let end = client.recv::<api::VoiceAssistantAudio>().await;
    assert_eq!(
        end,
        api::VoiceAssistantAudio {
            data: Bytes::new(),
            end: true,
            data2: Bytes::new()
        }
    );
    let expected: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    assert_eq!(received, expected);
    // Audio after the end is discarded.
    satellite.send_run_audio(run, &samples).await.unwrap();

    use api::VoiceAssistantEvent as E;
    let events = [
        pipeline_event(E::VoiceAssistantRunStart, &[]),
        pipeline_event(E::VoiceAssistantSttStart, &[]),
        pipeline_event(E::VoiceAssistantSttVadStart, &[]),
        pipeline_event(E::VoiceAssistantSttVadEnd, &[]),
        pipeline_event(E::VoiceAssistantSttEnd, &[("text", "what time is it")]),
        pipeline_event(E::VoiceAssistantIntentStart, &[]),
        pipeline_event(
            E::VoiceAssistantIntentEnd,
            &[
                ("conversation_id", "01JCONV"),
                ("continue_conversation", "0"),
                ("speech", "It is 9:41."),
            ],
        ),
        pipeline_event(E::VoiceAssistantTtsStart, &[("text", "It is 9:41.")]),
        pipeline_event(
            E::VoiceAssistantTtsEnd,
            &[("url", "http://ha:8123/api/tts_proxy/t.flac")],
        ),
        pipeline_event(E::VoiceAssistantRunEnd, &[]),
    ];
    for event in &events {
        client.send(event).await;
    }
    let mapped = [
        RunEvent::Started { tts_url: None },
        RunEvent::SttStarted,
        RunEvent::VadStarted,
        RunEvent::VadEnded,
        RunEvent::SttEnded {
            text: "what time is it".to_owned(),
        },
        RunEvent::IntentStarted,
        RunEvent::IntentEnded {
            conversation_id: Some("01JCONV".to_owned()),
            continue_conversation: false,
            speech: "It is 9:41.".to_owned(),
        },
        RunEvent::TtsStarted {
            text: "It is 9:41.".to_owned(),
        },
        RunEvent::TtsEnded {
            url: Some("http://ha:8123/api/tts_proxy/t.flac".to_owned()),
        },
        RunEvent::Closed(RunClosed::Finished),
    ];
    for event in mapped {
        assert_eq!(
            next_request(&mut requests).await,
            SatelliteRequest::Run { run, event }
        );
    }

    // The reply played: Home Assistant hears it finished.
    satellite.finish_run_response(run, true).await.unwrap();
    assert_eq!(
        client.recv::<api::VoiceAssistantAnnounceFinished>().await,
        api::VoiceAssistantAnnounceFinished { success: true }
    );
    assert_eq!(
        satellite.finish_run_response(run, true).await,
        Err(FinishError::NotPending)
    );
}

#[tokio::test]
async fn a_new_run_aborts_the_old_and_ignores_its_late_events() {
    let (satellite, mut requests) = satellite();
    let mut client = subscribed_voice_client(&satellite).await;

    let first = satellite.start_run(None).await.unwrap();
    client.recv::<api::VoiceAssistantRequest>().await;
    client
        .send(&api::VoiceAssistantResponse {
            port: 0,
            error: false,
        })
        .await;
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run: first,
            event: RunEvent::Accepted
        }
    );

    let second = satellite.start_run(None).await.unwrap();
    assert_ne!(first, second);
    // The old pipeline is stopped, then the new one requested.
    assert_eq!(
        client.recv::<api::VoiceAssistantRequest>().await,
        api::VoiceAssistantRequest::default()
    );
    assert!(client.recv::<api::VoiceAssistantRequest>().await.start);
    // An event Home Assistant sent for the old run before answering the new
    // start belongs to no current run and is dropped.
    client
        .send(&pipeline_event(
            api::VoiceAssistantEvent::VoiceAssistantSttStart,
            &[],
        ))
        .await;
    client
        .send(&api::VoiceAssistantResponse {
            port: 0,
            error: true,
        })
        .await;
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run: second,
            event: RunEvent::Closed(RunClosed::Refused)
        }
    );

    // A run whose connection goes away is lost.
    let third = satellite.start_run(None).await.unwrap();
    client.recv::<api::VoiceAssistantRequest>().await;
    drop(client);
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run: third,
            event: RunEvent::Closed(RunClosed::Lost)
        }
    );
    assert_eq!(
        satellite.start_run(None).await,
        Err(StartRunError::NotSubscribed)
    );
}

#[tokio::test]
async fn a_runs_end_waits_for_room_rather_than_being_dropped() {
    let (satellite, mut requests) = satellite_with(VoiceEntities::default(), 1);
    let mut client = subscribed_voice_client(&satellite).await;
    let run = satellite.start_run(None).await.unwrap();
    client.recv::<api::VoiceAssistantRequest>().await;
    client
        .send(&api::VoiceAssistantResponse {
            port: 0,
            error: false,
        })
        .await;
    // The queue's one place holds the acceptance; the end must wait.
    client
        .send(&pipeline_event(
            api::VoiceAssistantEvent::VoiceAssistantRunEnd,
            &[],
        ))
        .await;
    // The node answers this only after handling the end, so the end met a
    // full queue.
    client
        .send(&api::VoiceAssistantConfigurationRequest::default())
        .await;
    client
        .recv::<api::VoiceAssistantConfigurationResponse>()
        .await;
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run,
            event: RunEvent::Accepted
        }
    );
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::Run {
            run,
            event: RunEvent::Closed(RunClosed::Finished)
        }
    );
    // The connection was not closed for it.
    assert_eq!(satellite.status().borrow().connections, 1);
}

#[tokio::test]
async fn a_runtime_that_stops_reading_requests_closes_the_connection() {
    let (satellite, mut requests) = satellite_with(VoiceEntities::default(), 1);
    let mut status = satellite.status();
    let mut client = Client::connected(satellite.local_addr()).await;
    client
        .send(&api::ButtonCommandRequest {
            key: 5,
            device_id: 0,
        })
        .await;
    client
        .send(&api::ButtonCommandRequest {
            key: 5,
            device_id: 0,
        })
        .await;
    client.expect_closed().await;
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::RequestsBacklogged)
    })
    .await;
    // The first request was delivered, not lost.
    assert_eq!(
        next_request(&mut requests).await,
        SatelliteRequest::StopAlert
    );
}

#[tokio::test]
async fn home_assistant_can_reconnect_after_disconnecting() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();
    let address = satellite.local_addr();

    let mut client = Client::connected(address).await;
    client.send(&api::DisconnectRequest::default()).await;
    client.recv::<api::DisconnectResponse>().await;
    client.expect_closed().await;
    wait_status(&mut status, |s| {
        s.connections == 0 && s.last_close == Some(CloseReason::ClientDisconnected)
    })
    .await;

    let mut client = Client::connected(address).await;
    client.send(&api::SubscribeStatesRequest {}).await;
    wait_status(&mut status, |s| s.state_subscribers == 1).await;
    // A connection that just drops is forgotten too.
    drop(client);
    wait_status(&mut status, |s| {
        s.connections == 0 && s.last_close == Some(CloseReason::ConnectionLost)
    })
    .await;

    let mut client = Client::connected(address).await;
    client.send(&api::DeviceInfoRequest {}).await;
    assert_eq!(
        client.recv::<api::DeviceInfoResponse>().await.name,
        "echomuse-lf0965260f1j"
    );
}

#[tokio::test]
async fn shutdown_disconnects_clients_and_releases_the_port() {
    let (satellite, _requests) = satellite();
    let address = satellite.local_addr();
    let mut client = Client::connected(address).await;
    let ha = tokio::spawn(async move {
        client.recv::<api::DisconnectRequest>().await;
        client.send(&api::DisconnectResponse {}).await;
        client.expect_closed().await;
    });
    timeout(WAIT, satellite.shutdown())
        .await
        .expect("shutdown is finite");
    ha.await.unwrap();
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn silent_clients_are_pinged_then_dropped() {
    let (satellite, _requests) = satellite();
    let mut status = satellite.status();

    // No hello within a minute.
    let mut client = Client::connect(satellite.local_addr()).await;
    assert!(
        client
            .next_frame_within(Duration::from_secs(120))
            .await
            .is_none()
    );
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::HandshakeTimeout)
    })
    .await;

    // After the hello, a minute of silence draws a ping, and two and a half
    // without an answer end the connection.
    let mut client = Client::connected(satellite.local_addr()).await;
    let ping = client
        .next_frame_within(Duration::from_secs(120))
        .await
        .unwrap();
    assert_eq!(ping.message_type, api::PingRequest::ID);
    assert!(
        client
            .next_frame_within(Duration::from_secs(120))
            .await
            .is_none()
    );
    wait_status(&mut status, |s| {
        s.last_close == Some(CloseReason::KeepaliveTimeout)
    })
    .await;
}

fn advert(last: u8) -> BleAdvertisement {
    BleAdvertisement::new(
        [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, last],
        BleAddressType::Public,
        -60,
        Bytes::from(vec![0x02, 0x01, 0x06, last]),
    )
    .unwrap()
}

#[tokio::test]
async fn the_proxy_forwards_advertisements_only_while_subscribed() {
    let proxy = proxy();
    let mut status = proxy.status();
    let mut client = Client::connect(proxy.local_addr()).await;
    assert_eq!(client.hello().await.name, "echomuse-lf0965260f1j-bt");
    client.send(&api::DeviceInfoRequest {}).await;
    assert_eq!(
        client.recv::<api::DeviceInfoResponse>().await,
        api::DeviceInfoResponse {
            name: "echomuse-lf0965260f1j-bt".to_owned(),
            mac_address: "92:F0:96:52:60:F1".to_owned(),
            model: "Echo Dot Gen 2 (biscuit)".to_owned(),
            project_name: "EchoMuse.Echo Dot Gen 2 (biscuit)".to_owned(),
            project_version: "2.0.0".to_owned(),
            manufacturer: "EchoMuse".to_owned(),
            friendly_name: "Kitchen BT Proxy".to_owned(),
            bluetooth_proxy_feature_flags: 33,
            ..Default::default()
        }
    );
    let entities = client.list_entities().await;
    assert_eq!(entities.len(), 1);
    let sensor = entities[0]
        .decode::<api::ListEntitiesSensorResponse>()
        .unwrap();
    assert_eq!(
        (
            sensor.key,
            sensor.object_id.as_str(),
            sensor.entity_category
        ),
        (
            1,
            "ble_advertisements",
            api::EntityCategory::Diagnostic as i32
        )
    );
    client.send(&api::SubscribeStatesRequest {}).await;
    let count = client.recv::<api::SensorStateResponse>().await;
    assert_eq!(
        (count.key, count.state, count.missing_state),
        (1, 0.0, false)
    );

    // Not subscribed yet: nowhere to go.
    proxy.forward(vec![advert(0)]).unwrap();
    client
        .send(&api::SubscribeBluetoothLeAdvertisementsRequest { flags: 1 })
        .await;
    wait_status(&mut status, |s| s.subscribed).await;

    proxy.forward((1..=20).map(advert).collect()).unwrap();
    let first = client
        .recv::<api::BluetoothLeRawAdvertisementsResponse>()
        .await;
    let second = client
        .recv::<api::BluetoothLeRawAdvertisementsResponse>()
        .await;
    assert_eq!(
        (first.advertisements.len(), second.advertisements.len()),
        (16, 4)
    );
    assert_eq!(
        first.advertisements[0],
        api::BluetoothLeRawAdvertisement {
            address: 0xAABB_CCDD_EE01,
            rssi: -60,
            address_type: 0,
            data: Bytes::from_static(&[0x02, 0x01, 0x06, 0x01]),
        }
    );
    assert_eq!(second.advertisements[3].address, 0xAABB_CCDD_EE14);

    client
        .send(&api::UnsubscribeBluetoothLeAdvertisementsRequest {})
        .await;
    wait_status(&mut status, |s| !s.subscribed).await;
    proxy.forward(vec![advert(30)]).unwrap();
    client
        .send(&api::SubscribeBluetoothLeAdvertisementsRequest { flags: 1 })
        .await;
    wait_status(&mut status, |s| s.subscribed).await;
    proxy.forward(vec![advert(31)]).unwrap();
    // The advertisement sent while unsubscribed never arrives.
    let next = client
        .recv::<api::BluetoothLeRawAdvertisementsResponse>()
        .await;
    assert_eq!(next.advertisements.len(), 1);
    assert_eq!(next.advertisements[0].address, 0xAABB_CCDD_EE1F);
    proxy.shutdown().await;
}
