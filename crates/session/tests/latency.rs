//! End-to-end latency under network trouble: host and client engines over
//! loopback with fake media whose frames are as large as the bitrate target
//! allows and carry their capture time, and a simulated bottleneck link on
//! the host's media path. The picture must stay recent (bounded latency),
//! keep moving (no freezes) and recover full quality when the link clears.

use std::sync::Arc;
use std::time::{Duration, Instant};

use secondscrn_protocol::{DisplayRequest, Orientation};
use secondscrn_security::UnprotectedForTests;
use secondscrn_session::settings::HostSettings;
use secondscrn_session::snapshot::{ClientPhase, ClientSnapshot, HostSnapshot, SessionPhase};
use secondscrn_session::test_support::{FakeClientPlatform, FakeHostPlatform};
use secondscrn_session::*;
use secondscrn_transport::{Impairment, InterfacePolicy};
use tokio::sync::watch;

struct Rig {
    host: HostEngine,
    host_platform: Arc<FakeHostPlatform>,
    _client: ClientEngine,
    client_platform: Arc<FakeClientPlatform>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn wait<T: Clone, F: Fn(&T) -> bool>(rx: &mut watch::Receiver<T>, secs: u64, what: &str, f: F) -> T {
    match tokio::time::timeout(Duration::from_secs(secs), rx.wait_for(|v| f(v))).await {
        Ok(Ok(v)) => v.clone(),
        _ => panic!("timed out waiting for {what}"),
    }
}

/// Paired and streaming 1280x800 at 60 fps.
async fn streaming() -> Rig {
    streaming_with(FakeHostPlatform { sized_by_bitrate: true, ..Default::default() }, FakeClientPlatform::default())
        .await
}

async fn streaming_with(platform: FakeHostPlatform, client: FakeClientPlatform) -> Rig {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter(filter).try_init();
    let (hd, cd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let settings = HostSettings { control_port: port, discoverable: false, ..Default::default() };
    secondscrn_session::settings::save(&hd.path().join("settings.json"), &settings).unwrap();
    let host_platform = Arc::new(platform);
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
    let client_platform = Arc::new(client);
    let client = ClientEngine::start_on(
        tokio::runtime::Handle::current(),
        ClientConfig {
            data_dir: cd.path().to_path_buf(),
            interfaces: InterfacePolicy::loopback_only(),
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: "test".into(),
        },
        client_platform.clone(),
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
    wait(&mut cs, 15, "client streaming", |s: &ClientSnapshot| s.phase == ClientPhase::Streaming).await;
    wait(&mut hs, 10, "host streaming", |s: &HostSnapshot| {
        s.session.as_ref().is_some_and(|x| x.phase == SessionPhase::Streaming)
    })
    .await;
    Rig { host, host_platform, _client: client, client_platform, _dirs: (hd, cd) }
}

/// Latency and freezes of the frames decoded in `from..to`.
struct Window {
    frames: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
    /// Longest time without a new picture.
    longest_gap: Duration,
}

fn window(r: &Rig, from: Instant, to: Instant) -> Window {
    let all = r.client_platform.state.latencies.lock().unwrap().clone();
    let picked: Vec<(Instant, Duration)> = all.into_iter().filter(|(t, _)| *t >= from && *t < to).collect();
    let mut lat: Vec<Duration> = picked.iter().map(|(_, l)| *l).collect();
    lat.sort();
    let pct = |p: f64| {
        lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1))).copied().unwrap_or_default()
    };
    let mut times: Vec<Instant> = vec![from];
    times.extend(picked.iter().map(|(t, _)| *t));
    times.push(to);
    let longest_gap = times.windows(2).map(|w| w[1].saturating_duration_since(w[0])).max().unwrap_or_default();
    Window {
        frames: lat.len(),
        p50: pct(0.5),
        p95: pct(0.95),
        max: lat.last().copied().unwrap_or_default(),
        longest_gap,
    }
}

fn target_bps(r: &Rig) -> u64 {
    r.host_platform.state.last_target.lock().unwrap().map_or(0, |(b, _)| b)
}

fn report(what: &str, w: &Window, r: &Rig) {
    println!(
        "{what}: {} frames, latency p50 {:?} p95 {:?} max {:?}, longest gap {:?}, target {:.1} Mbit/s",
        w.frames,
        w.p50,
        w.p95,
        w.max,
        w.longest_gap,
        target_bps(r) as f64 / 1e6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_stays_bounded_when_the_link_shrinks_and_recovers() {
    let r = streaming().await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let t0 = Instant::now();
    report("clean", &window(&r, t0 - Duration::from_secs(2), t0), &r);
    let clean_target = target_bps(&r);

    // The link drops to 3 Mbit/s behind a deep (1 s) buffer, like Wi-Fi with
    // interference: the stream must shrink to fit without queueing up.
    r.host.set_network_impairment(Some(Impairment {
        bandwidth_bps: Some(3_000_000),
        queue_limit: Duration::from_secs(1),
        ..Default::default()
    }));
    tokio::time::sleep(Duration::from_secs(10)).await;
    let t1 = Instant::now();
    let reacting = window(&r, t0, t0 + Duration::from_secs(2));
    let constrained = window(&r, t0 + Duration::from_secs(2), t1);
    report("first 2 s on the slow link", &reacting, &r);
    report("slow link, settled", &constrained, &r);

    // The link clears: quality comes back.
    r.host.set_network_impairment(None);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let t2 = Instant::now();
    let recovered = window(&r, t2 - Duration::from_secs(3), t2);
    report("recovered", &recovered, &r);

    assert!(reacting.max < Duration::from_millis(400), "latency while adapting: {:?}", reacting.max);
    assert!(reacting.longest_gap < Duration::from_millis(400), "freeze while adapting: {:?}", reacting.longest_gap);
    assert!(constrained.p95 < Duration::from_millis(120), "latency on the slow link: p95 {:?}", constrained.p95);
    assert!(
        constrained.longest_gap < Duration::from_millis(250),
        "freeze on the slow link: {:?}",
        constrained.longest_gap
    );
    assert!(constrained.frames as f64 > 8.0 * 15.0, "too few pictures on the slow link: {}", constrained.frames);
    assert!(recovered.p95 < Duration::from_millis(60), "latency after recovery: p95 {:?}", recovered.p95);
    assert!(
        target_bps(&r) as f64 >= clean_target as f64 * 0.8,
        "quality did not recover: {:.1} of {:.1} Mbit/s",
        target_bps(&r) as f64 / 1e6,
        clean_target as f64 / 1e6
    );
}

/// Wi-Fi hiccups: every 1.5 s nothing gets through for 200 ms (longer than
/// retransmission can repair), on a 20 ms link that carries 5 Mbit/s.
async fn hiccups(references: bool) -> (Window, u64, u64, u64) {
    let r = streaming_with(
        FakeHostPlatform { sized_by_bitrate: true, references, ..Default::default() },
        FakeClientPlatform::default(),
    )
    .await;
    let link = Impairment {
        bandwidth_bps: Some(5_000_000),
        queue_limit: Duration::from_millis(500),
        delay: Duration::from_millis(20),
        ..Default::default()
    };
    r.host.set_network_impairment(Some(link.clone()));
    tokio::time::sleep(Duration::from_secs(4)).await;
    let state = &r.host_platform.state;
    let keyframes_before = state.keyframe_requests.load(std::sync::atomic::Ordering::SeqCst);
    let t0 = Instant::now();
    for _ in 0..6 {
        r.host.set_network_impairment(Some(Impairment::outage()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        r.host.set_network_impairment(Some(link.clone()));
        tokio::time::sleep(Duration::from_millis(1300)).await;
    }
    let w = window(&r, t0, Instant::now());
    if std::env::var_os("SHOW_GAPS").is_some() {
        let lat = r.client_platform.state.latencies.lock().unwrap().clone();
        for p in lat.windows(2).filter(|p| p[1].0 - p[0].0 > Duration::from_millis(150)) {
            let wall = std::time::SystemTime::now() - Instant::now().duration_since(p[0].0);
            let ms = wall.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 86_400_000;
            let (h, m, sec, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
            println!("gap {:?} from {h:02}:{m:02}:{sec:02}.{milli:03} (latency after {:?})", p[1].0 - p[0].0, p[1].1);
        }
    }
    let keyframes = state.keyframe_requests.load(std::sync::atomic::Ordering::SeqCst) - keyframes_before;
    let recoveries = state.recoveries.load(std::sync::atomic::Ordering::SeqCst);
    report(if references { "hiccups, reference recovery" } else { "hiccups, keyframes" }, &w, &r);
    println!("    keyframes {keyframes}, recoveries {recoveries}");
    (w, keyframes, recoveries, target_bps(&r))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hiccups_neither_freeze_long_nor_starve_the_stream() {
    let (w, keyframes, _, target) = hiccups(true).await;
    // The hiccup itself (200 ms), the repair window (100 ms) and recovery.
    assert!(w.longest_gap < Duration::from_millis(500), "freeze: {:?}", w.longest_gap);
    assert!(keyframes <= 1, "{keyframes} keyframes");
    // Outages are not congestion: the stream keeps most of the link.
    assert!(target > 2_000_000, "starved: {:.1} Mbit/s on a 5 Mbit/s link", target as f64 / 1e6);
}

/// The tablet's decoder stalls for 400 ms every 2 s (a busy tablet): frames
/// queued meanwhile are too old to show, so the stream must resume from a
/// fresh picture, a recovery frame or a keyframe.
async fn decoder_stalls(references: bool) -> (Window, u64, u64) {
    let r = streaming_with(
        FakeHostPlatform { sized_by_bitrate: true, references, ..Default::default() },
        FakeClientPlatform {
            decode_stall: Some((Duration::from_secs(2), Duration::from_millis(400))),
            ..Default::default()
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let state = &r.host_platform.state;
    let keyframes_before = state.keyframe_requests.load(std::sync::atomic::Ordering::SeqCst);
    let recoveries_before = state.recoveries.load(std::sync::atomic::Ordering::SeqCst);
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let w = window(&r, t0, Instant::now());
    let keyframes = state.keyframe_requests.load(std::sync::atomic::Ordering::SeqCst) - keyframes_before;
    let recoveries = state.recoveries.load(std::sync::atomic::Ordering::SeqCst) - recoveries_before;
    report(if references { "decoder stalls, reference recovery" } else { "decoder stalls, keyframes" }, &w, &r);
    println!("    keyframes {keyframes}, recoveries {recoveries}");
    (w, keyframes, recoveries)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decoder_stalls_recover_from_references() {
    let (w, keyframes, recoveries) = decoder_stalls(true).await;
    assert!(recoveries >= 3, "only {recoveries} recoveries for 5 stalls");
    assert!(keyframes <= 1, "{keyframes} keyframes");
    // The stall itself plus at most a round of recovery.
    assert!(w.longest_gap < Duration::from_millis(600), "freeze: {:?}", w.longest_gap);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decoder_stalls_without_references_use_keyframes() {
    let (w, keyframes, recoveries) = decoder_stalls(false).await;
    assert_eq!(recoveries, 0);
    assert!(keyframes >= 3, "only {keyframes} keyframes for 5 stalls");
    assert!(w.longest_gap < Duration::from_millis(700), "freeze: {:?}", w.longest_gap);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_too_slow_for_the_chosen_quality_is_reported() {
    use secondscrn_session::snapshot::PerformanceWarning;
    let r = streaming().await;
    let (mut hs, mut cs) = (r.host.subscribe(), r._client.subscribe());
    // Far below what 1280x800 at 60 fps needs.
    r.host.set_network_impairment(Some(Impairment {
        bandwidth_bps: Some(700_000),
        queue_limit: Duration::from_millis(500),
        ..Default::default()
    }));
    wait(&mut hs, 15, "limited on the PC", |s: &HostSnapshot| s.session.as_ref().is_some_and(|x| x.network_limited))
        .await;
    wait(&mut cs, 5, "warning on the tablet", |s: &ClientSnapshot| {
        s.metrics.warning == Some(PerformanceWarning::Bandwidth)
    })
    .await;
    r.host.set_network_impairment(None);
    wait(&mut hs, 20, "no longer limited", |s: &HostSnapshot| s.session.as_ref().is_some_and(|x| !x.network_limited))
        .await;
    wait(&mut cs, 5, "warning gone", |s: &ClientSnapshot| s.metrics.warning.is_none()).await;
}
