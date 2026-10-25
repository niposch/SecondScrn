//! Host and client engines talking to each other over loopback with fake
//! media: pairing, streaming, input, reconnection, revocation.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use secondscrn_protocol::{
    DisplayRequest, InputEvent, Orientation, PenAction, PointerAction, PointerButton, TouchContact,
};
use secondscrn_security::UnprotectedForTests;
use secondscrn_session::settings::HostSettings;
use secondscrn_session::snapshot::{ClientPhase, ClientSnapshot, HostSnapshot, SessionPhase};
use secondscrn_session::test_support::{FakeClientPlatform, FakeHostPlatform};
use secondscrn_session::*;
use secondscrn_transport::InterfacePolicy;
use tokio::sync::watch;

struct Rig {
    host: HostEngine,
    host_platform: Arc<FakeHostPlatform>,
    client: ClientEngine,
    client_platform: Arc<FakeClientPlatform>,
    port: u16,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn wait<T: Clone, F: Fn(&T) -> bool>(rx: &mut watch::Receiver<T>, secs: u64, what: &str, f: F) -> T {
    let r = tokio::time::timeout(Duration::from_secs(secs), rx.wait_for(|v| f(v))).await;
    match r {
        Ok(Ok(v)) => v.clone(),
        _ => panic!("timed out waiting for {what}"),
    }
}

fn client_engine(dir: &std::path::Path, platform: Arc<FakeClientPlatform>) -> ClientEngine {
    let c = ClientEngine::start_on(
        tokio::runtime::Handle::current(),
        ClientConfig {
            data_dir: dir.to_path_buf(),
            interfaces: InterfacePolicy::loopback_only(),
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        platform,
    )
    .unwrap();
    c.set_display(DisplayRequest {
        width: 1280,
        height: 800,
        refresh_hz: 60,
        dpi: 200,
        orientation: Orientation::Landscape,
    });
    c
}

/// A setting of port 0 is sanitised to the default port, so tests pick a free
/// port explicitly.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn rig() -> Rig {
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();
    let hd = tempfile::tempdir().unwrap();
    let cd = tempfile::tempdir().unwrap();
    let port = free_port();
    let settings = HostSettings { control_port: port, discoverable: false, ..Default::default() };
    secondscrn_session::settings::save(&hd.path().join("settings.json"), &settings).unwrap();
    let host_platform = Arc::new(FakeHostPlatform::default());
    let host = HostEngine::start_on(
        tokio::runtime::Handle::current(),
        HostConfig {
            data_dir: hd.path().to_path_buf(),
            default_host_name: "Test PC".into(),
            listen_addr: [127, 0, 0, 1].into(),
            interfaces: InterfacePolicy::loopback_only(),
            enable_mdns: false,
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        host_platform.clone(),
    )
    .unwrap();
    let mut hs = host.subscribe();
    wait(&mut hs, 5, "listening", |s: &HostSnapshot| s.listening).await;
    let client_platform = Arc::new(FakeClientPlatform::default());
    let client = client_engine(cd.path(), client_platform.clone());
    Rig { host, host_platform, client, client_platform, port, _dirs: (hd, cd) }
}

async fn pair(r: &Rig) {
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    r.client.connect(ConnectTarget { address: "127.0.0.1".into(), port: r.port, fingerprint: None });
    let snap = wait(&mut cs, 10, "pin", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Pairing { .. })).await;
    let ClientPhase::Pairing { pin } = snap.phase else { unreachable!() };
    let prompt = wait(&mut hs, 10, "pairing prompt", |s: &HostSnapshot| s.pairing.is_some()).await.pairing.unwrap();
    assert_eq!(prompt.device_name, "Test Tablet");
    r.host.submit_pairing_pin(prompt.id, &pin).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_stream_input_and_reconnect() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    pair(&r).await;

    // Streaming starts on both sides.
    wait(&mut cs, 15, "client streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;
    let hsnap = wait(&mut hs, 10, "host streaming", |s: &HostSnapshot| {
        s.session.as_ref().is_some_and(|x| x.phase == SessionPhase::Streaming)
    })
    .await;
    assert_eq!(hsnap.paired_devices.len(), 1);
    let session = hsnap.session.unwrap();
    assert_eq!((session.display.as_ref().unwrap().width, session.display.as_ref().unwrap().height), (1280, 800));
    assert_eq!(session.stream.as_ref().unwrap().codec, secondscrn_protocol::Codec::H265);

    // Frames get decoded.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let decoded = r.client_platform.state.frames_decoded.load(Ordering::SeqCst);
    assert!(decoded > 20, "only {decoded} frames decoded");
    assert!(r.client_platform.state.keyframes_decoded.load(Ordering::SeqCst) >= 1);

    // Input reaches the host in order.
    r.client.send_pointer(PointerAction::Down, PointerButton::Primary, 0.5, 0.5);
    r.client.send_pointer(PointerAction::Move, PointerButton::Primary, 0.6, 0.5);
    r.client.send_pointer(PointerAction::Up, PointerButton::Primary, 0.6, 0.5);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let inputs = r.host_platform.state.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 3);
    assert!(inputs.windows(2).all(|w| w[0].seq() < w[1].seq()));

    // Metrics flow back to the host UI.
    wait(&mut hs, 5, "client stats", |s: &HostSnapshot| {
        s.session.as_ref().is_some_and(|x| x.metrics.client.is_some_and(|c| c.frames_decoded > 0))
    })
    .await;

    // User disconnects on the tablet: host session ends, display released.
    r.client.disconnect();
    wait(&mut hs, 10, "host session end", |s: &HostSnapshot| s.session.is_none()).await;
    assert_eq!(r.host_platform.state.displays_released.load(Ordering::SeqCst), 1);

    // Reconnect to the now-known host: no PIN needed.
    let known = r.client.snapshot().known_hosts;
    assert_eq!(known.len(), 1);
    assert!(r.client.connect_last());
    let mut saw_pairing = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let phase = r.client.snapshot().phase;
        saw_pairing |= matches!(phase, ClientPhase::Pairing { .. });
        if phase == ClientPhase::Streaming {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "reconnect timed out in {phase:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!saw_pairing);
    assert_eq!(r.host_platform.state.displays_acquired.load(Ordering::SeqCst), 2);
}

async fn streaming(r: &Rig) {
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    pair(r).await;
    wait(&mut cs, 15, "client streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;
    wait(&mut hs, 10, "host streaming", |s: &HostSnapshot| {
        s.session.as_ref().is_some_and(|x| x.phase == SessionPhase::Streaming)
    })
    .await;
}

fn stream_size(s: &ClientSnapshot) -> Option<(u32, u32, u32)> {
    s.stream.as_ref().map(|c| (c.width, c.height, c.scale_percent))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolution_follows_tablet_pc_and_windows_settings() {
    let r = rig().await;
    streaming(&r).await;
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    assert_eq!(stream_size(&cs.borrow()), Some((1280, 800, 100)));
    // Rotating needs no new mode later: the rotated size is offered up front.
    let first = r.host_platform.state.plans.lock().unwrap()[0].clone();
    assert_eq!(first.alternatives, vec![(800, 1280)]);
    let opts = wait(&mut hs, 5, "options", |s: &HostSnapshot| {
        s.session.as_ref().is_some_and(|x| x.resolutions.len() == secondscrn_protocol::RESOLUTION_SCALES.len())
    })
    .await;
    assert_eq!(opts.session.unwrap().resolutions[0].width, 1280);
    assert_eq!(cs.borrow().resolutions.len(), secondscrn_protocol::RESOLUTION_SCALES.len());

    // The tablet asks for half resolution; the PC remembers it.
    r.client.set_resolution(50);
    wait(&mut cs, 10, "half resolution", |s: &ClientSnapshot| stream_size(s) == Some((640, 400, 50))).await;
    let h = wait(&mut hs, 5, "remembered", |s: &HostSnapshot| {
        s.paired_devices.first().is_some_and(|d| d.preferences.resolution_scale == Some(50))
            && s.session.as_ref().and_then(|x| x.display.as_ref()).is_some_and(|d| d.width == 640)
    })
    .await;
    assert_eq!(h.session.unwrap().stream.unwrap().scale_percent, 50);

    // Rotation keeps the chosen scale.
    r.client.set_display(DisplayRequest {
        width: 800,
        height: 1280,
        refresh_hz: 60,
        dpi: 200,
        orientation: Orientation::Portrait,
    });
    wait(&mut cs, 10, "rotated", |s: &ClientSnapshot| stream_size(s) == Some((400, 640, 50))).await;

    // The PC app picks 75 % (snapped from 74).
    r.host.set_resolution(74).unwrap();
    wait(&mut cs, 10, "75 %", |s: &ClientSnapshot| stream_size(s) == Some((600, 960, 75))).await;

    // Frame rate: the PC's global default (60) is overridden per tablet; the
    // monitor keeps its mode.
    assert_eq!(cs.borrow().frame_rates, vec![30, 60]);
    r.client.set_frame_rate(30);
    wait(&mut cs, 10, "30 fps", |s: &ClientSnapshot| s.stream.as_ref().is_some_and(|c| c.max_fps == 30)).await;
    assert_eq!(stream_size(&cs.borrow()), Some((600, 960, 75)));
    wait(&mut hs, 5, "fps remembered", |s: &HostSnapshot| {
        s.paired_devices.first().is_some_and(|d| d.preferences.max_fps == Some(30))
    })
    .await;

    // The user changes the mode in the Windows display settings: the stream
    // follows it instead of failing.
    *r.host_platform.state.external_mode.lock().unwrap() = Some((768, 1024));
    wait(&mut cs, 10, "external mode", |s: &ClientSnapshot| stream_size(s) == Some((768, 1024, 0))).await;
    assert_eq!(r.client.snapshot().phase, ClientPhase::Streaming);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tablet_changes_windows_scaling() {
    let r = rig().await;
    streaming(&r).await;
    let mut cs = r.client.subscribe();
    let s = wait(&mut cs, 5, "scale", |s: &ClientSnapshot| s.ui_scale.is_some()).await.ui_scale.unwrap();
    assert_eq!(s.current, 150);
    assert!(s.options.contains(&200));
    r.client.set_ui_scale(200);
    wait(&mut cs, 5, "scale 200", |s: &ClientSnapshot| s.ui_scale.as_ref().is_some_and(|u| u.current == 200)).await;
    assert_eq!(r.host_platform.state.ui_scale.load(Ordering::SeqCst), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn touch_and_pen_reach_the_host() {
    let r = rig().await;
    streaming(&r).await;
    let c = |id, action, x| TouchContact { id, action, x, y: 0.5 };
    r.client.send_touch(vec![c(0, PointerAction::Down, 0.2)]);
    r.client.send_touch(vec![c(0, PointerAction::Move, 0.3), c(1, PointerAction::Down, 1.7)]);
    r.client.send_touch(vec![c(0, PointerAction::Up, 0.3), c(1, PointerAction::Up, 0.8)]);
    r.client.send_pen(PenAction::Hover, 0.4, 0.4, 0.0, 0.0, 0.0, false, false);
    r.client.send_pen(PenAction::Down, 0.4, 0.4, 0.7, 10.0, -5.0, false, false);
    r.client.send_pen(PenAction::Up, 0.4, 0.4, 0.0, 10.0, -5.0, false, false);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let inputs = r.host_platform.state.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 6);
    match &inputs[1] {
        InputEvent::Touch { contacts, .. } => {
            assert_eq!(contacts.len(), 2);
            assert_eq!(contacts[1].x, 1.0, "coordinates are clamped");
        }
        other => panic!("expected touch, got {other:?}"),
    }
    assert!(matches!(inputs[4], InputEvent::Pen { action: PenAction::Down, pressure, .. } if pressure == 0.7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_pin_is_rejected_and_not_trusted() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    r.client.connect(ConnectTarget { address: "127.0.0.1".into(), port: r.port, fingerprint: None });
    let snap = wait(&mut cs, 10, "pin", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Pairing { .. })).await;
    let ClientPhase::Pairing { pin } = snap.phase else { unreachable!() };
    let wrong = if pin == "000000" { "111111" } else { "000000" };
    let prompt = wait(&mut hs, 10, "prompt", |s: &HostSnapshot| s.pairing.is_some()).await.pairing.unwrap();
    r.host.submit_pairing_pin(prompt.id, wrong).unwrap();

    let snap = wait(&mut cs, 15, "failure", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Failed { .. })).await;
    let ClientPhase::Failed { message, .. } = snap.phase else { unreachable!() };
    assert!(message.contains("did not match"), "{message}");
    let hsnap = wait(&mut hs, 10, "prompt cleared", |s: &HostSnapshot| s.pairing.is_none()).await;
    assert!(hsnap.paired_devices.is_empty());
    assert!(r.client.snapshot().known_hosts.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declined_pairing_reports_to_tablet() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    r.client.connect(ConnectTarget { address: "127.0.0.1".into(), port: r.port, fingerprint: None });
    let prompt = wait(&mut hs, 10, "prompt", |s: &HostSnapshot| s.pairing.is_some()).await.pairing.unwrap();
    r.host.reject_pairing(prompt.id).unwrap();
    let snap = wait(&mut cs, 10, "failure", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Failed { .. })).await;
    assert!(matches!(snap.phase, ClientPhase::Failed { needs_pairing: false, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_tablet_must_pair_again() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    let mut hs = r.host.subscribe();
    pair(&r).await;
    wait(&mut cs, 15, "streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;
    let fp = r.host.snapshot().paired_devices[0].fingerprint.clone();

    assert!(r.host.revoke_device(&fp).unwrap());
    wait(&mut hs, 10, "session end", |s: &HostSnapshot| s.session.is_none()).await;
    // Host ended the session: the tablet stops and does not auto-reconnect.
    wait(&mut cs, 10, "client stopped", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Failed { .. })).await;

    assert!(r.client.connect_last());
    let snap = wait(&mut cs, 10, "not paired", |s: &ClientSnapshot| {
        matches!(s.phase, ClientPhase::Failed { needs_pairing: true, .. })
    })
    .await;
    let ClientPhase::Failed { message, .. } = snap.phase else { unreachable!() };
    assert!(message.contains("no longer paired"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_restart_triggers_client_reconnect() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    pair(&r).await;
    wait(&mut cs, 15, "streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;

    // The PC app stops (update/restart): the tablet must keep retrying.
    let Rig { host, client, port, _dirs, .. } = r;
    let host_dir = _dirs.0.path().to_path_buf();
    drop(host);
    wait(&mut cs, 30, "reconnecting", |s: &ClientSnapshot| matches!(s.phase, ClientPhase::Reconnecting { .. })).await;

    // The PC comes back with the same identity on the same port.
    let settings = HostSettings { control_port: port, discoverable: false, ..Default::default() };
    secondscrn_session::settings::save(&host_dir.join("settings.json"), &settings).unwrap();
    let platform = Arc::new(FakeHostPlatform::default());
    let host2 = HostEngine::start_on(
        tokio::runtime::Handle::current(),
        HostConfig {
            data_dir: host_dir,
            default_host_name: "Test PC".into(),
            listen_addr: [127, 0, 0, 1].into(),
            interfaces: InterfacePolicy::loopback_only(),
            enable_mdns: false,
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        platform,
    )
    .unwrap();
    wait(&mut cs, 30, "streaming again", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;
    drop(host2);
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_tablet_cannot_connect_without_pairing() {
    let r = rig().await;
    let mut cs = r.client.subscribe();
    // Pretend we know the host but it doesn't know us.
    let fp = r.host.fingerprint().to_hex();
    let mut settings = r.client.snapshot().settings;
    settings.known_hosts.push(secondscrn_session::settings::KnownHost {
        fingerprint: fp.clone(),
        name: "Test PC".into(),
        last_address: "127.0.0.1".into(),
        port: r.port,
        auto_reconnect: true,
        last_connected: None,
    });
    r.client.update_settings(settings);
    r.client.connect(ConnectTarget { address: "127.0.0.1".into(), port: r.port, fingerprint: Some(fp) });
    wait(&mut cs, 10, "not paired", |s: &ClientSnapshot| {
        matches!(s.phase, ClientPhase::Failed { needs_pairing: true, .. })
    })
    .await;
    assert!(r.host.snapshot().session.is_none());
    assert_eq!(r.host_platform.state.displays_acquired.load(Ordering::SeqCst), 0);
}
