//! End-to-end test of the real Windows host: virtual monitor (VDD), WGC
//! capture, GPU conversion, hardware encoding, WebRTC over loopback, and
//! input injection, against a recording client.
//!
//! Needs a GPU with a hardware encoder and the Virtual Display Driver, and
//! briefly adds a monitor to the desktop and moves the cursor. Run with:
//!   cargo test -p secondscrn-windows-host --test e2e -- --ignored --nocapture

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use secondscrn_devtools::RecordingPlatform;
use secondscrn_host::platform::{DisplaySource, WindowsHostPlatform};
use secondscrn_host::{capture, display, input};
use secondscrn_protocol::{
    DisplayRequest, InputEvent, Orientation, PenAction, PointerAction, PointerButton, TouchContact,
};
use secondscrn_security::UnprotectedForTests;
use secondscrn_session::platform::{DisplayLease, DisplayPlan};
use secondscrn_session::settings::HostSettings;
use secondscrn_session::snapshot::{ClientPhase, SessionPhase};
use secondscrn_session::{ClientConfig, ClientEngine, ConnectTarget, HostConfig, HostEngine};
use secondscrn_transport::InterfacePolicy;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore]
fn virtual_monitor_stream_and_input() {
    secondscrn_host::init_dpi_awareness();
    let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
    let protected_before = display::protected_targets().unwrap();

    let host_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let settings =
        HostSettings { control_port: port, discoverable: false, display_linger_secs: 0, ..Default::default() };
    secondscrn_session::settings::save(&host_dir.path().join("settings.json"), &settings).unwrap();

    let platform = Arc::new(WindowsHostPlatform::new(DisplaySource::Virtual));
    let host = HostEngine::start(
        HostConfig {
            data_dir: host_dir.path().into(),
            default_host_name: "e2e host".into(),
            listen_addr: [127, 0, 0, 1].into(),
            interfaces: InterfacePolicy::loopback_only(),
            enable_mdns: false,
            protector: Arc::new(secondscrn_host::dpapi::Dpapi),
            logs: None,
            app_version: "test".into(),
        },
        platform.clone(),
    )
    .unwrap();

    let out = host_dir.path().join("stream.bin");
    let recording = RecordingPlatform::new(Some(out.clone()));
    let rec_stats = recording.stats.clone();
    let client = ClientEngine::start(
        ClientConfig {
            data_dir: client_dir.path().into(),
            interfaces: InterfacePolicy::loopback_only(),
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        Arc::new(recording),
    )
    .unwrap();
    client.set_display(DisplayRequest {
        width: 1920,
        height: 1200,
        refresh_hz: 60,
        dpi: 220,
        orientation: Orientation::Landscape,
    });

    // Pair.
    client.connect(ConnectTarget { address: "127.0.0.1".into(), port, fingerprint: None });
    let mut pin = String::new();
    wait("pin", 10, || {
        if let ClientPhase::Pairing { pin: p } = client.snapshot().phase {
            pin = p;
            true
        } else {
            false
        }
    });
    wait("pairing prompt", 10, || host.snapshot().pairing.is_some());
    host.submit_pairing_pin(host.snapshot().pairing.unwrap().id, &pin).unwrap();

    // Stream from the virtual monitor.
    wait("streaming", 60, || client.snapshot().phase == ClientPhase::Streaming);
    wait("host streaming", 10, || host.snapshot().session.is_some_and(|s| s.phase == SessionPhase::Streaming));
    let session = host.snapshot().session.unwrap();
    let display_info = session.display.clone().unwrap();
    println!("streaming {:?} from {:?}", session.stream, display_info);
    assert!(display_info.is_virtual);
    assert_eq!((display_info.width, display_info.height), (1920, 1200));

    // Input travels through the session (ordering/auth are covered by the
    // engine tests; coordinate mapping by `input_mapping_on_virtual_monitor`).
    client.send_pointer(PointerAction::Move, PointerButton::Primary, 0.5, 0.5);

    // Let it run a bit, then check stats on both sides.
    std::thread::sleep(Duration::from_secs(3));
    let frames = rec_stats.frames.load(Ordering::Relaxed);
    let keys = rec_stats.keyframes.load(Ordering::Relaxed);
    let hs = host.snapshot().session.unwrap();
    println!("client received {frames} frames ({keys} keyframes); host metrics {:?}", hs.metrics);
    assert!(frames >= 3, "too few frames: {frames}");
    assert!(keys >= 1);
    assert!(hs.metrics.rtt_ms.is_some());

    // End: the monitor is removed (linger 0) and user displays are intact.
    client.disconnect();
    wait("session end", 10, || host.snapshot().session.is_none());
    wait("virtual monitor detached", 15, || display::active_vdd().ok().flatten().is_none());
    let protected_after = display::protected_targets().unwrap();
    for p in &protected_before {
        assert!(protected_after.contains(p), "display {p} was deactivated");
    }
    drop(client);
    host.shutdown();

    // The recorded bitstream must be decodable by an independent decoder.
    if let Ok(o) = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,color_range,color_primaries,nb_read_frames"])
        .args(["-of", "compact"])
        .arg(&out)
        .output()
    {
        let text = String::from_utf8_lossy(&o.stdout);
        println!("ffprobe: {text}");
        assert!(text.contains("width=1920") && text.contains("height=1200"), "{text}");
        assert!(text.contains("color_primaries=bt709") && text.contains("color_range=tv"), "{text}");
    }
}

/// Injects a pointer event and reads the cursor back immediately. Skips
/// (with a message) when Windows blocks injection, e.g. because an elevated
/// window has focus.
#[test]
#[ignore]
fn input_mapping_on_virtual_monitor() {
    secondscrn_host::init_dpi_awareness();
    let req =
        DisplayRequest { width: 1920, height: 1200, refresh_hz: 60, dpi: 96, orientation: Orientation::Landscape };
    let mgr = secondscrn_host::virtual_display::VirtualDisplayManager::default();
    let lease = mgr.acquire(&DisplayPlan { request: req, alternatives: Vec::new() }, Duration::ZERO).unwrap();
    let info = lease.info();
    let monitor = capture::monitor_by_device(&info.id).unwrap();
    let mut injector = input::Injector::default();
    let mut saved = POINT::default();
    unsafe { GetCursorPos(&mut saved).unwrap() };
    let mut results = Vec::new();
    for (x, y) in [(0.0f32, 0.0f32), (0.5, 0.5), (1.0, 1.0), (0.25, 0.75)] {
        let ev = secondscrn_protocol::InputEvent::Pointer {
            seq: 1,
            action: PointerAction::Move,
            button: PointerButton::Primary,
            x,
            y,
        };
        if let Err(e) = injector.inject(&info.id, &ev) {
            eprintln!("skipping: input injection blocked: {e}");
            unsafe { SetCursorPos(saved.x, saved.y).unwrap() };
            return;
        }
        let mut pos = POINT::default();
        unsafe { GetCursorPos(&mut pos).unwrap() };
        if results.is_empty() && pos.x == saved.x && pos.y == saved.y {
            // UIPI drops injected input silently while an elevated window
            // has focus; SendInput still reports success.
            eprintln!("skipping: injected input had no effect (elevated window focused or input desktop locked)");
            return;
        }
        results.push(((x, y), pos));
    }
    unsafe { SetCursorPos(saved.x, saved.y).unwrap() };
    drop(lease);
    let r = monitor.rect;
    for ((x, y), pos) in results {
        let ex = r.left + (x * (r.right - r.left - 1) as f32).round() as i32;
        let ey = r.top + (y * (r.bottom - r.top - 1) as f32).round() as i32;
        assert!((pos.x - ex).abs() <= 2 && (pos.y - ey).abs() <= 2, "({x},{y}) -> {pos:?}, expected ({ex}, {ey})");
    }
}

/// Real touch and pen through synthetic pointer devices: Windows must accept
/// every frame of a two-finger gesture and a pen stroke with hover. Taps on
/// the (empty) virtual monitor only.
#[test]
#[ignore]
fn touch_and_pen_injection_on_virtual_monitor() {
    secondscrn_host::init_dpi_awareness();
    let req =
        DisplayRequest { width: 1920, height: 1200, refresh_hz: 60, dpi: 96, orientation: Orientation::Landscape };
    let mgr = secondscrn_host::virtual_display::VirtualDisplayManager::default();
    let lease = mgr.acquire(&DisplayPlan { request: req, alternatives: Vec::new() }, Duration::ZERO).unwrap();
    let id = lease.info().id;
    let mut injector = input::Injector::default();
    let c = |id, action, x: f32| TouchContact { id, action, x, y: 0.9 };
    let frames = [
        vec![c(0, PointerAction::Down, 0.90)],
        vec![c(0, PointerAction::Move, 0.91), c(1, PointerAction::Down, 0.95)],
        vec![c(0, PointerAction::Move, 0.92), c(1, PointerAction::Move, 0.96)],
        vec![c(0, PointerAction::Up, 0.92), c(1, PointerAction::Move, 0.96)],
        vec![c(1, PointerAction::Up, 0.96)],
    ];
    for (seq, contacts) in frames.into_iter().enumerate() {
        let ev = InputEvent::Touch { seq: seq as u64 + 1, contacts };
        if let Err(e) = injector.inject(&id, &ev) {
            panic!("touch frame {seq} rejected: {e:#}");
        }
        std::thread::sleep(Duration::from_millis(20));
        injector.idle(); // held contacts are refreshed without errors
    }
    let pen = |action, pressure| InputEvent::Pen {
        seq: 100,
        action,
        x: 0.9,
        y: 0.1,
        pressure,
        tilt_x: 20.0,
        tilt_y: -10.0,
        barrel: false,
        eraser: false,
    };
    for (action, p) in [
        (PenAction::Hover, 0.0),
        (PenAction::Down, 0.3),
        (PenAction::Move, 0.8),
        (PenAction::Up, 0.0),
        (PenAction::Hover, 0.0),
        (PenAction::Leave, 0.0),
    ] {
        if let Err(e) = injector.inject(&id, &pen(action, p)) {
            panic!("pen {action:?} rejected: {e:#}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    injector.release_all();
    drop(lease);
}
