use echomuse_esphome::entities::{DeviceVolume, Playback};
use echomuse_esphome::voice::MediaRequest;
use echomuse_protocol::Capabilities;

use super::*;

type Player = MediaPlayer<u32>;

fn volume(step: u8) -> DeviceVolume {
    DeviceVolume::new(step).unwrap()
}

fn player() -> Player {
    MediaPlayer::new(&Capabilities::default(), Some(volume(100)))
}

fn effects(player: &mut Player) -> Vec<MediaEffect<u32>> {
    std::iter::from_fn(|| player.next_effect()).collect()
}

fn play(url: &str) -> MediaRequest {
    MediaRequest::PlayUrl {
        url: url.to_owned(),
        announcement: false,
    }
}

fn state(playback: Playback) -> MediaEffect<u32> {
    MediaEffect::ReportState(PlayerState {
        playback,
        volume: Some(volume(100)),
        muted: false,
    })
}

fn started(effects: &[MediaEffect<u32>]) -> Vec<(StreamId, u64)> {
    effects
        .iter()
        .filter_map(|e| match e {
            MediaEffect::StartContent {
                stream,
                skip_frames,
                ..
            } => Some((*stream, *skip_frames)),
            _ => None,
        })
        .collect()
}

fn stopped(effects: &[MediaEffect<u32>]) -> Vec<StreamId> {
    effects
        .iter()
        .filter_map(|e| match e {
            MediaEffect::StopContent { stream } => Some(*stream),
            _ => None,
        })
        .collect()
}

/// Plays `url` and returns its stream, with the decode opened.
fn playing(player: &mut Player, url: &str, seekable: Seekability) -> StreamId {
    assert_eq!(player.command(play(url)), CommandOutcome::Applied);
    let all = effects(player);
    let [(stream, 0)] = started(&all)[..] else {
        panic!("one start from the beginning: {all:?}");
    };
    player.content_opened(stream, seekable);
    stream
}

fn stopped_at(frames: u64) -> ContentEnd {
    ContentEnd {
        reason: ContentEndReason::Stopped,
        completed_frames: Some(frames),
    }
}

#[test]
fn play_pause_and_resume_from_the_bookmark() {
    let mut player = player();
    assert_eq!(
        player.command(play("http://ha/song.flac")),
        CommandOutcome::Applied
    );
    let all = effects(&mut player);
    assert_eq!(
        all,
        [
            MediaEffect::StartContent {
                stream: StreamId(1),
                url: "http://ha/song.flac".into(),
                skip_frames: 0
            },
            state(Playback::Playing),
        ]
    );
    let stream = StreamId(1);
    player.content_opened(stream, Seekability::Seekable);
    player.content_progress(stream, 96_000);

    assert_eq!(player.command(MediaRequest::Pause), CommandOutcome::Applied);
    assert_eq!(
        effects(&mut player),
        [MediaEffect::StopContent { stream }, state(Playback::Paused)]
    );
    // The device's end carries the exact bookmark.
    player.content_ended(stream, stopped_at(100_000));
    assert_eq!(effects(&mut player), []);

    assert_eq!(player.command(MediaRequest::Play), CommandOutcome::Applied);
    let all = effects(&mut player);
    assert_eq!(started(&all), [(stream, 100_000)]);
    assert!(all.contains(&state(Playback::Playing)));

    // A second pause adds to the bookmark.
    player.command(MediaRequest::Pause);
    player.content_ended(stream, stopped_at(48_000));
    effects(&mut player);
    player.command(MediaRequest::Play);
    assert_eq!(started(&effects(&mut player)), [(stream, 148_000)]);
}

#[test]
fn a_live_stream_resumes_at_its_live_edge() {
    let mut player = player();
    let stream = playing(&mut player, "http://radio/live", Seekability::Live);
    player.content_progress(stream, 480_000);
    player.command(MediaRequest::Pause);
    player.content_ended(stream, stopped_at(480_000));
    effects(&mut player);
    player.command(MediaRequest::Play);
    assert_eq!(started(&effects(&mut player)), [(stream, 0)]);
}

#[test]
fn resume_before_the_bookmark_arrives_waits_for_it() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/song.flac", Seekability::Seekable);
    player.command(MediaRequest::Pause);
    effects(&mut player);
    assert_eq!(player.command(MediaRequest::Play), CommandOutcome::Applied);
    assert!(
        started(&effects(&mut player)).is_empty(),
        "no start without the bookmark"
    );
    player.content_ended(stream, stopped_at(30_000));
    assert_eq!(started(&effects(&mut player)), [(stream, 30_000)]);
}

#[test]
fn commands_during_a_dialog_are_deferred_but_reported_at_once() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/song.flac", Seekability::Seekable);
    effects(&mut player);

    player.dialog_started();
    assert_eq!(
        player.command(MediaRequest::Pause),
        CommandOutcome::Deferred
    );
    // Home Assistant sees paused at once; the music keeps playing, ducked.
    assert_eq!(effects(&mut player), [state(Playback::Paused)]);
    assert_eq!(player.state().playback, Playback::Paused);

    // Volume is not deferred.
    assert_eq!(
        player.command(MediaRequest::SetVolume(volume(60))),
        CommandOutcome::Applied
    );
    assert!(effects(&mut player).contains(&MediaEffect::SetDeviceVolume(volume(60))));

    player.dialog_ended();
    assert_eq!(effects(&mut player), [MediaEffect::StopContent { stream }]);
    player.content_ended(stream, stopped_at(10_000));
    assert_eq!(effects(&mut player), []);
}

#[test]
fn deferred_commands_net_out_when_the_dialog_ends() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/song.flac", Seekability::Seekable);
    effects(&mut player);
    player.dialog_started();
    assert_eq!(
        player.command(MediaRequest::Pause),
        CommandOutcome::Deferred
    );
    assert_eq!(player.command(MediaRequest::Play), CommandOutcome::Deferred);
    player.dialog_ended();
    // Paused then resumed: nothing to do, the stream never stopped.
    let all = effects(&mut player);
    assert!(
        stopped(&all).is_empty() && started(&all).is_empty(),
        "{all:?}"
    );
    assert_eq!(player.state().playback, Playback::Playing);
    let _ = stream;
}

#[test]
fn a_stop_during_a_dialog_is_not_undone_by_its_end() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/song.flac", Seekability::Seekable);
    effects(&mut player);
    player.dialog_started();
    assert_eq!(player.command(MediaRequest::Stop), CommandOutcome::Deferred);
    assert_eq!(effects(&mut player), [state(Playback::Idle)]);
    player.dialog_ended();
    assert_eq!(effects(&mut player), [MediaEffect::StopContent { stream }]);
    // Its late end changes nothing, and there is nothing to resume.
    player.content_ended(stream, stopped_at(5_000));
    assert_eq!(effects(&mut player), []);
    assert_eq!(
        player.command(MediaRequest::Play),
        CommandOutcome::Unavailable(Unavailable::NothingToResume)
    );
}

#[test]
fn a_new_url_during_a_dialog_replaces_the_stream_when_it_ends() {
    let mut player = player();
    let old = playing(&mut player, "http://ha/a.flac", Seekability::Seekable);
    effects(&mut player);
    player.dialog_started();
    assert_eq!(
        player.command(play("http://ha/b.flac")),
        CommandOutcome::Deferred
    );
    assert!(effects(&mut player).is_empty(), "already reported playing");
    player.dialog_ended();
    let all = effects(&mut player);
    assert_eq!(stopped(&all), [old]);
    let [(new, 0)] = started(&all)[..] else {
        panic!("{all:?}");
    };
    assert_ne!(new, old);
    // The old stream's end does not touch the new one.
    player.content_ended(old, stopped_at(1));
    assert_eq!(effects(&mut player), []);
    assert_eq!(player.state().playback, Playback::Playing);
}

#[test]
fn a_stream_that_ends_goes_idle() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/a.flac", Seekability::Seekable);
    effects(&mut player);
    player.content_ended(
        stream,
        ContentEnd {
            reason: ContentEndReason::Drained,
            completed_frames: Some(1_000),
        },
    );
    assert_eq!(effects(&mut player), [state(Playback::Idle)]);
    assert_eq!(
        player.command(MediaRequest::Pause),
        CommandOutcome::Unavailable(Unavailable::NothingPlaying)
    );
}

#[test]
fn losing_the_device_pauses_at_the_last_known_position() {
    let mut player = player();
    let stream = playing(&mut player, "http://ha/a.flac", Seekability::Seekable);
    player.content_progress(stream, 20_000);
    effects(&mut player);
    player.content_ended(
        stream,
        ContentEnd {
            reason: ContentEndReason::Interrupted,
            completed_frames: None,
        },
    );
    assert_eq!(effects(&mut player), [state(Playback::Paused)]);
    player.command(MediaRequest::Play);
    assert_eq!(started(&effects(&mut player)), [(stream, 20_000)]);
}

#[test]
fn announcements_queue_behind_replies_and_each_other() {
    let caps = Capabilities::from_names(["announcement_chime"]);
    let mut player: Player = MediaPlayer::new(&caps, Some(volume(100)));
    let stream = playing(&mut player, "http://ha/a.flac", Seekability::Seekable);
    effects(&mut player);

    player.dialog_started();
    let first = player.announce(SatelliteAnnouncement {
        id: 7,
        media_url: "http://ha/tts/1.mp3".into(),
        text: Some("Dinner".into()),
        preannounce_url: Some("http://ha/chime.mp3".into()),
        start_conversation: false,
    });
    let second = player.announce(SatelliteAnnouncement {
        id: 8,
        media_url: "http://ha/tts/2.mp3".into(),
        text: None,
        preannounce_url: None,
        start_conversation: true,
    });
    assert!(
        effects(&mut player).is_empty(),
        "the reply is still playing"
    );
    // A media command meanwhile is deferred behind both.
    assert_eq!(
        player.command(MediaRequest::Pause),
        CommandOutcome::Deferred
    );
    effects(&mut player);

    player.dialog_ended();
    let all = effects(&mut player);
    assert_eq!(
        all,
        [
            MediaEffect::StartAnnouncement(AnnouncementPlan {
                key: first,
                media_url: "http://ha/tts/1.mp3".into(),
                opener: Opener::Chime,
                text: Some("Dinner".into()),
                start_conversation: false,
                satellite: Some(7),
            }),
            state(Playback::Announcing),
        ]
    );
    // The second waits for the first; ending the wrong one does nothing.
    player.announcement_ended(second, true);
    assert!(effects(&mut player).is_empty());
    player.announcement_ended(first, true);
    let all = effects(&mut player);
    assert_eq!(
        all[0],
        MediaEffect::FinishAnnouncement {
            id: 7,
            success: true
        }
    );
    assert!(matches!(
        &all[1],
        MediaEffect::StartAnnouncement(AnnouncementPlan { key, opener: Opener::None, start_conversation: true, .. }) if *key == second
    ));
    assert!(
        stopped(&all).is_empty(),
        "the pause waits for the announcements"
    );

    player.announcement_ended(second, false);
    let all = effects(&mut player);
    assert_eq!(
        all[0],
        MediaEffect::FinishAnnouncement {
            id: 8,
            success: false
        }
    );
    assert_eq!(stopped(&all), [stream], "then the deferred pause applies");
    assert!(all.contains(&state(Playback::Paused)));
}

#[test]
fn announcement_openers_follow_the_device_capability() {
    let mut player = player();
    player.announce(SatelliteAnnouncement {
        id: 1,
        media_url: "http://ha/tts.mp3".into(),
        text: None,
        preannounce_url: Some("http://ha/chime.mp3".into()),
        start_conversation: false,
    });
    let all = effects(&mut player);
    assert!(matches!(
        &all[0],
        MediaEffect::StartAnnouncement(AnnouncementPlan { opener: Opener::Sound(url), .. }) if url == "http://ha/chime.mp3"
    ));
    // A media player announcement on a device without the chime has none.
    let mut player = player_without_volume();
    assert_eq!(
        player.command(MediaRequest::PlayUrl {
            url: "http://ha/tts.mp3".into(),
            announcement: true
        }),
        CommandOutcome::Applied
    );
    assert!(matches!(
        &effects(&mut player)[0],
        MediaEffect::StartAnnouncement(AnnouncementPlan {
            opener: Opener::None,
            satellite: None,
            ..
        })
    ));
}

fn player_without_volume() -> Player {
    MediaPlayer::new(&Capabilities::default(), None)
}

#[test]
fn volume_and_mute() {
    let mut player = player();
    assert_eq!(player.command(MediaRequest::Mute), CommandOutcome::Applied);
    assert_eq!(
        effects(&mut player)[0],
        MediaEffect::SetDeviceVolume(volume(0))
    );
    assert!(player.state().muted);
    assert_eq!(player.state().volume, Some(volume(100)));
    assert_eq!(
        player.command(MediaRequest::Unmute),
        CommandOutcome::Applied
    );
    assert_eq!(
        effects(&mut player)[0],
        MediaEffect::SetDeviceVolume(volume(100))
    );
    assert!(!player.state().muted);

    // The device's own buttons win.
    player.command(MediaRequest::Mute);
    effects(&mut player);
    player.device_volume(Some(volume(40)));
    assert_eq!(
        effects(&mut player),
        [MediaEffect::ReportState(PlayerState {
            playback: Playback::Idle,
            volume: Some(volume(40)),
            muted: false
        })]
    );

    let mut fixed = player_without_volume();
    assert_eq!(
        fixed.state().media_state(),
        None,
        "unknown volume is not reported as 0"
    );
    for request in [MediaRequest::Mute, MediaRequest::SetVolume(volume(3))] {
        assert_eq!(
            fixed.command(request),
            CommandOutcome::Unavailable(Unavailable::NoVolumeControl)
        );
    }
}

#[test]
fn session_loss_fails_pending_announcements() {
    let mut player = player();
    player.dialog_started();
    player.announce(SatelliteAnnouncement {
        id: 3,
        media_url: "u".into(),
        text: None,
        preannounce_url: None,
        start_conversation: false,
    });
    effects(&mut player);
    player.session_lost();
    assert_eq!(
        effects(&mut player),
        [MediaEffect::FinishAnnouncement {
            id: 3,
            success: false
        }]
    );
}
