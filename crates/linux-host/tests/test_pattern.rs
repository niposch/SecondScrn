//! The Linux pipeline end to end without a desktop: a test pattern encoded
//! with GStreamer, streamed by the real host engine to a client engine over
//! loopback; the received stream must decode cleanly (checked with ffmpeg
//! when installed).
//!
//! Needs GStreamer with x264 (`gstreamer1.0-plugins-ugly`); skipped
//! otherwise. Uses the software encoder so it works on any machine.
#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secondscrn_linux_host::platform::{DisplaySource, LinuxHostPlatform};
use secondscrn_protocol::{Codec, CodecCapability, DeviceInfo, DisplayRequest, Orientation, StreamConfig};
use secondscrn_security::UnprotectedForTests;
use secondscrn_session::platform::{ClientPlatform, DecodeOutcome, DecoderStats, PlatformError, VideoDecoder};
use secondscrn_session::settings::HostSettings;
use secondscrn_session::snapshot::{ClientPhase, ClientSnapshot, HostSnapshot};
use secondscrn_session::*;
use secondscrn_transport::{InterfacePolicy, ReceivedFrame};

#[derive(Default)]
struct Recorder {
    stream: Arc<Mutex<Vec<u8>>>,
    /// (frames, keyframes)
    frames: Arc<Mutex<(u64, u64)>>,
}

struct RecordingDecoder {
    stream: Arc<Mutex<Vec<u8>>>,
    frames: Arc<Mutex<(u64, u64)>>,
    stats: DecoderStats,
    started: bool,
}

impl VideoDecoder for RecordingDecoder {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError> {
        if !self.started && !frame.keyframe {
            return Ok(DecodeOutcome::NeedKeyframe);
        }
        self.started = true;
        self.stream.lock().unwrap().extend_from_slice(&frame.data);
        let mut f = self.frames.lock().unwrap();
        f.0 += 1;
        f.1 += frame.keyframe as u64;
        self.stats.frames_decoded += 1;
        self.stats.frames_rendered += 1;
        Ok(DecodeOutcome::Ok)
    }
    fn stats(&self) -> DecoderStats {
        self.stats
    }
}

impl ClientPlatform for Recorder {
    fn device_info(&self) -> DeviceInfo {
        DeviceInfo { name: "Test Tablet".into(), model: "Recorder".into(), app_version: "test".into() }
    }
    fn decoders(&self) -> Vec<CodecCapability> {
        vec![CodecCapability {
            codec: Codec::H264,
            implementation: "recorder".into(),
            hardware: true,
            max_width: 4096,
            max_height: 4096,
            max_fps: 120,
        }]
    }
    fn create_decoder(&self, _c: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
        Ok(Box::new(RecordingDecoder {
            stream: self.stream.clone(),
            frames: self.frames.clone(),
            stats: DecoderStats::default(),
            started: false,
        }))
    }
}

async fn wait<T: Clone, F: Fn(&T) -> bool>(rx: &mut tokio::sync::watch::Receiver<T>, secs: u64, what: &str, f: F) -> T {
    match tokio::time::timeout(Duration::from_secs(secs), rx.wait_for(|v| f(v))).await {
        Ok(Ok(v)) => v.clone(),
        _ => panic!("timed out waiting for {what}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pattern_streams_decodable_h264() {
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();
    // SAFETY: set before the engines start other threads.
    unsafe { std::env::set_var("SECONDSCRN_ENCODERS", "x264enc,openh264enc") };
    let hd = tempfile::tempdir().unwrap();
    let cd = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let settings = HostSettings { control_port: port, discoverable: false, ..Default::default() };
    secondscrn_session::settings::save(&hd.path().join("settings.json"), &settings).unwrap();

    let platform = Arc::new(LinuxHostPlatform::new(DisplaySource::TestPattern, None, hd.path().to_path_buf()));
    if secondscrn_session::platform::HostPlatform::encoders(platform.as_ref()).is_empty() {
        eprintln!("skipped: no GStreamer H.264 software encoder installed");
        return;
    }
    let host = HostEngine::start_on(
        tokio::runtime::Handle::current(),
        HostConfig {
            data_dir: hd.path().to_path_buf(),
            default_host_name: "Linux PC".into(),
            listen_addr: [127, 0, 0, 1].into(),
            interfaces: InterfacePolicy::loopback_only(),
            enable_mdns: false,
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        platform.clone(),
    )
    .unwrap();
    let mut hs = host.subscribe();
    wait(&mut hs, 5, "listening", |s: &HostSnapshot| s.listening).await;

    let recorder = Arc::new(Recorder::default());
    let (stream, frames) = (recorder.stream.clone(), recorder.frames.clone());
    let client = ClientEngine::start_on(
        tokio::runtime::Handle::current(),
        ClientConfig {
            data_dir: cd.path().to_path_buf(),
            interfaces: InterfacePolicy::loopback_only(),
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        recorder,
    )
    .unwrap();
    client.set_display(DisplayRequest {
        width: 1280,
        height: 800,
        refresh_hz: 60,
        dpi: 200,
        orientation: Orientation::Landscape,
    });
    let mut cs = client.subscribe();
    client.connect(ConnectTarget { address: "127.0.0.1".into(), port, fingerprint: None });
    let snap = wait(&mut cs, 10, "pin", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Pairing { .. })).await;
    let ClientPhase::Pairing { pin } = snap.phase else { unreachable!() };
    let prompt = wait(&mut hs, 10, "pairing prompt", |s: &HostSnapshot| s.pairing.is_some()).await.pairing.unwrap();
    host.submit_pairing_pin(prompt.id, &pin).unwrap();
    wait(&mut cs, 20, "streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    let (count, keyframes) = *frames.lock().unwrap();
    let session = host.snapshot().session.expect("session");
    eprintln!("frames {count} (keyframes {keyframes}), pipeline {:?}", session.metrics.pipeline);
    assert!(count > 60, "only {count} frames in 3 s");
    assert!(keyframes >= 1);
    let config = session.stream.unwrap();
    assert_eq!((config.codec, config.width, config.height), (Codec::H264, 1280, 800));

    client.disconnect();
    wait(&mut hs, 10, "session end", |s: &HostSnapshot| s.session.is_none()).await;
    host.shutdown();

    let path = cd.path().join("stream.h264");
    std::fs::write(&path, &*stream.lock().unwrap()).unwrap();
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", "h264", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output();
    match out {
        Ok(o) => {
            let errors = String::from_utf8_lossy(&o.stderr);
            assert!(o.status.success() && errors.trim().is_empty(), "ffmpeg: {errors}");
            let probe = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries"])
                .args(["stream=width,height,nb_read_frames", "-of", "csv=p=0"])
                .arg(&path)
                .output()
                .unwrap();
            let info = String::from_utf8_lossy(&probe.stdout);
            eprintln!("ffprobe: {info}");
            assert!(info.starts_with("1280,800,"), "{info}");
        }
        Err(_) => eprintln!("ffmpeg not installed: stream not verified"),
    }
}
