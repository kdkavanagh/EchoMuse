//! Interoperability with Home Assistant's own ESPHome client library,
//! aioesphomeapi, driving both nodes over loopback.
//!
//! Opt-in: it needs a Python interpreter with aioesphomeapi 45.6.1 (the
//! version Home Assistant 2026.8.1 pins) installed. Run it with
//!
//! ```sh
//! ECHOMUSE_AIOESPHOMEAPI_PYTHON=/path/to/python cargo test -p echomuse-esphome \
//!     --test aioesphomeapi_interop -- --ignored
//! ```

use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use echomuse_core::{services::TimerEventKind, types::DeviceId};
use echomuse_esphome::bluetooth::{
    BleAddressType, BleAdvertisement, BluetoothProxyConfig, start_bluetooth_proxy,
};
use echomuse_esphome::entities::{DeviceVolume, MediaState, Playback, VoiceEntities, VoiceState};
use echomuse_esphome::voice::{
    MediaRequest, RunClosed, RunEvent, SatelliteRequest, VoiceSatelliteConfig,
    start_voice_satellite,
};
use echomuse_protocol::Platform;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

const PYTHON_ENV: &str = "ECHOMUSE_AIOESPHOMEAPI_PYTHON";

/// What Home Assistant's side does, with its own checks. It prints
/// `interop ok` once every check has passed.
const SCRIPT: &str = r#"
import asyncio, sys
from importlib.metadata import version
from aioesphomeapi import (
    APIClient, MediaPlayerCommand, VoiceAssistantEventType as E,
    VoiceAssistantTimerEventType,
)

def check(label, actual, expected):
    if actual != expected:
        sys.exit(f"{label}: expected {expected!r}, got {actual!r}")

async def voice(port):
    cli = APIClient("127.0.0.1", port, None, client_info="Home Assistant 2026.8.1",
                    expected_name="echomuse-lf0965260f1j")
    await cli.connect(login=True)
    check("api version", (cli.api_version.major, cli.api_version.minor), (1, 10))
    info, entities, services = await cli.device_info_and_list_entities()
    check("name", info.name, "echomuse-lf0965260f1j")
    check("mac", info.mac_address, "90:F0:96:52:60:F1")
    check("friendly name", info.friendly_name, "Kitchen Voice Assistant")
    check("model", info.model, "Echo Dot Gen 2 (biscuit)")
    check("manufacturer", info.manufacturer, "EchoMuse")
    check("project", (info.project_name, info.project_version),
          ("EchoMuse.Echo Dot Gen 2 (biscuit)", "2.0.0"))
    check("voice flags", info.voice_assistant_feature_flags_compat(cli.api_version), 61)
    check("services", services, [])
    check("entities", [(type(e).__name__, e.key, e.object_id, e.name) for e in entities], [
        ("MediaPlayerInfo", 1, "media_player", "Kitchen"),
        ("EventInfo", 2, "action_button", "Kitchen Action Button"),
        ("SensorInfo", 3, "ambient_light", "Kitchen Ambient Light"),
        ("BinarySensorInfo", 4, "alert_ringing", "Kitchen Alert ringing"),
        ("ButtonInfo", 5, "stop_alert", "Kitchen Stop alert"),
        ("TextSensorInfo", 6, "voice_state", "Kitchen Voice state"),
    ])
    check("media features", entities[0].feature_flags_compat(cli.api_version), 1200653)
    check("event types", list(entities[1].event_types), ["long", "single", "double", "triple"])

    states = {}
    got_states = asyncio.Event()
    def on_state(state):
        states[state.key] = state
        if len(states) == 4:
            got_states.set()
    cli.subscribe_states(on_state)
    await asyncio.wait_for(got_states.wait(), 10)
    check("media state", (states[1].state.name, states[1].volume, states[1].muted),
          ("IDLE", 1.0, False))
    check("light", (states[3].state, states[3].missing_state), (8.0, False))
    check("ringing", (states[4].state, states[4].missing_state), (False, False))
    check("voice state", (states[6].state, states[6].missing_state), ("idle", False))

    audio = bytearray()
    started = {}
    stopped = asyncio.Event()
    finished = []
    async def handle_start(conversation_id, flags, audio_settings, wake_word_phrase):
        started.update(conversation_id=conversation_id, flags=flags, wake_word=wake_word_phrase)
        return 0
    async def handle_stop(abort):
        started["abort"] = abort
        stopped.set()
    async def handle_audio(data, data2):
        audio.extend(data)
    async def handle_finished(message):
        finished.append(message.success)
    cli.subscribe_voice_assistant(handle_start=handle_start, handle_stop=handle_stop,
                                  handle_audio=handle_audio,
                                  handle_announcement_finished=handle_finished)
    config = await cli.get_voice_assistant_configuration(10)
    check("wake words", (list(config.available_wake_words), list(config.active_wake_words),
                         config.max_active_wake_words), ([], [], 0))

    cli.media_player_command(1, volume=0.5)
    cli.media_player_command(1, media_url="http://ha.local/a.flac", announcement=True)
    cli.media_player_command(1, command=MediaPlayerCommand.PAUSE)
    cli.button_command(5)
    cli.send_voice_assistant_timer_event(
        VoiceAssistantTimerEventType.VOICE_ASSISTANT_TIMER_STARTED, "t1", "pasta", 600, 600, True)
    result = await cli.send_voice_assistant_announcement_await_response(
        "http://ha.local/announce.flac", 10, "Dinner is ready", start_conversation=True)
    check("announcement", result.success, True)

    # The runtime starts the conversation once the announcement has played.
    await asyncio.wait_for(stopped.wait(), 10)
    await asyncio.sleep(0.2)
    check("run start", started, {"conversation_id": "", "flags": 0, "wake_word": None,
                                 "abort": False})
    check("audio", bytes(audio), bytes(i % 256 for i in range(2000)))
    for event, data in [
        (E.VOICE_ASSISTANT_RUN_START, {}),
        (E.VOICE_ASSISTANT_STT_START, {}),
        (E.VOICE_ASSISTANT_STT_END, {"text": "what time is it"}),
        (E.VOICE_ASSISTANT_INTENT_START, {}),
        (E.VOICE_ASSISTANT_INTENT_END, {"conversation_id": "c1", "continue_conversation": "1",
                                        "speech": "It is 9:41."}),
        (E.VOICE_ASSISTANT_TTS_START, {"text": "It is 9:41."}),
        (E.VOICE_ASSISTANT_TTS_END, {"url": "http://ha.local/tts.flac"}),
        (E.VOICE_ASSISTANT_RUN_END, {}),
    ]:
        cli.send_voice_assistant_event(event, data)
    for _ in range(100):
        if len(finished) == 2:
            break
        await asyncio.sleep(0.1)
    check("finished", finished, [True, True])
    await cli.disconnect()

async def proxy(port):
    cli = APIClient("127.0.0.1", port, None, client_info="Home Assistant 2026.8.1",
                    expected_name="echomuse-lf0965260f1j-bt")
    await cli.connect(login=True)
    info, entities, _ = await cli.device_info_and_list_entities()
    check("proxy mac", info.mac_address, "92:F0:96:52:60:F1")
    check("proxy flags", info.bluetooth_proxy_feature_flags_compat(cli.api_version), 33)
    check("proxy entities", [(e.key, e.object_id, e.entity_category.name) for e in entities],
          [(1, "ble_advertisements", "DIAGNOSTIC")])
    adverts = []
    got = asyncio.Event()
    def on_adverts(message):
        adverts.extend(message.advertisements)
        if len(adverts) >= 3:
            got.set()
    cli.subscribe_bluetooth_le_raw_advertisements(on_adverts)
    await asyncio.wait_for(got.wait(), 10)
    check("adverts", [(a.address, a.rssi, a.address_type, bytes(a.data)) for a in adverts], [
        (0xAABBCCDDEE00 + i, -60 - i, 1, bytes([2, 1, 6, i])) for i in range(3)])
    await cli.disconnect()

async def main(voice_port, proxy_port):
    print("aioesphomeapi", version("aioesphomeapi"))
    await asyncio.wait_for(voice(voice_port), 60)
    await asyncio.wait_for(proxy(proxy_port), 60)
    print("interop ok")

asyncio.run(main(int(sys.argv[1]), int(sys.argv[2])))
"#;

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test]
#[ignore = "needs ECHOMUSE_AIOESPHOMEAPI_PYTHON: a Python with aioesphomeapi 45.6.1"]
async fn home_assistants_client_library_drives_both_nodes() {
    let python = std::env::var(PYTHON_ENV).unwrap_or_else(|_| {
        panic!("set {PYTHON_ENV} to a Python interpreter with aioesphomeapi 45.6.1 installed")
    });
    let device = DeviceId("G090LF0965260F1J".to_owned());
    let (satellite, mut requests) = start_voice_satellite(VoiceSatelliteConfig {
        device: device.clone(),
        label: "Kitchen".to_owned(),
        platform: Platform::FireOs6,
        project_version: "2.0.0".to_owned(),
        address: "127.0.0.1:0".parse().unwrap(),
        entities: VoiceEntities {
            action_button: true,
            ambient_light: true,
        },
        request_capacity: 64,
    })
    .unwrap();
    let proxy = start_bluetooth_proxy(BluetoothProxyConfig {
        device,
        label: "Kitchen".to_owned(),
        platform: Platform::FireOs6,
        project_version: "2.0.0".to_owned(),
        address: "127.0.0.1:0".parse().unwrap(),
    })
    .unwrap();
    satellite.set_media(Some(MediaState {
        playback: Playback::Idle,
        volume: DeviceVolume::MAX,
        muted: false,
    }));
    satellite.set_ambient_light(Some(8.0)).unwrap();
    satellite.set_alert_ringing(Some(false));
    satellite.set_voice_state(Some(VoiceState::Idle));

    let mut child = Command::new(&python)
        .arg("-c")
        .arg(SCRIPT)
        .arg(satellite.local_addr().port().to_string())
        .arg(proxy.local_addr().port().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|error| panic!("starting {python}: {error}"));

    let mut next = async || {
        timeout(WAIT, requests.recv())
            .await
            .expect("a request in time")
            .expect("node running")
    };
    assert_eq!(
        next().await,
        SatelliteRequest::Media(MediaRequest::SetVolume(DeviceVolume::new(64).unwrap()))
    );
    assert_eq!(
        next().await,
        SatelliteRequest::Media(MediaRequest::PlayUrl {
            url: "http://ha.local/a.flac".to_owned(),
            announcement: true
        })
    );
    assert_eq!(next().await, SatelliteRequest::Media(MediaRequest::Pause));
    assert_eq!(next().await, SatelliteRequest::StopAlert);
    let SatelliteRequest::Timer(timer) = next().await else {
        panic!("expected a timer event");
    };
    assert_eq!(
        (timer.kind, timer.name.as_deref(), timer.total_seconds),
        (TimerEventKind::Started, Some("pasta"), 600)
    );
    let SatelliteRequest::Announce(announcement) = next().await else {
        panic!("expected an announcement");
    };
    assert!(announcement.start_conversation);
    satellite
        .finish_announcement(announcement.id, true)
        .await
        .unwrap();

    let run = satellite.start_run(None).await.unwrap();
    assert_eq!(
        next().await,
        SatelliteRequest::Run {
            run,
            event: RunEvent::Accepted
        }
    );
    let pcm: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
    let samples: Vec<i16> = pcm
        .chunks(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    satellite.send_run_audio(run, &samples).await.unwrap();
    satellite.end_run_audio(run).await.unwrap();
    let mut events = Vec::new();
    loop {
        let SatelliteRequest::Run { run: id, event } = next().await else {
            panic!("expected run events");
        };
        assert_eq!(id, run);
        let closed = event == RunEvent::Closed(RunClosed::Finished);
        events.push(event);
        if closed {
            break;
        }
    }
    assert_eq!(events.len(), 8);
    assert_eq!(
        events[4],
        RunEvent::IntentEnded {
            conversation_id: Some("c1".to_owned()),
            continue_conversation: true,
            speech: "It is 9:41.".to_owned()
        }
    );
    satellite.finish_run_response(run, true).await.unwrap();

    let mut proxy_status = proxy.status();
    timeout(WAIT, proxy_status.wait_for(|s| s.subscribed))
        .await
        .expect("Home Assistant subscribed to advertisements")
        .unwrap();
    let adverts = (0..3u8)
        .map(|i| {
            BleAdvertisement::new(
                [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, i],
                BleAddressType::Random,
                -60 - i32::from(i),
                Bytes::from(vec![2, 1, 6, i]),
            )
            .unwrap()
        })
        .collect();
    proxy.forward(adverts).unwrap();

    let status = timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("the client finished")
        .unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .await
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(
        status.success() && stdout.contains("interop ok"),
        "aioesphomeapi client failed ({status}):\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("aioesphomeapi 45.6.1"), "{stdout}");
    satellite.shutdown().await;
    proxy.shutdown().await;
}
