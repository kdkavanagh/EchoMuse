//! Model acceptance tests (§7.3 of the Rust controller evaluation) against the real pinned
//! models and native libraries.
//!
//! These need downloaded assets, so they are `#[ignore]`d and read their locations from
//! environment variables. A test run with `--ignored` but without a variable fails, naming it:
//!
//! | Variable | What it points at |
//! |---|---|
//! | `ECHOMUSE_ORT_LIB` | `libonnxruntime.so.1.20.1` from the official 1.20.1 release ([`pins::ORT_X86_64`]) |
//! | `ECHOMUSE_KROKO_DIR` | the extracted `sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06` archive |
//! | `ECHOMUSE_SILERO_VAD` | `silero_vad_v5.onnx` |
//! | `ECHOMUSE_DTLN_DIR` | a directory holding DTLN's `model_1.onnx` and `model_2.onnx` |
//!
//! The BCResNet graph and sidecar are the repository-root files, and its reference logits are
//! the firmware's `device/internal/wakeword/bcresnet/testdata/deployed_fixture.json`.
//!
//! `fixtures/kroko_test_wav_steps.jsonl` holds sherpa-onnx's own 1.13.8 Python package (the
//! wheel `speech_bundle.json` pins) decoding the Kroko test WAV: `OnlineRecognizer.from_transducer`
//! with greedy search, 16 kHz, 80 features, one thread; the 24 kHz file as float32
//! `int16 / 32768` followed by 1.5 s of zeros, fed in 80 ms (1,920-sample) blocks at 24 kHz;
//! after each block `decode_stream` while `is_ready`, then one line of
//! `get_result_as_json_string`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use echomuse_speech::bcresnet::{WAKE_WINDOW_SAMPLES, prepare_window};
use echomuse_speech::dtln::{DTLN_BLOCK_LEN, DTLN_BLOCK_SHIFT, DTLN_LATENCY_SAMPLES};
use echomuse_speech::pins::{
    self, DEPLOYED_WAKE_GRAPH_SHA256, DEPLOYED_WAKE_SIDECAR_SHA256, DTLN_MODEL_1, DTLN_MODEL_2,
    KROKO_TEST_TRANSCRIPT, KROKO_TEST_WAV_SHA256,
};
use echomuse_speech::qualify::{
    self, FailureKind, ModelUnderTest, QualificationEvidence, QualificationRejection,
    QualificationRequest, QualifierCommand,
};
use echomuse_speech::silero::SILERO_CELL_SAMPLES;
use echomuse_speech::{
    BcresnetModelFiles, BcresnetScorer, DtlnDenoiser, DtlnModelFiles, KrokoModelFiles,
    KrokoRecognizer, LinearResampler, MonoWav, NativeLibraries, OrtRuntime, PinnedFile,
    Sha256Digest, SherpaRuntime, SileroVad, SpeechBundleManifest, SpeechError, WindowScore,
};

fn asset(var: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!("this test needs {var}; see the table at the top of tests/models.rs"),
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn native() -> NativeLibraries {
    NativeLibraries::pinned(asset("ECHOMUSE_ORT_LIB")).unwrap()
}

fn ort() -> OrtRuntime {
    OrtRuntime::load(&native().onnxruntime).unwrap()
}

fn manifest() -> SpeechBundleManifest {
    SpeechBundleManifest::load(&repo().join("controller/speech_bundle.json")).unwrap()
}

/// The Kroko files as extracted from the archive, pinned by the bundle manifest.
fn kroko_files() -> KrokoModelFiles {
    let dir = asset("ECHOMUSE_KROKO_DIR");
    let m = manifest();
    let pin = |name: &str, d: Sha256Digest| PinnedFile::new(dir.join(name), d);
    KrokoModelFiles {
        encoder: pin("encoder.onnx", m.asr.files.encoder.sha256),
        decoder: pin("decoder.onnx", m.asr.files.decoder.sha256),
        joiner: pin("joiner.onnx", m.asr.files.joiner.sha256),
        tokens: pin("tokens.txt", m.asr.files.tokens.sha256),
    }
}

fn kroko_test_wav() -> PinnedFile {
    PinnedFile::new(
        asset("ECHOMUSE_KROKO_DIR").join("test_wavs/0.wav"),
        KROKO_TEST_WAV_SHA256,
    )
}

fn kroko() -> KrokoRecognizer {
    let sherpa = SherpaRuntime::verify(&native().sherpa).unwrap();
    KrokoRecognizer::load(&sherpa, &kroko_files()).unwrap()
}

fn silero_file() -> PinnedFile {
    PinnedFile::new(asset("ECHOMUSE_SILERO_VAD"), manifest().vad.sha256)
}

fn dtln_files() -> DtlnModelFiles {
    let dir = asset("ECHOMUSE_DTLN_DIR");
    DtlnModelFiles {
        model_1: PinnedFile::new(dir.join("model_1.onnx"), DTLN_MODEL_1.sha256),
        model_2: PinnedFile::new(dir.join("model_2.onnx"), DTLN_MODEL_2.sha256),
    }
}

fn wake_files() -> BcresnetModelFiles {
    BcresnetModelFiles {
        graph: PinnedFile::new(
            repo().join("bcresnet_audio.onnx"),
            DEPLOYED_WAKE_GRAPH_SHA256,
        ),
        sidecar: PinnedFile::new(
            repo().join("bcresnet_audio.json"),
            DEPLOYED_WAKE_SIDECAR_SHA256,
        ),
    }
}

/// The test WAV at 16 kHz, resampled in 80 ms pieces as sherpa-onnx does internally.
fn speech_16k() -> Vec<f32> {
    let wav = MonoWav::read(&kroko_test_wav().path).unwrap();
    assert_eq!(wav.sample_rate, 24_000);
    let mut r = LinearResampler::to_kroko_rate(wav.sample_rate).unwrap();
    let mut out = Vec::new();
    for chunk in wav.to_f32().chunks(1920) {
        out.extend(r.resample(chunk, false).unwrap());
    }
    out
}

/// Feeds `audio` in 80 ms blocks, decoding after each, and returns the JSON after each block.
fn decode_steps(recognizer: &KrokoRecognizer, audio: &[f32]) -> Vec<String> {
    let mut stream = recognizer.create_stream().unwrap();
    audio
        .chunks(1280)
        .map(|block| {
            stream.accept(block).unwrap();
            stream.decode_ready();
            stream.result_json().unwrap()
        })
        .collect()
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB and ECHOMUSE_KROKO_DIR"]
fn kroko_matches_upstream_python_step_by_step() {
    let recognizer = kroko();
    let wav = MonoWav::read(&kroko_test_wav().path).unwrap();
    kroko_test_wav().verify().unwrap();
    let rate = wav.sample_rate as usize;
    let mut audio = wav.to_f32();
    let speech_len = audio.len();
    // The qualification protocol: the WAV then 4 s of zeros, at 24 kHz, in 80 ms blocks. Its
    // first 1.5 s of zeros are the reference fixture's.
    audio.extend(std::iter::repeat_n(0.0, rate * 4));
    let mut resampler = LinearResampler::to_kroko_rate(wav.sample_rate).unwrap();
    let mut stream = recognizer.create_stream().unwrap();
    let mut ours = Vec::new();
    let mut events = Vec::new();
    let block = rate * 80 / 1000;
    for (i, piece) in audio.chunks(block).enumerate() {
        stream
            .accept(&resampler.resample(piece, false).unwrap())
            .unwrap();
        let ran = stream.decode_ready();
        let json = stream.result_json().unwrap();
        if ran > 0 && (i + 1) * block > speech_len {
            let r = echomuse_speech::AsrResult::parse(&json).unwrap();
            events.push(((i + 1) as f64 * 0.08, f64::from(r.num_trailing_blanks)));
        }
        ours.push(json);
    }
    let reference = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kroko_test_wav_steps.jsonl"),
    )
    .unwrap();
    let reference: Vec<&str> = reference.lines().collect();
    assert_eq!(reference.len(), 67, "fixture step count");
    for (i, (a, b)) in ours.iter().zip(&reference).enumerate() {
        assert_eq!(a, b, "step {i} differs from sherpa-onnx's Python package");
    }
    let last = echomuse_speech::AsrResult::parse(ours.last().unwrap()).unwrap();
    assert_eq!(last.text.trim(), KROKO_TEST_TRANSCRIPT);
    assert_eq!(last.tokens.len(), last.timestamps.len());
    let blank_rate = qualify::trailing_blank_rate(&events).unwrap();
    println!(
        "trailing blanks: {blank_rate:.2} per second from {} decode events; {} at the end",
        events.len(),
        last.num_trailing_blanks
    );
    assert!(
        qualify::TRAILING_BLANK_RATE.contains(&blank_rate),
        "{blank_rate} blanks/s"
    );
    // 4 s of silence is 100 blank frames; the last partial encoder chunk is not decoded yet.
    assert!(
        (64..=100).contains(&last.num_trailing_blanks),
        "{}",
        last.num_trailing_blanks
    );
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB and ECHOMUSE_KROKO_DIR"]
fn kroko_independent_streams_across_threads_match_sequential() {
    let recognizer = kroko();
    let speech = speech_16k();
    let mut reversed = speech.clone();
    reversed.reverse();
    let mut twice = speech.clone();
    twice.extend_from_slice(&speech);
    let inputs: Vec<Vec<f32>> = vec![
        speech.clone(),
        speech.iter().map(|x| x * 0.3).collect(),
        reversed,
        twice,
        speech.iter().map(|x| (x * 4.0).clamp(-1.0, 1.0)).collect(),
        speech,
    ];
    let inputs: Vec<Vec<f32>> = inputs
        .into_iter()
        .map(|mut a| {
            a.extend(std::iter::repeat_n(0.0, 24_000));
            a
        })
        .collect();

    let sequential: Vec<Vec<String>> = inputs
        .iter()
        .map(|a| decode_steps(&recognizer, a))
        .collect();
    assert_eq!(
        sequential[0], sequential[5],
        "the same audio gave different results"
    );
    assert_ne!(sequential[0], sequential[1]);

    // Streams keep the recognizer alive: the threads get clones and this handle is dropped.
    let barrier = Arc::new(Barrier::new(inputs.len()));
    let handles: Vec<_> = inputs
        .into_iter()
        .map(|audio| {
            let recognizer = recognizer.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut stream = recognizer.create_stream().unwrap();
                drop(recognizer);
                barrier.wait();
                audio
                    .chunks(1280)
                    .map(|block| {
                        stream.accept(block).unwrap();
                        stream.decode_ready();
                        stream.result_json().unwrap()
                    })
                    .collect::<Vec<String>>()
            })
        })
        .collect();
    drop(recognizer);
    for (i, (h, want)) in handles.into_iter().zip(&sequential).enumerate() {
        let got = h.join().unwrap();
        assert_eq!(
            &got, want,
            "stream {i} decoded concurrently differs from sequential"
        );
    }
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB and ECHOMUSE_KROKO_DIR"]
fn kroko_stream_rejects_bad_input_and_resets() {
    let recognizer = kroko();
    let mut stream = recognizer.create_stream().unwrap();
    assert!(matches!(
        stream.accept(&[0.0, f32::NAN]),
        Err(SpeechError::InvalidInput(_))
    ));
    stream.accept(&speech_16k()).unwrap();
    assert!(stream.decode_ready() > 0);
    assert!(!stream.result().unwrap().text.is_empty());
    stream.reset();
    let after = stream.result().unwrap();
    assert!(after.text.is_empty(), "reset kept text {:?}", after.text);
    // The stream keeps working after a reset: more speech gives text again.
    stream.accept(&speech_16k()).unwrap();
    stream.input_finished();
    stream.decode_ready();
    assert!(
        !stream.result().unwrap().text.is_empty(),
        "no text after reset"
    );
}

/// One window of the firmware fixture, regenerated from its recipe.
#[derive(serde::Deserialize)]
struct FixtureWindow {
    name: String,
    seed: u64,
    modulus: u64,
    offset: i64,
    loud_from: usize,
    loud_modulus: u64,
    loud_offset: i64,
    scored: bool,
    #[serde(default)]
    probe: Vec<f64>,
    #[serde(default)]
    sum: Option<f64>,
    #[serde(default)]
    logits: Vec<f64>,
    #[serde(default)]
    prob: Option<f64>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    graph_sha256: Sha256Digest,
    sidecar_sha256: Sha256Digest,
    onnxruntime: String,
    probe_stride: usize,
    windows: Vec<FixtureWindow>,
}

fn fixture_pcm(w: &FixtureWindow) -> Vec<i16> {
    let mut x = w.seed;
    (0..WAKE_WINDOW_SAMPLES)
        .map(|i| {
            x = (1_103_515_245 * x + 12_345) & 0x7FFF_FFFF;
            let v = if i < w.loud_from {
                ((x >> 8) % w.modulus) as i64 - w.offset
            } else {
                ((x >> 8) % w.loud_modulus) as i64 - w.loud_offset
            };
            i16::try_from(v).unwrap()
        })
        .collect()
}

/// Logits may differ from ONNX Runtime 1.20.1's Python output by at most this much.
const LOGIT_TOLERANCE: f64 = 1e-5;

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB"]
fn bcresnet_matches_the_deployed_fixture() {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(
            repo().join("device/internal/wakeword/bcresnet/testdata/deployed_fixture.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(fixture.graph_sha256, DEPLOYED_WAKE_GRAPH_SHA256);
    assert_eq!(fixture.sidecar_sha256, DEPLOYED_WAKE_SIDECAR_SHA256);
    assert_eq!(fixture.onnxruntime, pins::ORT_VERSION);
    let mut scorer = BcresnetScorer::load(&ort(), &wake_files()).unwrap();
    let mut prepared = vec![0.0f32; WAKE_WINDOW_SAMPLES];
    let mut worst = 0.0f64;
    let mut scored = 0;
    for w in &fixture.windows {
        let pcm = fixture_pcm(w);
        let score = scorer.score(&pcm).unwrap();
        if !w.scored {
            assert_eq!(score, WindowScore::BelowRmsFloor, "{}", w.name);
            continue;
        }
        scored += 1;
        assert!(prepare_window(&mut prepared, &pcm, 0.8).unwrap());
        // The fixture holds float32 values written as JSON doubles; compare as float32 (the
        // JSON parser may be one double ulp off, which rounds to the same float32).
        for (k, &p) in w.probe.iter().enumerate() {
            assert_eq!(
                prepared[k * fixture.probe_stride],
                p as f32,
                "{} probe {k}",
                w.name
            );
        }
        let sum: f64 = prepared.iter().map(|&x| f64::from(x)).sum();
        assert!((sum - w.sum.unwrap()).abs() < 1e-9, "{} sum", w.name);
        let logits = scorer.logits(&prepared).unwrap().to_vec();
        assert_eq!(logits.len(), w.logits.len());
        for (a, b) in logits.iter().zip(&w.logits) {
            worst = worst.max((f64::from(*a) - b).abs());
        }
        let WindowScore::Scored { probability } = score else {
            panic!("{} was not scored", w.name)
        };
        assert!(
            (probability - w.prob.unwrap()).abs() < 1e-6,
            "{} probability",
            w.name
        );
    }
    println!("{scored} scored windows; largest logit difference {worst:e}");
    assert!(scored >= 3);
    assert!(worst <= LOGIT_TOLERANCE, "logits differ by {worst}");
    assert!(matches!(
        scorer.score(&pcm_of_len(100)),
        Err(SpeechError::InvalidInput(_))
    ));
}

fn pcm_of_len(n: usize) -> Vec<i16> {
    vec![100; n]
}

/// The VAD input of the architecture: canonical PCM at +30 dB, saturated, as floats.
fn vad_input() -> Vec<f32> {
    let gain = 10f32.powf(1.5);
    speech_16k()
        .iter()
        .map(|&x| ((x * 32768.0 * gain).clamp(-32768.0, 32767.0)).round() / 32768.0)
        .collect()
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB, ECHOMUSE_SILERO_VAD and ECHOMUSE_KROKO_DIR (test WAV)"]
fn silero_state_save_restore_and_reset_reproduce_probabilities() {
    let rt = ort();
    let mut vad = SileroVad::load(&rt, &silero_file()).unwrap();
    let audio = vad_input();
    let (cells, _) = audio.as_chunks::<SILERO_CELL_SAMPLES>();
    let k = cells.len() / 3;

    let mut first = Vec::new();
    let mut saved = None;
    for (i, c) in cells.iter().enumerate() {
        if i == k {
            saved = Some(vad.state());
        }
        first.push(vad.process_cell(c).unwrap());
    }
    let speech = first.iter().filter(|&&p| p >= 0.5).count();
    println!("{} cells, {speech} at or above 0.5", first.len());
    assert!(speech > 10, "speech was not detected");
    assert!(first.iter().any(|&p| p < 0.35), "silence was not detected");

    vad.restore(&saved.unwrap());
    for (i, c) in cells.iter().enumerate().skip(k) {
        assert_eq!(
            vad.process_cell(c).unwrap().to_bits(),
            first[i].to_bits(),
            "restored cell {i}"
        );
    }
    vad.reset();
    for (i, c) in cells.iter().enumerate() {
        assert_eq!(
            vad.process_cell(c).unwrap().to_bits(),
            first[i].to_bits(),
            "reset cell {i}"
        );
    }
    let mut fresh = SileroVad::load(&rt, &silero_file()).unwrap();
    for (i, c) in cells.iter().enumerate().take(k) {
        assert_eq!(
            fresh.process_cell(c).unwrap().to_bits(),
            first[i].to_bits(),
            "fresh cell {i}"
        );
    }
    // Carrying state matters: cell k from the initial state differs from cell k after k cells.
    fresh.reset();
    let cold = fresh.process_cell(&cells[k]).unwrap();
    assert_ne!(
        cold.to_bits(),
        first[k].to_bits(),
        "state made no difference"
    );

    let state = vad.state();
    // A cell's length is fixed by its type; non-finite samples are rejected without a state
    // change.
    assert!(matches!(
        vad.process_cell(&[f32::NAN; SILERO_CELL_SAMPLES]),
        Err(SpeechError::InvalidInput(_))
    ));
    assert_eq!(vad.state(), state, "a rejected cell changed the state");
}

/// The upstream real-time loop over a whole signal at once, with a direct DFT, as an
/// independent reference for the streaming denoiser.
fn dtln_reference(rt: &OrtRuntime, files: &DtlnModelFiles, audio: &[f32]) -> Vec<f32> {
    use ort::session::Session;
    use ort::value::TensorRef;
    let n = DTLN_BLOCK_LEN;
    let bins = n / 2 + 1;
    let open = |f: &PinnedFile| {
        Session::builder(rt.environment())
            .unwrap()
            .with_intra_threads(1)
            .unwrap()
            .commit_from_file(&f.path)
            .unwrap()
    };
    let mut s1 = open(&files.model_1);
    let mut s2 = open(&files.model_2);
    let (cos, sin): (Vec<f64>, Vec<f64>) = (0..n)
        .map(|i| {
            let a = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            (a.cos(), a.sin())
        })
        .unzip();
    let mut st1 = vec![0f32; 512];
    let mut st2 = vec![0f32; 512];
    let mut inb = vec![0f32; n];
    let mut outb = vec![0f32; n];
    let mut out = Vec::new();
    assert_eq!(audio.len() % DTLN_BLOCK_SHIFT, 0);
    for shift in audio.chunks(DTLN_BLOCK_SHIFT) {
        inb.copy_within(DTLN_BLOCK_SHIFT.., 0);
        inb[n - DTLN_BLOCK_SHIFT..].copy_from_slice(shift);
        let mut re = vec![0f64; bins];
        let mut im = vec![0f64; bins];
        for k in 0..bins {
            for (t, &x) in inb.iter().enumerate() {
                let idx = (k * t) % n;
                re[k] += f64::from(x) * cos[idx];
                im[k] -= f64::from(x) * sin[idx];
            }
        }
        let mag: Vec<f32> = (0..bins).map(|k| re[k].hypot(im[k]) as f32).collect();
        let phase: Vec<f64> = (0..bins).map(|k| im[k].atan2(re[k])).collect();
        let o = s1
            .run(ort::inputs![
                TensorRef::from_array_view(([1usize, 1, bins], &mag[..])).unwrap(),
                TensorRef::from_array_view(([1usize, 2, 128, 2], &st1[..])).unwrap()
            ])
            .unwrap();
        let mask = o[0].try_extract_tensor::<f32>().unwrap().1.to_vec();
        st1 = o[1].try_extract_tensor::<f32>().unwrap().1.to_vec();
        drop(o);
        let (er, ei): (Vec<f64>, Vec<f64>) = (0..bins)
            .map(|k| {
                let a = f64::from(mag[k]) * f64::from(mask[k]);
                (a * phase[k].cos(), a * phase[k].sin())
            })
            .unzip();
        let block: Vec<f32> = (0..n)
            .map(|t| {
                let mut v = er[0] + er[bins - 1] * if t % 2 == 0 { 1.0 } else { -1.0 };
                for k in 1..bins - 1 {
                    let idx = (k * t) % n;
                    v += 2.0 * (er[k] * cos[idx] - ei[k] * sin[idx]);
                }
                (v / n as f64) as f32
            })
            .collect();
        let o = s2
            .run(ort::inputs![
                TensorRef::from_array_view(([1usize, 1, n], &block[..])).unwrap(),
                TensorRef::from_array_view(([1usize, 2, 128, 2], &st2[..])).unwrap()
            ])
            .unwrap();
        let ob = o[0].try_extract_tensor::<f32>().unwrap().1.to_vec();
        st2 = o[1].try_extract_tensor::<f32>().unwrap().1.to_vec();
        outb.copy_within(DTLN_BLOCK_SHIFT.., 0);
        outb[n - DTLN_BLOCK_SHIFT..].fill(0.0);
        for (a, b) in outb.iter_mut().zip(&ob) {
            *a += b;
        }
        out.extend_from_slice(&outb[..DTLN_BLOCK_SHIFT]);
    }
    out
}

/// Largest difference allowed between the streaming denoiser and the reference, on a [-1, 1]
/// scale (about −100 dBFS).
const DTLN_TOLERANCE: f32 = 1e-5;

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB, ECHOMUSE_DTLN_DIR and ECHOMUSE_KROKO_DIR (test WAV)"]
fn dtln_streaming_matches_a_single_shot_reference() {
    let rt = ort();
    let files = dtln_files();
    // Speech plus deterministic noise, padded to whole steps.
    let mut x = 7u64;
    let mut audio: Vec<f32> = speech_16k()
        .iter()
        .map(|&s| {
            x = (1_103_515_245 * x + 12_345) & 0x7FFF_FFFF;
            s + ((x >> 8) % 2001) as f32 / 2000.0 * 0.02 - 0.01
        })
        .collect();
    audio.truncate(audio.len() / DTLN_BLOCK_SHIFT * DTLN_BLOCK_SHIFT);
    let reference = dtln_reference(&rt, &files, &audio);

    let mut ns = DtlnDenoiser::load(&rt, &files).unwrap();
    let sizes = [1usize, 127, 128, 1000, 7, 1280, 333, 64, 4096];
    let mut streamed = Vec::new();
    let mut rest = &audio[..];
    let mut i = 0;
    while !rest.is_empty() {
        let take = sizes[i % sizes.len()].min(rest.len());
        ns.process(&rest[..take], &mut streamed).unwrap();
        rest = &rest[take..];
        i += 1;
    }
    let d = max_diff(&streamed, &reference);
    println!(
        "streaming vs reference: largest difference {d:e} over {} samples",
        audio.len()
    );
    assert!(d <= DTLN_TOLERANCE, "{d}");

    // Reset gives the same output again, fed all at once this time.
    ns.reset();
    let mut again = Vec::new();
    ns.process(&audio, &mut again).unwrap();
    assert_eq!(again, streamed, "chunking or reset changed the output");

    // The STT copy: aligned with the input, same length, and denoising actually happened.
    let span = ns.denoise_span(&audio).unwrap();
    assert_eq!(span.len(), audio.len());
    let d = max_diff(
        &span[..audio.len() - DTLN_LATENCY_SAMPLES],
        &reference[DTLN_LATENCY_SAMPLES..],
    );
    assert!(d <= DTLN_TOLERANCE, "span alignment: {d}");
    let energy = |v: &[f32]| v.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
    let tail = 16_000 * 4 / 10;
    let noise_in = energy(&audio[audio.len() - tail..]);
    let noise_out = energy(&span[span.len() - tail..]);
    println!("trailing noise energy {noise_in:.4} in, {noise_out:.4} out");
    assert!(
        noise_out < noise_in * 0.5,
        "the noise-only tail was not suppressed"
    );
    assert!(matches!(
        ns.process(&[f32::INFINITY], &mut again),
        Err(SpeechError::InvalidInput(_))
    ));
}

// ---- Qualification in a child process ----

const CHILD_ENV: &str = "ECHOMUSE_SPEECH_QUALIFY_CHILD";

/// The child half of the qualification tests. Spawned by the tests below with
/// `ECHOMUSE_SPEECH_QUALIFY_CHILD=1`, it acts as the `qualify-model` subcommand: reads a
/// request on stdin, writes the report line, and exits. Run directly, it checks the child
/// entry rejects an empty request.
#[test]
fn qualification_child() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let code = match qualify::run_child(std::io::stdin().lock(), std::io::stdout().lock()) {
            Ok(()) => 0,
            Err(_) => 2,
        };
        std::process::exit(code);
    }
    let mut out = Vec::new();
    qualify::run_child(&b""[..], &mut out).unwrap();
    let line = String::from_utf8(out).unwrap();
    assert!(
        line.trim_start().starts_with(qualify::REPORT_MARKER),
        "{line}"
    );
    assert!(line.contains("\"request\""), "{line}");
}

fn child() -> QualifierCommand {
    QualifierCommand::new(
        std::env::current_exe().unwrap(),
        [
            "qualification_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(Into::into),
    )
    .with_env(CHILD_ENV, "1")
}

fn qualify_model(
    model: ModelUnderTest,
    timeout: Duration,
) -> Result<qualify::QualifiedModel, QualificationRejection> {
    qualify::qualify_in_child(
        &child(),
        &QualificationRequest {
            native: native(),
            model,
        },
        timeout,
    )
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB, ECHOMUSE_KROKO_DIR, ECHOMUSE_SILERO_VAD and ECHOMUSE_DTLN_DIR"]
fn qualification_accepts_the_pinned_models() {
    let long = Duration::from_secs(120);
    let kroko = qualify_model(
        ModelUnderTest::Kroko {
            files: kroko_files(),
            test_wav: kroko_test_wav(),
            expected_transcript: Some(KROKO_TEST_TRANSCRIPT.into()),
        },
        long,
    )
    .unwrap();
    match &kroko.evidence {
        QualificationEvidence::Kroko {
            transcript,
            trailing_blanks_per_second,
            ..
        } => {
            assert_eq!(transcript, KROKO_TEST_TRANSCRIPT);
            println!("Kroko: {trailing_blanks_per_second:.2} blanks/s");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(kroko.files.len(), 5);
    let wake = qualify_model(
        ModelUnderTest::Bcresnet {
            files: wake_files(),
        },
        long,
    )
    .unwrap();
    assert!(
        matches!(wake.evidence, QualificationEvidence::Bcresnet { ref logits, .. } if logits.len() == 3)
    );
    let vad = qualify_model(
        ModelUnderTest::SileroVad {
            model: silero_file(),
        },
        long,
    )
    .unwrap();
    assert!(
        matches!(vad.evidence, QualificationEvidence::SileroVad { ref probabilities } if probabilities.len() == 16)
    );
    let dtln = qualify_model(
        ModelUnderTest::Dtln {
            files: dtln_files(),
        },
        long,
    )
    .unwrap();
    assert!(
        matches!(dtln.evidence, QualificationEvidence::Dtln { output_rms } if output_rms.is_finite())
    );
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB"]
fn qualification_rejects_wrong_and_corrupt_wake_models() {
    let long = Duration::from_secs(60);
    // A hash that does not match.
    let mut files = wake_files();
    files.graph.sha256 = Sha256Digest::of_bytes(b"something else");
    match qualify_model(ModelUnderTest::Bcresnet { files }, long) {
        Err(QualificationRejection::Rejected(f)) => assert_eq!(f.kind, FailureKind::HashMismatch),
        other => panic!("{other:?}"),
    }
    // Corrupt bytes pinned by their own hash, as an upload would be: the graph does not load.
    let dir = std::env::temp_dir().join(format!("echomuse-speech-qualify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut bytes = std::fs::read(repo().join("bcresnet_audio.onnx")).unwrap();
    bytes.truncate(bytes.len() / 2);
    for b in bytes.iter_mut().skip(16).step_by(7) {
        *b ^= 0x5a;
    }
    let corrupt = dir.join("corrupt.onnx");
    std::fs::write(&corrupt, &bytes).unwrap();
    let mut files = wake_files();
    files.graph = PinnedFile::new(&corrupt, Sha256Digest::of_bytes(&bytes));
    let result = qualify_model(ModelUnderTest::Bcresnet { files }, long);
    std::fs::remove_dir_all(&dir).unwrap();
    match result {
        Err(QualificationRejection::Rejected(f)) => assert_eq!(f.kind, FailureKind::Load, "{f}"),
        other => panic!("{other:?}"),
    }
    // A valid graph with the wrong contract: the Silero model offered as a wake graph.
    if std::env::var_os("ECHOMUSE_SILERO_VAD").is_some() {
        let mut files = wake_files();
        files.graph = silero_file();
        match qualify_model(ModelUnderTest::Bcresnet { files }, long) {
            Err(QualificationRejection::Rejected(f)) => {
                assert_eq!(f.kind, FailureKind::ModelContract, "{f}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
#[ignore = "needs ECHOMUSE_ORT_LIB and ECHOMUSE_KROKO_DIR"]
fn qualification_survives_a_wrong_kroko_file_and_a_timeout() {
    // The decoder offered as the encoder, pinned by its own hash. sherpa-onnx does not validate
    // it: sherpa-onnx 1.13.8 logs "Unknown model type" and then dereferences a null model
    // (SIGSEGV). The parent gets a typed rejection and keeps running.
    let mut files = kroko_files();
    files.encoder = files.decoder.clone();
    let result = qualify_model(
        ModelUnderTest::Kroko {
            files,
            test_wav: kroko_test_wav(),
            expected_transcript: Some(KROKO_TEST_TRANSCRIPT.into()),
        },
        Duration::from_secs(120),
    );
    println!("wrong encoder: {result:?}");
    assert!(
        matches!(
            result,
            Err(QualificationRejection::Crashed {
                signal: libc::SIGSEGV,
                ..
            })
        ),
        "{result:?}"
    );

    let result = qualify_model(
        ModelUnderTest::Kroko {
            files: kroko_files(),
            test_wav: kroko_test_wav(),
            expected_transcript: Some(KROKO_TEST_TRANSCRIPT.into()),
        },
        Duration::from_millis(100),
    );
    assert!(
        matches!(result, Err(QualificationRejection::TimedOut { .. })),
        "{result:?}"
    );
}
