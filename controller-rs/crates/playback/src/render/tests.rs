use bytes::Bytes;
use echomuse_protocol::Capabilities;
use echomuse_protocol::ema::{Content, Kind, Packet};
use serde_json::json;
use tokio::time::{self, Instant};

use super::*;
use crate::sim::{SimLink, finished, progress, rejected};
use crate::wire::{AckFailure, Command, Earcon};

fn client() -> RenderClient {
    RenderClient::new(&Capabilities::default())
}

fn dialog(limits: Option<ResponseLimits>) -> NetworkStart {
    NetworkStart {
        class: NetworkClass::DialogOutput,
        gain_db: 0.0,
        announcement: false,
        limits,
    }
}

fn content() -> NetworkStart {
    NetworkStart {
        class: NetworkClass::Content,
        gain_db: -3.0,
        announcement: false,
        limits: None,
    }
}

fn tone(frames: usize) -> Vec<i16> {
    (0..frames).map(|i| (i % 1000) as i16).collect()
}

fn events(client: &mut RenderClient) -> Vec<RenderEvent> {
    std::iter::from_fn(|| client.next_event()).collect()
}

/// Starts a playback and acknowledges its start.
fn running(client: &mut RenderClient, link: &mut SimLink, now: Instant) -> PlaybackRef {
    let playback = client.start_network(link, content()).unwrap();
    let (id, _, _) = link.last_command();
    assert!(client.on_ack(link, now, &id, AckOutcome::Accepted));
    playback
}

#[test]
fn render_start_names_a_fresh_nonzero_epoch_and_no_frame_precedes_the_ack() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client.start_network(&mut link, dialog(None)).unwrap();
    assert_eq!(playback.generation(), 1);

    let (start_id, command, generation) = link.last_command();
    assert_eq!(generation, 1);
    let body = command.body_json();
    assert_eq!(body["source_class"], "dialog_output");
    assert_eq!(body["format"], 1);
    assert_eq!(body["local_asset"], json!(null));
    let epoch: u64 = body["epoch"].as_str().unwrap().parse().unwrap();
    assert_ne!(epoch, 0);

    // A second of audio, then the source ends, all before the device answers.
    client
        .push_audio(&mut link, now, &playback, &tone(48_000))
        .unwrap();
    client.finish_audio(&mut link, now, &playback).unwrap();
    assert!(link.frames().is_empty(), "no frame before the ack");
    assert_eq!(link.commands().len(), 1, "no render.end before the ack");

    assert!(client.on_ack(&mut link, now, &start_id, AckOutcome::Accepted));
    assert_eq!(
        events(&mut client),
        [RenderEvent::Accepted(playback.clone())]
    );
    let frames = link.frames();
    assert_eq!(frames.len(), 13);
    // Every frame follows the acknowledged start in the send order.
    assert!(matches!(link.sent[0], crate::sim::Sent::Command { .. }));
    for (index, frame) in frames.iter().enumerate() {
        let packet = Packet::parse(Bytes::clone(&frame.bytes)).unwrap();
        let header = packet.header();
        assert_eq!(header.kind, Kind::Render);
        assert_eq!(header.epoch, epoch);
        assert_eq!(header.generation, 1);
        assert_eq!(header.sequence, index as u64);
        assert_eq!(header.first_sample, index as u64 * 3_840);
        assert_eq!(frame.first_frame, header.first_sample);
        let Content::Pcm(samples) = packet.content() else {
            panic!("render frames carry PCM");
        };
        let expected = tone(48_000);
        let first = index * 3_840;
        assert!(
            samples.eq(expected[first..first + frame.frames as usize]
                .iter()
                .copied())
        );
    }
}

#[test]
fn end_frame_is_the_frame_after_the_last_one_sent() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = running(&mut client, &mut link, now);

    client
        .push_audio(&mut link, now, &playback, &tone(6_000))
        .unwrap();
    // One whole packet goes; the rest waits for more audio or the end.
    assert_eq!(link.frames().len(), 1);
    client
        .push_audio(&mut link, now, &playback, &tone(4_000))
        .unwrap();
    assert_eq!(link.frames().len(), 2);
    client.finish_audio(&mut link, now, &playback).unwrap();

    let frames = link.frames();
    let sizes: Vec<u32> = frames.iter().map(|f| f.frames).collect();
    assert_eq!(sizes, [3_840, 3_840, 2_320]);
    let (_, command, generation) = link.last_command();
    let Command::RenderEnd(end) = command else {
        panic!("render.end last");
    };
    assert_eq!(end.end_frame, 10_000);
    assert_eq!(generation, playback.generation());
    assert_eq!(
        Command::RenderEnd(end).body_json()["end_frame"],
        json!("10000")
    );
    assert_eq!(
        client.push_audio(&mut link, now, &playback, &tone(10)),
        Err(AudioRefused::SourceEnded)
    );

    // The device drains it.
    let id = playback.id().as_str().to_owned();
    assert!(client.on_report(&mut link, now, 1, finished(&id, "drained", 10_000)));
    let ended = events(&mut client).pop().unwrap();
    assert_eq!(
        ended,
        RenderEvent::Ended {
            playback: playback.clone(),
            end: PlaybackEnd::Finished {
                reason: Reported::Known(FinishReason::Drained),
                last_completed_frame: 10_000,
            },
        }
    );
    assert_eq!(
        client.cancel(&mut link, &playback, CancelReason::Stopped),
        Err(NotLive),
        "an ended playback is no longer live"
    );
}

#[test]
fn an_empty_source_ends_at_frame_zero_without_frames() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = running(&mut client, &mut link, now);
    client.finish_audio(&mut link, now, &playback).unwrap();
    assert!(link.frames().is_empty());
    let (_, Command::RenderEnd(end), _) = link.last_command() else {
        panic!("render.end");
    };
    assert_eq!(end.end_frame, 0);
}

#[test]
fn flow_control_never_runs_more_than_258304_frames_ahead_of_progress() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = running(&mut client, &mut link, now);
    let id = playback.id().as_str().to_owned();
    let total: u64 = 20 * 48_000;
    let mut fed = 0u64;
    let mut completed = 0u64;
    let handed = |link: &SimLink| link.frames().last().map_or(0, |f| f.frame_range().end);

    while completed < total {
        // The runtime feeds the decoder's chunks while the client wants them.
        while client.wants_audio(&playback) && fed < total {
            let chunk = 3_840.min(total - fed) as usize;
            client
                .push_audio(&mut link, now, &playback, &tone(chunk))
                .unwrap();
            fed += chunk as u64;
            if fed == total {
                client.finish_audio(&mut link, now, &playback).unwrap();
            }
        }
        let ahead = handed(&link) - completed;
        assert!(ahead <= MAX_FRAMES_AHEAD, "{ahead} frames ahead");
        if fed < total {
            // With audio waiting, the window is filled to within a packet.
            assert!(
                ahead + PACKET_FRAMES as u64 > MAX_FRAMES_AHEAD,
                "window not filled: {ahead}"
            );
        }
        // The device plays one 80 ms report's worth, a sink write at a time.
        completed = (completed + 3 * 2_048).min(handed(&link));
        link.write_all();
        assert!(client.on_report(&mut link, now, 1, progress(&id, "progress", completed)));
    }
    assert_eq!(handed(&link), total);
    let (_, Command::RenderEnd(end), _) = link.last_command() else {
        panic!("render.end");
    };
    assert_eq!(end.end_frame, total);
    // The buffer of decoded audio never exceeded the prefetch bound plus one
    // chunk: the client stopped wanting audio while the window was full.
    assert!(!client.wants_audio(&playback));
}

#[test]
fn a_full_link_queue_is_retried_at_the_next_deadline() {
    let now = Instant::now();
    let mut link = SimLink::new();
    link.capacity = 2;
    let mut client = client();
    let playback = running(&mut client, &mut link, now);
    client
        .push_audio(&mut link, now, &playback, &tone(4 * 3_840))
        .unwrap();
    assert_eq!(link.frames().len(), 2);
    assert_eq!(client.next_deadline(), Some(now + FRAME_RETRY));

    link.write_all();
    client.poll(&mut link, now + FRAME_RETRY);
    let firsts: Vec<u64> = link.frames().iter().map(|f| f.first_frame).collect();
    assert_eq!(
        firsts,
        [0, 3_840, 7_680, 11_520],
        "nothing lost or reordered"
    );
    assert_eq!(client.next_deadline(), None);
}

#[test]
fn cancel_withdraws_every_handed_frame_and_sends_render_cancel() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = running(&mut client, &mut link, now);
    let id = playback.id().as_str().to_owned();
    client
        .push_audio(&mut link, now, &playback, &tone(5 * 3_840 + 100))
        .unwrap();
    assert_eq!(link.frames().len(), 5);
    assert!(client.on_report(&mut link, now, 1, progress(&id, "progress", 2_048)));
    events(&mut client);

    client
        .cancel(&mut link, &playback, CancelReason::Interrupted)
        .unwrap();
    // The link's writer drops a frame whose playback is cancelled.
    assert!(
        link.frames().iter().all(|f| f.playback.is_cancelled()),
        "every handed frame is withdrawn"
    );
    let (_, command, generation) = link.last_command();
    assert_eq!(generation, 1);
    assert_eq!(command.body_json()["reason"], "interrupted");
    assert!(matches!(command, Command::RenderCancel(_)));
    assert_eq!(
        events(&mut client),
        [RenderEvent::Cancelled {
            playback: playback.clone(),
            reason: CancelReason::Interrupted,
        }]
    );
    // Nothing more goes out for it, and audio is refused.
    assert_eq!(
        client.push_audio(&mut link, now, &playback, &tone(10_000)),
        Err(AudioRefused::NotAccepting)
    );
    assert_eq!(link.frames().len(), 5);
    // Cancelling again sends nothing.
    let sent = link.sent.len();
    client
        .cancel(&mut link, &playback, CancelReason::Stopped)
        .unwrap();
    assert_eq!(link.sent.len(), sent);

    assert!(client.on_report(&mut link, now, 1, finished(&id, "cancelled", 2_048)));
    assert!(matches!(
        events(&mut client).as_slice(),
        [RenderEvent::Ended {
            end: PlaybackEnd::Finished {
                reason: Reported::Known(FinishReason::Cancelled),
                ..
            },
            ..
        }]
    ));
}

#[test]
fn generations_grow_per_class_and_a_new_start_cancels_the_old_playback_first() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let first = running(&mut client, &mut link, now);
    let earcon = client
        .start_local(
            &mut link,
            &LocalSource::Earcon(Earcon::WakeChime),
            0.0,
            false,
        )
        .unwrap();
    assert_eq!(earcon.generation(), 1, "classes count separately");
    client
        .push_audio(&mut link, now, &first, &tone(3_840))
        .unwrap();
    events(&mut client);

    let second = client.start_network(&mut link, content()).unwrap();
    assert_eq!(second.generation(), 2);
    let commands = link.commands();
    let tail: Vec<(&str, u32)> = commands[commands.len() - 2..]
        .iter()
        .map(|(_, c, g)| (c.message_type().as_str(), *g))
        .collect();
    assert_eq!(tail, [("render.cancel", 1), ("render.start", 2)]);
    assert!(matches!(
        events(&mut client).as_slice(),
        [RenderEvent::Cancelled {
            reason: CancelReason::Superseded,
            ..
        }]
    ));

    // The old playback's reports still name it; a report naming its id at
    // another generation is stale and ignored.
    let old = first.id().as_str().to_owned();
    assert!(!client.on_report(&mut link, now, 2, progress(&old, "progress", 10)));
    assert!(client.on_report(&mut link, now, 1, finished(&old, "cancelled", 0)));
    assert!(matches!(
        events(&mut client).as_slice(),
        [RenderEvent::Ended { playback, .. }] if *playback == first
    ));
}

#[test]
fn a_stale_generation_rejection_ends_the_playback_without_frames() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client.start_network(&mut link, content()).unwrap();
    let (start, _, _) = link.last_command();
    client
        .push_audio(&mut link, now, &playback, &tone(10_000))
        .unwrap();
    assert!(client.on_ack(&mut link, now, &start, rejected("stale_generation")));
    assert_eq!(
        events(&mut client),
        [RenderEvent::Ended {
            playback: playback.clone(),
            end: PlaybackEnd::Rejected(Some(Reported::Known(RenderRejection::StaleGeneration))),
        }]
    );
    assert!(link.frames().is_empty());
    assert!(!client.wants_audio(&playback));
    // Later reports about it are not this client's.
    let id = playback.id().as_str().to_owned();
    assert!(!client.on_report(&mut link, now, 1, progress(&id, "progress", 0)));
    // The next playback of the class moves to a newer generation.
    let next = client.start_network(&mut link, content()).unwrap();
    assert_eq!(next.generation(), 2);
}

#[test]
fn an_unacknowledged_start_is_cancelled_and_ends_unconfirmed() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client.start_network(&mut link, content()).unwrap();
    let (start, _, _) = link.last_command();
    assert!(client.on_ack(
        &mut link,
        now,
        &start,
        AckOutcome::Failed(AckFailure::TimedOut)
    ));
    assert!(matches!(link.last_command().1, Command::RenderCancel(_)));
    assert!(matches!(
        events(&mut client).as_slice(),
        [
            RenderEvent::Cancelled {
                reason: CancelReason::Unconfirmed,
                ..
            },
            RenderEvent::Ended {
                end: PlaybackEnd::Unconfirmed,
                ..
            }
        ]
    ));
    assert!(!client.wants_audio(&playback));
}

#[test]
fn earcons_are_gated_by_the_device_capabilities() {
    let mut link = SimLink::new();
    let mut bare = client();
    for (earcon, capability) in [
        (Earcon::NoInputChime, Capability::NoInputChime),
        (Earcon::AnnouncementChime, Capability::AnnouncementChime),
    ] {
        assert_eq!(
            bare.start_local(&mut link, &LocalSource::Earcon(earcon), 0.0, false),
            Err(LocalStartError::Unsupported(capability))
        );
    }
    assert!(
        link.sent.is_empty(),
        "nothing sent for an unsupported earcon"
    );
    bare.start_local(
        &mut link,
        &LocalSource::Earcon(Earcon::WakeChime),
        0.0,
        false,
    )
    .unwrap();
    let body = link.last_command().1.body_json();
    assert_eq!(body["local_asset"], "builtin:wake_chime");
    assert_eq!(body["epoch"], json!(null));
    assert_eq!(body["source_class"], "earcon");

    let caps = Capabilities::from_names(["no_input_chime", "announcement_chime"]);
    let mut capable = RenderClient::new(&caps);
    capable
        .start_local(
            &mut link,
            &LocalSource::Earcon(Earcon::AnnouncementChime),
            0.0,
            true,
        )
        .unwrap();
    let body = link.last_command().1.body_json();
    assert_eq!(body["local_asset"], "builtin:announcement_chime");
    assert_eq!(body["announcement"], true);
    capable
        .start_local(
            &mut link,
            &LocalSource::Earcon(Earcon::NoInputChime),
            0.0,
            false,
        )
        .unwrap();

    let hash = echomuse_protocol::Sha256::of(b"alarm");
    bare.start_local(
        &mut link,
        &LocalSource::AlertPreview(echomuse_protocol::alerts::AlertSound::Asset(hash.clone())),
        0.0,
        false,
    )
    .unwrap();
    let body = link.last_command().1.body_json();
    assert_eq!(body["source_class"], "alert_preview");
    assert_eq!(body["local_asset"], hash.as_str());
}

#[test]
fn a_local_playback_ends_with_the_device_report() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let chime = client
        .start_local(
            &mut link,
            &LocalSource::Earcon(Earcon::WakeChime),
            0.0,
            false,
        )
        .unwrap();
    let (start, _, _) = link.last_command();
    assert!(client.on_ack(&mut link, now, &start, AckOutcome::Accepted));
    assert!(!client.wants_audio(&chime));
    let id = chime.id().as_str().to_owned();
    assert!(client.on_report(&mut link, now, 1, finished(&id, "drained", 9_600)));
    assert!(matches!(
        events(&mut client).as_slice(),
        [
            RenderEvent::Accepted(_),
            RenderEvent::Ended {
                end: PlaybackEnd::Finished { .. },
                ..
            }
        ]
    ));
}

#[test]
fn session_end_ends_every_playback() {
    let now = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = running(&mut client, &mut link, now);
    events(&mut client);
    client.session_ended();
    assert_eq!(
        events(&mut client),
        [RenderEvent::Ended {
            playback,
            end: PlaybackEnd::LinkClosed
        }]
    );
    link.closed = true;
    assert_eq!(client.start_network(&mut link, content()), Err(LinkClosed));
}

fn limit_events(client: &mut RenderClient) -> Vec<ResponseLimit> {
    events(client)
        .into_iter()
        .filter_map(|event| match event {
            RenderEvent::LimitExceeded { limit, .. } => Some(limit),
            _ => None,
        })
        .collect()
}

/// Sleeps to the client's next deadline under paused time and polls.
async fn run_to_deadline(client: &mut RenderClient, link: &mut SimLink) -> Instant {
    let deadline = client.next_deadline().expect("a deadline");
    time::sleep_until(deadline).await;
    let now = Instant::now();
    client.poll(link, now);
    now
}

#[tokio::test(start_paused = true)]
async fn answer_audio_must_start_within_ten_seconds() {
    let ready = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client
        .start_network(
            &mut link,
            dialog(Some(ResponseLimits::from_answer_ready(ready))),
        )
        .unwrap();
    let (start, _, _) = link.last_command();
    assert!(client.on_ack(&mut link, ready, &start, AckOutcome::Accepted));
    time::advance(Duration::from_secs(9)).await;
    client.poll(&mut link, Instant::now());
    assert!(limit_events(&mut client).is_empty());

    let fired = run_to_deadline(&mut client, &mut link).await;
    assert_eq!(fired - ready, ANSWER_START_LIMIT);
    let all = events(&mut client);
    assert_eq!(
        all[0],
        RenderEvent::LimitExceeded {
            playback: playback.clone(),
            limit: ResponseLimit::Start
        }
    );
    assert!(matches!(
        all[1],
        RenderEvent::Cancelled {
            reason: CancelReason::ResponseTimeout,
            ..
        }
    ));
    assert_eq!(
        link.last_command().1.body_json()["reason"],
        "response_timeout"
    );
    // The limit fires once.
    assert_eq!(client.next_deadline(), None);
}

#[tokio::test(start_paused = true)]
async fn a_started_answer_may_not_stall_for_two_seconds() {
    let ready = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client
        .start_network(
            &mut link,
            dialog(Some(ResponseLimits::from_answer_ready(ready))),
        )
        .unwrap();
    let (start, _, _) = link.last_command();
    assert!(client.on_ack(&mut link, ready, &start, AckOutcome::Accepted));
    let id = playback.id().as_str().to_owned();
    time::advance(Duration::from_secs(1)).await;
    assert!(client.on_report(&mut link, Instant::now(), 1, progress(&id, "start", 0)));
    // Progress advances for a second, then stops while reports keep coming.
    let mut completed = 0;
    for _ in 0..12 {
        time::advance(Duration::from_millis(80)).await;
        completed += 3_840;
        client.on_report(
            &mut link,
            Instant::now(),
            1,
            progress(&id, "progress", completed),
        );
    }
    let last_advance = Instant::now();
    for _ in 0..20 {
        time::advance(Duration::from_millis(80)).await;
        client.on_report(
            &mut link,
            Instant::now(),
            1,
            progress(&id, "progress", completed),
        );
        client.poll(&mut link, Instant::now());
    }
    assert!(
        limit_events(&mut client).is_empty(),
        "1.6 s without progress is allowed"
    );
    let fired = run_to_deadline(&mut client, &mut link).await;
    assert_eq!(fired - last_advance, STALL_LIMIT);
    assert_eq!(limit_events(&mut client), [ResponseLimit::Stall]);
}

#[tokio::test(start_paused = true)]
async fn one_spoken_answer_plays_at_most_120_seconds() {
    let ready = Instant::now();
    let mut link = SimLink::new();
    let mut client = client();
    let playback = client
        .start_network(
            &mut link,
            dialog(Some(ResponseLimits::from_answer_ready(ready))),
        )
        .unwrap();
    let (start, _, _) = link.last_command();
    assert!(client.on_ack(&mut link, ready, &start, AckOutcome::Accepted));
    let id = playback.id().as_str().to_owned();
    let started = Instant::now();
    assert!(client.on_report(&mut link, started, 1, progress(&id, "start", 0)));
    let mut completed = 0;
    while limit_events(&mut client).is_empty() {
        time::advance(Duration::from_millis(80)).await;
        completed += 3_840;
        client.on_report(
            &mut link,
            Instant::now(),
            1,
            progress(&id, "progress", completed),
        );
        client.poll(&mut link, Instant::now());
        assert!(Instant::now() - started <= ANSWER_LENGTH_LIMIT);
    }
    assert_eq!(Instant::now() - started, ANSWER_LENGTH_LIMIT);
}

#[tokio::test(start_paused = true)]
async fn content_has_no_response_limits() {
    let mut link = SimLink::new();
    let mut client = client();
    running(&mut client, &mut link, Instant::now());
    assert_eq!(client.next_deadline(), None);
    time::advance(Duration::from_secs(600)).await;
    client.poll(&mut link, Instant::now());
    assert!(limit_events(&mut client).is_empty());
}
