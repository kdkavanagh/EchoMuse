use std::fs::{File, FileTimes};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::{Duration, SystemTime};

use echomuse_settings::values::{AlarmSound, TimerSound};
use tempfile::TempDir;

use super::*;
use crate::sounds::export::tests::tone;
use crate::sounds::export::{ASSET_RATE, wav_bytes};
use crate::sounds::service::{ServiceError, SoundService};

const FFMPEG: &str = "/usr/bin/ffmpeg";
const NO_FFMPEG: &str = "/nonexistent/ffmpeg";

fn id(text: &str) -> SoundId {
    SoundId::new(text).unwrap()
}

/// A PCM16 WAV upload at any rate and channel count.
fn upload_wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

fn stereo_tone(seconds: f64) -> Vec<u8> {
    upload_wav(44_100, 2, &tone(44_100, 2, seconds, 880.0))
}

fn catalog(dir: &TempDir, ffmpeg: &str) -> SoundCatalog {
    SoundCatalog::open(
        SoundsDir::at(dir.path().join("sounds")),
        PathBuf::from(ffmpeg),
    )
    .unwrap()
}

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn an_uploaded_tone_is_exported_as_a_48_khz_asset_named_by_its_hash() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    let summary = catalog
        .upload(id("chime"), UploadFormat::Wav, &stereo_tone(2.0), &never())
        .unwrap();
    let ExportState::Ready {
        sha256,
        seconds,
        shortened,
    } = summary.state.clone()
    else {
        panic!("{summary:?}");
    };
    assert_eq!(seconds, 2.0);
    assert!(!shortened);

    let dir = catalog.dir().clone();
    let asset_path = dir.asset_path(&sha256);
    assert_eq!(
        asset_path,
        dir.root().join("assets").join(format!("{sha256}.wav"))
    );
    let asset = fs::read(&asset_path).unwrap();
    assert_eq!(Sha256::of(&asset), sha256, "the name is the file's hash");
    // 48 kHz, mono, 16-bit, and 2 s long.
    assert_eq!(&asset[20..36], &wav_bytes(&[])[20..36]);
    assert_eq!(asset.len(), 44 + 2 * 96_000);
    assert_eq!(
        u32::from_le_bytes(asset[24..28].try_into().unwrap()),
        ASSET_RATE
    );
    assert_eq!(mode(&asset_path), 0o600);

    let upload_path = dir.upload_path(&id("chime"), UploadFormat::Wav);
    assert_eq!(mode(&upload_path), 0o644);
    let record_path = dir.root().join("chime.alert.json");
    assert_eq!(mode(&record_path), 0o600);
    let record = ExportRecord::parse(&fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record.stamp, upload_stamp(&upload_path).unwrap());
    assert!(record.stamp.starts_with("chime.wav:"));
    assert_eq!(record.sha256.as_ref(), Some(&sha256));
    assert_eq!(record.seconds, Some(2.0));
    assert_eq!(record.error, None);

    // Nothing else is left behind: no scratch or temporary files.
    let mut names: Vec<String> = fs::read_dir(dir.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["assets", "chime.alert.json", "chime.wav"]);

    let snapshot = catalog.snapshot();
    assert_eq!(
        snapshot.resolve(&SoundChoice::Named("chime".into())),
        Resolved {
            sound: AlertSound::Asset(sha256),
            unresolved: false
        }
    );
}

#[test]
fn a_long_upload_is_shortened_to_ten_seconds() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    let summary = catalog
        .upload(id("long"), UploadFormat::Wav, &stereo_tone(12.0), &never())
        .unwrap();
    assert_eq!(summary.seconds(), Some(10.0));
    assert!(summary.shortened());
    let asset = fs::read(catalog.dir().asset_path(summary.sha256().unwrap())).unwrap();
    assert_eq!(asset.len(), 44 + 2 * 480_000);
}

#[test]
fn an_upload_that_is_not_audio_is_refused_and_not_kept() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    let refused = catalog
        .upload(
            id("junk"),
            UploadFormat::Mp3,
            b"this is not audio at all",
            &never(),
        )
        .unwrap_err();
    assert!(
        matches!(refused, UploadError::Export(ExportError::Decode(_))),
        "{refused:?}"
    );
    assert!(catalog.snapshot().get(&id("junk")).is_none());
    assert!(
        !catalog
            .dir()
            .upload_path(&id("junk"), UploadFormat::Mp3)
            .exists()
    );
    assert!(!catalog.dir().root().join("junk.alert.json").exists());
    assert_eq!(
        catalog.upload(id("empty"), UploadFormat::Wav, b"", &never()),
        Err(UploadError::Size(0))
    );
    let huge = vec![0u8; MAX_UPLOAD_BYTES + 1];
    assert_eq!(
        catalog.upload(id("huge"), UploadFormat::Wav, &huge, &never()),
        Err(UploadError::Size(MAX_UPLOAD_BYTES + 1))
    );
}

#[test]
fn an_existing_asset_is_never_rewritten() {
    let first = TempDir::new().unwrap();
    let upload = stereo_tone(1.0);
    let made = catalog(&first, FFMPEG)
        .upload(id("a"), UploadFormat::Wav, &upload, &never())
        .unwrap();
    let sha = made.sha256().unwrap().clone();
    let bytes = fs::read(SoundsDir::at(first.path().join("sounds")).asset_path(&sha)).unwrap();

    // Another installation already holds that asset, from an earlier export.
    let second = TempDir::new().unwrap();
    let dir = SoundsDir::at(second.path().join("sounds"));
    dir.create().unwrap();
    let asset_path = dir.asset_path(&sha);
    fs::write(&asset_path, &bytes).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    File::options()
        .write(true)
        .open(&asset_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old))
        .unwrap();
    let before = fs::metadata(&asset_path).unwrap();

    let mut catalog = catalog(&second, FFMPEG);
    let again = catalog
        .upload(id("b"), UploadFormat::Wav, &upload, &never())
        .unwrap();
    assert_eq!(again.sha256(), Some(&sha));
    let after = fs::metadata(&asset_path).unwrap();
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.modified().unwrap(), old);
    assert_eq!(
        after.permissions().mode() & 0o777,
        before.permissions().mode() & 0o777
    );
}

/// The layout of a real installation, with synthetic content: an upload with its export
/// record, the older decode cache beside it, and assets whose uploads were deleted.
#[test]
fn a_catalog_shaped_like_a_real_one_is_read_without_exporting() {
    let tmp = TempDir::new().unwrap();
    let dir = SoundsDir::at(tmp.path().join("sounds"));
    dir.create().unwrap();
    let name = "system_alerts_24_legacy_short";
    let upload = upload_wav(48_000, 2, &tone(48_000, 2, 2.0, 660.0));
    let upload_path = dir.root().join(format!("{name}.wav"));
    fs::write(&upload_path, &upload).unwrap();
    let pcm = wav_bytes(&tone(48_000, 1, 2.0, 660.0))[44..].to_vec();
    fs::write(dir.root().join(format!("{name}.pcm")), &pcm).unwrap();
    fs::write(
        dir.root().join(format!("{name}.pcm.stamp")),
        "1789940065:384132",
    )
    .unwrap();
    let asset = crate::sounds::export::finish(tone(48_000, 1, 2.0, 660.0)).unwrap();
    fs::write(dir.asset_path(&asset.sha256), &asset.wav).unwrap();
    let stamp = upload_stamp(&upload_path).unwrap();
    let record = format!(
        r#"{{"stamp": "{stamp}", "sha256": "{}", "seconds": 2.0, "shortened": false, "error": null}}"#,
        asset.sha256
    );
    fs::write(dir.root().join(format!("{name}.alert.json")), &record).unwrap();
    let orphan = Sha256::of(b"an alarm made from a deleted upload");
    fs::write(dir.asset_path(&orphan), b"RIFF").unwrap();
    // A failed export is read back as failed, not retried.
    let broken = dir.root().join("broken.ogg");
    fs::write(&broken, b"ogg?").unwrap();
    let failed = ExportRecord {
        stamp: upload_stamp(&broken).unwrap(),
        sha256: None,
        seconds: None,
        shortened: false,
        error: Some("ffmpeg could not decode the sound: bad header".into()),
    };
    fs::write(dir.root().join("broken.alert.json"), failed.to_text()).unwrap();

    // With no ffmpeg at all, everything current is read from the records.
    let catalog = SoundCatalog::open(dir.clone(), PathBuf::from(NO_FFMPEG)).unwrap();
    assert_eq!(catalog.pending(), Vec::<SoundId>::new());
    let snapshot = catalog.snapshot();
    let ids: Vec<&str> = snapshot.sounds().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["broken", name]);
    let sound = snapshot.get(&id(name)).unwrap();
    assert_eq!(sound.sha256(), Some(&asset.sha256));
    assert_eq!(sound.seconds(), Some(2.0));
    assert!(!sound.shortened());
    assert_eq!(
        snapshot.get(&id("broken")).unwrap().state,
        ExportState::Failed("ffmpeg could not decode the sound: bad header".into())
    );
    assert_eq!(
        snapshot.resolve(&SoundChoice::Named("broken".into())),
        Resolved {
            sound: AlertSound::Fallback,
            unresolved: true
        }
    );
    assert_eq!(
        fs::read(dir.root().join(format!("{name}.pcm"))).unwrap(),
        pcm
    );
    assert!(dir.asset_path(&orphan).is_file());
    assert_eq!(
        fs::read_to_string(dir.root().join(format!("{name}.alert.json"))).unwrap(),
        record
    );
}

#[test]
fn a_stale_record_or_a_missing_asset_waits_for_an_export() {
    let tmp = TempDir::new().unwrap();
    let mut made = catalog(&tmp, FFMPEG);
    let first = made
        .upload(id("bell"), UploadFormat::Wav, &stereo_tone(1.0), &never())
        .unwrap();
    let dir = made.dir().clone();
    let record_path = dir.root().join("bell.alert.json");
    let record = fs::read_to_string(&record_path).unwrap();

    // The asset went missing: pending, and an export that cannot run changes nothing.
    fs::remove_file(dir.asset_path(first.sha256().unwrap())).unwrap();
    let mut reopened = SoundCatalog::open(dir.clone(), PathBuf::from(NO_FFMPEG)).unwrap();
    assert_eq!(reopened.pending(), [id("bell")]);
    let error = reopened.export(&id("bell"), &never()).unwrap_err();
    assert!(matches!(error, ExportError::Spawn(_)), "{error:?}");
    assert_eq!(reopened.pending(), [id("bell")]);
    assert_eq!(fs::read_to_string(&record_path).unwrap(), record);

    // With ffmpeg the same asset comes back.
    let mut fixed = SoundCatalog::open(dir.clone(), PathBuf::from(FFMPEG)).unwrap();
    fixed.export(&id("bell"), &never()).unwrap();
    assert_eq!(fixed.snapshot().get(&id("bell")).unwrap(), &first);
    assert!(dir.asset_path(first.sha256().unwrap()).is_file());

    // The upload changed: its stamp no longer matches.
    let path = dir.upload_path(&id("bell"), UploadFormat::Wav);
    fs::write(&path, stereo_tone(1.5)).unwrap();
    let mut changed = SoundCatalog::open(dir.clone(), PathBuf::from(FFMPEG)).unwrap();
    assert_eq!(changed.pending(), [id("bell")]);
    changed.export(&id("bell"), &never()).unwrap();
    assert_eq!(
        changed.snapshot().get(&id("bell")).unwrap().seconds(),
        Some(1.5)
    );
    // Both assets stay.
    assert!(dir.asset_path(first.sha256().unwrap()).is_file());
}

#[test]
fn deleting_keeps_the_asset_and_refuses_a_selected_sound() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    let made = catalog
        .upload(id("gong"), UploadFormat::Wav, &stereo_tone(1.0), &never())
        .unwrap();
    let dir = catalog.dir().clone();
    fs::write(dir.root().join("gong.pcm"), b"old cache").unwrap();
    fs::write(dir.root().join("gong.pcm.stamp"), b"1:2").unwrap();

    let selected = [SoundChoice::Default, SoundChoice::Named("gong".into())];
    assert_eq!(
        catalog.delete(&id("gong"), &selected),
        Err(DeleteError::InUse)
    );
    assert_eq!(catalog.delete(&id("nope"), &[]), Err(DeleteError::NotFound));
    catalog
        .delete(&id("gong"), &[SoundChoice::Named("other".into())])
        .unwrap();
    for gone in ["gong.wav", "gong.alert.json", "gong.pcm", "gong.pcm.stamp"] {
        assert!(!dir.root().join(gone).exists(), "{gone}");
    }
    assert!(dir.asset_path(made.sha256().unwrap()).is_file());
    assert!(catalog.snapshot().get(&id("gong")).is_none());
}

#[test]
fn a_new_upload_replaces_the_old_one_of_that_name() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    catalog
        .upload(id("door"), UploadFormat::Wav, &stereo_tone(1.0), &never())
        .unwrap();
    // The same sound as FLAC, encoded by ffmpeg from the WAV.
    let dir = catalog.dir().clone();
    let flac = tmp.path().join("door.flac");
    let status = std::process::Command::new(FFMPEG)
        .args(["-loglevel", "error", "-y", "-i"])
        .arg(dir.upload_path(&id("door"), UploadFormat::Wav))
        .arg(&flac)
        .status()
        .unwrap();
    assert!(status.success());
    let replaced = catalog
        .upload(
            id("door"),
            UploadFormat::Flac,
            &fs::read(&flac).unwrap(),
            &never(),
        )
        .unwrap();
    assert_eq!(replaced.seconds(), Some(1.0));
    assert!(!dir.upload_path(&id("door"), UploadFormat::Wav).exists());
    assert!(dir.upload_path(&id("door"), UploadFormat::Flac).is_file());
    let record =
        ExportRecord::parse(&fs::read_to_string(dir.root().join("door.alert.json")).unwrap())
            .unwrap();
    assert!(record.stamp.starts_with("door.flac:"));
}

#[test]
fn choices_resolve_to_assets_or_the_fallback() {
    let tmp = TempDir::new().unwrap();
    let mut catalog = catalog(&tmp, FFMPEG);
    let timers = TimerSettings {
        timer_sound: TimerSound(SoundChoice::Named("pasta".into())),
        alarm_sound: AlarmSound(SoundChoice::Default),
        ..TimerSettings::default()
    };
    let fallback = |unresolved| Resolved {
        sound: AlertSound::Fallback,
        unresolved,
    };
    assert_eq!(
        catalog.snapshot().endpoint_sounds(&timers),
        EndpointSounds {
            alarm: fallback(false),
            timer: fallback(true),
        }
    );
    let pasta = catalog
        .upload(id("pasta"), UploadFormat::Wav, &stereo_tone(0.5), &never())
        .unwrap();
    let default = catalog
        .upload(
            id("default"),
            UploadFormat::Wav,
            &stereo_tone(0.25),
            &never(),
        )
        .unwrap();
    let asset = |s: &SoundSummary| Resolved {
        sound: AlertSound::Asset(s.sha256().unwrap().clone()),
        unresolved: false,
    };
    assert_eq!(
        catalog.snapshot().endpoint_sounds(&timers),
        EndpointSounds {
            alarm: asset(&default),
            timer: asset(&pasta),
        }
    );
    // A stored id that is not a valid catalog id cannot resolve either.
    assert_eq!(
        catalog.snapshot().resolve(&SoundChoice::Named(".x".into())),
        fallback(true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_service_exports_on_its_own_thread_and_publishes_snapshots() {
    let tmp = TempDir::new().unwrap();
    let dir = SoundsDir::at(tmp.path().join("sounds"));
    dir.create().unwrap();
    // An upload left from before, never exported.
    fs::write(dir.root().join("waiting.wav"), stereo_tone(0.5)).unwrap();

    let service = SoundService::start(dir.clone(), PathBuf::from(FFMPEG)).unwrap();
    let handle = service.handle();
    let mut updates = handle.subscribe();
    let exported = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let ready = updates
                .borrow_and_update()
                .get(&id("waiting"))
                .and_then(SoundSummary::seconds);
            if let Some(seconds) = ready {
                return seconds;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(exported, 0.5);

    let summary = handle
        .upload(id("bell"), UploadFormat::Wav, stereo_tone(1.0))
        .await
        .unwrap();
    // The answer comes after the catalog that holds it is published.
    assert_eq!(handle.snapshot().get(&id("bell")), Some(&summary));
    assert!(handle.dir().asset_path(summary.sha256().unwrap()).is_file());
    assert_eq!(
        handle
            .delete(id("bell"), vec![SoundChoice::Named("bell".into())])
            .await,
        Err(ServiceError::Refused(DeleteError::InUse))
    );
    handle.delete(id("bell"), Vec::new()).await.unwrap();
    assert!(handle.snapshot().get(&id("bell")).is_none());

    drop(service);
    assert_eq!(
        handle.delete(id("waiting"), Vec::new()).await,
        Err(ServiceError::Stopped)
    );
}
