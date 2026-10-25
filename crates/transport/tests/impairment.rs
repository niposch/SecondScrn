//! Reproducible network impairment: random and burst loss, delay/jitter, a
//! bandwidth bottleneck and a temporary outage, applied deterministically to
//! a real WebRTC session over loopback.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secondscrn_protocol::Codec;
use secondscrn_transport::*;

async fn connect() -> (MediaHandle, MediaHandle) {
    let (pending, offer) =
        PendingSender::create(SenderConfig { codec: Codec::H264, interfaces: InterfacePolicy::loopback_only() })
            .await
            .unwrap();
    let (mut receiver, answer) = accept_offer(
        ReceiverConfig {
            codec: Codec::H264,
            interfaces: InterfacePolicy::loopback_only(),
            host_hint: Some([127, 0, 0, 1].into()),
        },
        &offer,
    )
    .await
    .unwrap();
    let mut sender = pending.accept_answer(&answer).unwrap();
    for h in [&mut sender, &mut receiver] {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline.into(), h.events.recv()).await.expect("connect") {
                Some(TransportEvent::Connected { .. }) => break,
                Some(TransportEvent::Closed { reason }) => panic!("{reason}"),
                _ => {}
            }
        }
    }
    (sender, receiver)
}

fn frame(i: u64, key: bool, size: usize) -> Arc<[u8]> {
    let mut f = if key {
        vec![0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 0, 1, 0x65]
    } else {
        vec![0, 0, 0, 1, 0x41]
    };
    f.extend((0..size).map(|j| ((j as u64 + i) % 250 + 1) as u8));
    Arc::from(f)
}

/// Sends 60 fps frames of `size` bytes until `stop`; keyframes on request.
fn spawn_source(sender: FrameSender, size: usize, key_requested: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut i = 0u64;
        let start = Instant::now();
        // An interval keeps the average rate even with a coarse OS timer
        // (Windows sleeps round up to ~15.6 ms by default).
        let mut tick = tokio::time::interval(Duration::from_micros(16_667));
        while !stop.load(Ordering::Relaxed) {
            tick.tick().await;
            let key = i == 0 || key_requested.swap(false, Ordering::Relaxed);
            sender.send_frame(EncodedFrame {
                data: frame(i, key, if key { size * 4 } else { size }),
                capture_time: start + Duration::from_micros(i * 16_667),
                keyframe: key,
            });
            i += 1;
        }
    });
}

#[derive(Default)]
struct Observed {
    frames: AtomicU64,
    keyframes: AtomicU64,
    gaps: AtomicU64,
    keyframe_requests: AtomicU64,
    interrupted: AtomicBool,
    resumed: AtomicBool,
    closed: AtomicBool,
    stats: Mutex<Vec<TransportStats>>,
}

/// Moves a handle's event stream into a task that records what happens. The
/// handle stays with the caller so impairments can still be changed.
fn observe(h: &mut MediaHandle, key: Option<Arc<AtomicBool>>) -> Arc<Observed> {
    let o = Arc::new(Observed::default());
    let o2 = o.clone();
    let mut events = std::mem::replace(&mut h.events, tokio::sync::mpsc::channel(1).1);
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                TransportEvent::Frame(f) => {
                    o2.frames.fetch_add(1, Ordering::Relaxed);
                    if f.keyframe {
                        o2.keyframes.fetch_add(1, Ordering::Relaxed);
                    }
                    if !f.contiguous {
                        o2.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                }
                TransportEvent::KeyframeRequested => {
                    o2.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    if let Some(k) = &key {
                        k.store(true, Ordering::Relaxed);
                    }
                }
                TransportEvent::Stats(s) => o2.stats.lock().unwrap().push(s),
                TransportEvent::Interrupted => o2.interrupted.store(true, Ordering::Relaxed),
                TransportEvent::Resumed => o2.resumed.store(true, Ordering::Relaxed),
                TransportEvent::Closed { .. } => {
                    o2.closed.store(true, Ordering::Relaxed);
                    return;
                }
                _ => {}
            }
        }
    });
    o
}

struct Run {
    receiver: Arc<Observed>,
    sender: Arc<Observed>,
}

async fn run(sender_imp: Option<Impairment>, receiver_imp: Option<Impairment>, size: usize, secs: u64) -> Run {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_default())
        .try_init();
    let (mut sender, mut receiver) = connect().await;
    let probe = sender.frame_sender();
    sender.set_impairment(sender_imp);
    receiver.set_impairment(receiver_imp);
    let key = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    spawn_source(sender.frame_sender(), size, key.clone(), stop.clone());
    let tx = observe(&mut sender, Some(key));
    let rx = observe(&mut receiver, None);
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.store(true, Ordering::Relaxed);
    eprintln!("sender queue drops: {}", probe.frames_dropped());
    Run { receiver: rx, sender: tx }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn baseline_without_impairment() {
    let r = run(Some(Impairment::default()), None, 6000, 5).await;
    let frames = r.receiver.frames.load(Ordering::Relaxed);
    let stats = r.sender.stats.lock().unwrap().last().copied();
    eprintln!("baseline frames {frames}, sender stats {stats:?}");
    assert!(frames > 220, "only {frames} frames delivered without impairment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_loss_is_repaired() {
    // 5 % independent loss on media; NACK/RTX repair keeps frames coming.
    let imp = Impairment { loss: 0.05, burst: 1, seed: 7, ..Default::default() };
    let r = run(Some(imp), None, 4000, 4).await;
    let frames = r.receiver.frames.load(Ordering::Relaxed);
    // ~240 frames are sent; most must be delivered complete.
    assert!(frames > 180, "only {frames} frames delivered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_loss_recovers_through_keyframes() {
    let imp = Impairment { loss: 0.01, burst: 30, seed: 11, ..Default::default() };
    let r = run(Some(imp), None, 6000, 5).await;
    let frames = r.receiver.frames.load(Ordering::Relaxed);
    let requests = r.sender.keyframe_requests.load(Ordering::Relaxed);
    let keys = r.receiver.keyframes.load(Ordering::Relaxed);
    let gaps = r.receiver.gaps.load(Ordering::Relaxed);
    eprintln!(
        "burst: frames {frames} gaps {gaps} keyframe requests {requests} keyframes {keys} sender {:?}",
        r.sender.stats.lock().unwrap().last()
    );
    assert!(frames > 150);
    // Bursts lose whole frames: the receiver must notice the gap, ask for an
    // IDR, and get one.
    assert!(requests > 0, "receiver should ask for a keyframe after a burst");
    assert!(keys >= 2, "a fresh keyframe must arrive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delay_and_jitter_are_tolerated_and_visible_in_rtt() {
    let imp = Impairment {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(20),
        seed: 3,
        ..Default::default()
    };
    let r = run(Some(imp.clone()), Some(imp), 2000, 4).await;
    assert!(r.receiver.frames.load(Ordering::Relaxed) > 180);
    let rtt = r.sender.stats.lock().unwrap().iter().filter_map(|s| s.rtt).max().expect("rtt measured");
    assert!(rtt >= Duration::from_millis(70), "rtt {rtt:?} should include ~80 ms of added delay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bottleneck_shows_up_as_loss_or_delay() {
    // ~6 Mbit/s of media through a 3 Mbit/s bottleneck with a 150 ms queue:
    // the signals the session's controller reacts to must appear.
    let imp = Impairment {
        bandwidth_bps: Some(3_000_000),
        queue_limit: Duration::from_millis(150),
        seed: 5,
        ..Default::default()
    };
    let r = run(Some(imp), None, 12_000, 6).await;
    let stats = r.sender.stats.lock().unwrap();
    let worst_loss = stats.iter().filter_map(|s| s.loss).fold(0.0f32, f32::max);
    let worst_rtt = stats.iter().filter_map(|s| s.rtt).max().unwrap_or_default();
    assert!(
        worst_loss > 0.02 || worst_rtt > Duration::from_millis(80),
        "no congestion signal: loss {worst_loss}, rtt {worst_rtt:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temporary_outage_interrupts_and_resumes() {
    let (mut sender, mut receiver) = connect().await;
    let key = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    spawn_source(sender.frame_sender(), 2000, key.clone(), stop.clone());
    let _tx = observe(&mut sender, Some(key));
    let o = observe(&mut receiver, None);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Both directions go dark (Wi-Fi drop).
    sender.set_impairment(Some(Impairment::outage()));
    receiver.set_impairment(Some(Impairment::outage()));
    let started = Instant::now();
    while !o.interrupted.load(Ordering::Relaxed) {
        assert!(!o.closed.load(Ordering::Relaxed), "closed after {:?} without Interrupted", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(40), "outage not detected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("outage detected after {:?}", started.elapsed());
    let during = o.frames.load(Ordering::Relaxed);

    // The network returns. The transport must not hang: it either resumes
    // or closes, in which case the session layer reconnects (covered by the
    // engine tests; its 8 s control-channel timeout usually fires first).
    sender.set_impairment(None);
    receiver.set_impairment(None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !o.resumed.load(Ordering::Relaxed) && !o.closed.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline, "transport neither resumed nor closed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if o.resumed.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(o.frames.load(Ordering::Relaxed) > during + 20, "frames must flow again after resuming");
    }
    stop.store(true, Ordering::Relaxed);
}
