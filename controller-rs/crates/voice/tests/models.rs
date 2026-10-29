//! The speech workers over the real pinned models.
//!
//! These need downloaded assets, so they are `#[ignore]`d and read their locations from the
//! same environment variables as `crates/speech/tests/models.rs`. A test run with `--ignored`
//! but without a variable fails, naming it:
//!
//! | Variable | What it points at |
//! |---|---|
//! | `ECHOMUSE_ORT_LIB` | `libonnxruntime.so.1.20.1` from the official 1.20.1 release |
//! | `ECHOMUSE_KROKO_DIR` | the speech bundle's `kroko/` directory; the bundle's own `speech_bundle.json` sits beside it and pins the files |
//! | `ECHOMUSE_SILERO_VAD` | `silero_vad_v5.onnx` |
//! | `ECHOMUSE_DTLN_DIR` | a directory holding DTLN's `model_1.onnx` and `model_2.onnx` |
//! | `ECHOMUSE_WAKE_WAV` | optional: a saved wake clip (16 kHz mono, canonical level). Without it the wake case is skipped, and says so |
//!
//! The Kroko test WAV is 24 kHz at a normal recording level. The workers take canonical
//! 16 kHz mic, whose speech sits about 20 dB below that, so the tests resample it to 16 kHz
//! and divide it by 10: the +20 dB evidence copy then gives Kroko the recording's own level
//! back (to within int16 rounding).

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use echomuse_core::types::SessionId;
use echomuse_echo::EchoPlan;
use echomuse_speech::pins::{DTLN_MODEL_1, DTLN_MODEL_2, KROKO_TEST_WAV_SHA256};
use echomuse_speech::{
    DtlnModelFiles, KrokoModelFiles, LinearResampler, MonoWav, NativeLibraries, PinnedFile,
    Sha256Digest, SpeechAssets, SpeechBundleManifest,
};
use echomuse_turn::{AsrKind, CellIndex, Epoch, Sample, VadInput, VerifierResult};
use echomuse_uplink::SubscriptionId;
use echomuse_voice::speech::mic::MicAnalysis;
use echomuse_voice::speech::models::{BundleModels, ModelSet};
use echomuse_voice::speech::span::SpanDecoding;
use echomuse_voice::speech::verify::WakeVerification;
use echomuse_voice::speech::{MicJob, Pcm16Le, SpanJob, SpanOutput, StreamKey, SttGain, VerifyJob};

const PHRASE: &str = "ask not what your country";

fn asset(var: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!("this test needs {var}; see the table at the top of tests/models.rs"),
    }
}

fn assets() -> SpeechAssets {
    let kroko = asset("ECHOMUSE_KROKO_DIR");
    let manifest = SpeechBundleManifest::load(&kroko.join("../speech_bundle.json")).unwrap();
    let pin = |name: &str, d: Sha256Digest| PinnedFile::new(kroko.join(name), d);
    let dtln = asset("ECHOMUSE_DTLN_DIR");
    SpeechAssets {
        native: NativeLibraries::pinned(asset("ECHOMUSE_ORT_LIB"), None).unwrap(),
        kroko: KrokoModelFiles {
            encoder: pin("encoder.onnx", manifest.asr.files.encoder.sha256),
            decoder: pin("decoder.onnx", manifest.asr.files.decoder.sha256),
            joiner: pin("joiner.onnx", manifest.asr.files.joiner.sha256),
            tokens: pin("tokens.txt", manifest.asr.files.tokens.sha256),
        },
        kroko_test_wav: pin("test_wavs/0.wav", KROKO_TEST_WAV_SHA256),
        vad: PinnedFile::new(asset("ECHOMUSE_SILERO_VAD"), manifest.vad.sha256),
        dtln: DtlnModelFiles {
            model_1: PinnedFile::new(dtln.join("model_1.onnx"), DTLN_MODEL_1.sha256),
            model_2: PinnedFile::new(dtln.join("model_2.onnx"), DTLN_MODEL_2.sha256),
        },
    }
}

fn models() -> Arc<dyn ModelSet> {
    Arc::new(BundleModels::load(&assets()).unwrap())
}

/// The Kroko test WAV as canonical 16 kHz mic (see the module documentation).
fn test_wav_canonical() -> Vec<i16> {
    let wav = MonoWav::read(&asset("ECHOMUSE_KROKO_DIR").join("test_wavs/0.wav")).unwrap();
    let mut resampler = LinearResampler::to_kroko_rate(wav.sample_rate).unwrap();
    let audio = resampler.resample(&wav.to_f32(), true).unwrap();
    audio
        .iter()
        .map(|&v| (f64::from(v) * 32_768.0 / 10.0).round() as i16)
        .collect()
}

/// Lower-case words only, so punctuation and casing do not matter.
fn plain(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn pcm16(samples: &[i16]) -> Pcm16Le {
    Pcm16Le::new(Bytes::from(
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
}

#[test]
#[ignore = "needs the speech bundle; see the table at the top"]
fn mic_analysis_streams_the_test_wav() {
    let epoch = Epoch::new(1).unwrap();
    let vad_from = CellIndex(10);
    let start = vad_from.start().0;
    let key = StreamKey {
        session: SessionId(1),
        subscription: SubscriptionId::generate(),
        epoch,
    };
    let mut worker = MicAnalysis::new(models());
    let opened = worker.run(MicJob::Open {
        key: key.clone(),
        plan: EchoPlan {
            epoch,
            cells_from: vad_from,
            vad_from,
        },
    });
    assert!(opened.failure.is_none());
    worker.run(MicJob::StartUtterance {
        key: key.clone(),
        from: Sample(start),
    });

    let mut pcm = test_wav_canonical();
    pcm.extend(std::iter::repeat_n(0, 32_000));
    let mut vad = Vec::new();
    let mut observations = Vec::new();
    for (i, chunk) in pcm.chunks(1_280).enumerate() {
        let first = start + (i * 1_280) as u64;
        let out = worker.run(MicJob::Mic {
            key: key.clone(),
            first: Sample(first),
            mono_ns: first * 62_500,
            discontinuity: false,
            pcm: pcm16(chunk),
        });
        assert!(out.failure.is_none(), "{:?}", out.failure);
        if let Some((cell, v)) = out.vad {
            assert_eq!(cell.0, vad_from.0 + vad.len() as u64);
            vad.extend(v);
        }
        observations.extend(out.asr);
    }

    assert_eq!(vad.len(), pcm.len() / 512);
    let speech = vad
        .iter()
        .filter(|v| matches!(v, VadInput::Probability(p) if p.get() >= 0.5))
        .count();
    assert_eq!(observations.len(), pcm.len() / 1_280);
    let last = observations.last().unwrap();
    let text: String = last.tokens.iter().map(|t| t.text.as_str()).collect();
    let finalized: Vec<_> = observations
        .iter()
        .filter(|o| o.kind == AsrKind::Finalized)
        .map(|o| o.through.0 - start)
        .collect();
    println!(
        "VAD: {} cells, {speech} speech-positive; ASR: {} blocks, finalized at {finalized:?} samples; text {text:?}",
        vad.len(),
        observations.len()
    );
    assert!(speech > 50);
    assert!(plain(&text).contains(PHRASE), "{text:?}");
    assert!(!finalized.is_empty());
}

#[test]
#[ignore = "needs the speech bundle; see the table at the top"]
fn wake_verification_decodes_the_test_wav() {
    let mut worker = WakeVerification::new(models());
    let out = worker.run(VerifyJob {
        candidate: "c1".into(),
        pcm: test_wav_canonical().into(),
    });
    let VerifierResult::Text(text) = out.result else {
        panic!("the decode failed");
    };
    println!("verification text: {text:?}");
    assert!(plain(&text).contains(PHRASE), "{text:?}");
}

#[test]
#[ignore = "needs the speech bundle; see the table at the top"]
fn span_decoding_redecodes_and_makes_a_denoised_stt_copy() {
    let mut worker = SpanDecoding::new(models());
    let pcm: Arc<[i16]> = test_wav_canonical().into();
    let turn = uuid::Uuid::new_v4();

    let SpanOutput::Redecode { result, .. } = worker.run(SpanJob::Redecode {
        turn,
        pcm: pcm.clone(),
    }) else {
        panic!("asked for a re-decode");
    };
    let r = result.unwrap();
    println!("re-decode: {:?}", r.text);
    assert!(plain(&r.text).contains(PHRASE), "{:?}", r.text);

    let SpanOutput::SttCopy { result, .. } = worker.run(SpanJob::SttCopy {
        turn,
        pcm: pcm.clone(),
        gain: SttGain::FromWake(pcm[..16_000].into()),
        denoise: true,
    }) else {
        panic!("asked for an STT copy");
    };
    let copy = result.unwrap();
    let rms =
        |x: &[i16]| (x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
    println!(
        "STT copy: gain {:.2} dB, denoised {}, rms {:.0} from {:.0}",
        copy.gain_db,
        copy.denoised,
        rms(&copy.pcm[..pcm.len()]),
        rms(&pcm)
    );
    assert!(copy.denoised);
    assert!((10.0..=30.0).contains(&copy.gain_db));
    assert_eq!(copy.pcm.len(), pcm.len() + 8_000);
    assert!(copy.pcm[pcm.len()..].iter().all(|&s| s == 0));
    assert!(rms(&copy.pcm[..pcm.len()]) > rms(&pcm));
}

#[test]
#[ignore = "needs the speech bundle; see the table at the top"]
fn a_saved_wake_clip_passes_verification() {
    let Some(path) = std::env::var_os("ECHOMUSE_WAKE_WAV").filter(|v| !v.is_empty()) else {
        println!("ECHOMUSE_WAKE_WAV is not set: the saved wake clip case is skipped");
        return;
    };
    let wav = MonoWav::read(&PathBuf::from(path)).unwrap();
    assert_eq!(wav.sample_rate, 16_000, "wake clips are 16 kHz");
    let mut worker = WakeVerification::new(models());
    let out = worker.run(VerifyJob {
        candidate: "c1".into(),
        pcm: wav.samples.into(),
    });
    let VerifierResult::Text(text) = out.result else {
        panic!("the decode failed");
    };
    println!("wake clip text: {text:?}");
    assert!(
        echomuse_turn::text::verification_passes(&text, "ophel"),
        "{text:?}"
    );
}
