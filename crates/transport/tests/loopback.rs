//! End-to-end WebRTC session over loopback: ICE, DTLS-SRTP, packetisation,
//! keyframe requests and bandwidth estimation, without any media hardware.

use std::sync::Arc;
use std::time::{Duration, Instant};

use secondscrn_protocol::Codec;
use secondscrn_transport::*;

/// Splits an Annex-B stream into NAL payloads (start codes removed).
fn nals(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start = None;
    while i + 3 <= data.len() {
        let sc3 = data[i..i + 3] == [0, 0, 1];
        if sc3 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(data[s..end].to_vec());
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(data[s..].to_vec());
    }
    out
}

fn h264_frame(index: u32, keyframe: bool, size: usize) -> Vec<u8> {
    let mut f = Vec::new();
    if keyframe {
        f.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac]); // SPS
        f.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80]); // PPS
        f.extend_from_slice(&[0, 0, 0, 1, 0x65]); // IDR slice
    } else {
        f.extend_from_slice(&[0, 0, 0, 1, 0x41]); // non-IDR slice
    }
    // Payload bytes must avoid accidental start codes.
    f.extend((0..size).map(|i| ((i as u32 + index) % 250 + 1) as u8));
    f
}

async fn connect(codec: Codec) -> (MediaHandle, MediaHandle) {
    let (pending, offer) =
        PendingSender::create(SenderConfig { codec, interfaces: InterfacePolicy::loopback_only() }).await.unwrap();
    assert!(!candidate_ports(&offer).is_empty(), "offer must carry candidates");

    let (receiver, answer) = accept_offer(
        ReceiverConfig { codec, interfaces: InterfacePolicy::loopback_only(), host_hint: Some([127, 0, 0, 1].into()) },
        &offer,
    )
    .await
    .unwrap();
    let sender = pending.accept_answer(&answer).unwrap();
    (sender, receiver)
}

async fn wait_connected(h: &mut MediaHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline.into(), h.events.recv()).await.expect("connect timeout");
        match ev {
            Some(TransportEvent::Connected { .. }) => return,
            Some(TransportEvent::Closed { reason }) => panic!("closed: {reason}"),
            Some(_) => {}
            None => panic!("event stream ended"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_frames_arrive_intact() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let (mut sender, mut receiver) = connect(Codec::H264).await;
    wait_connected(&mut sender).await;
    wait_connected(&mut receiver).await;

    let start = Instant::now();
    let mut sent = Vec::new();
    for i in 0..30u32 {
        // Large keyframe exercises FU-A fragmentation.
        let frame = h264_frame(i, i == 0, if i == 0 { 60_000 } else { 3_000 });
        sent.push(frame.clone());
        assert!(sender.send_frame(EncodedFrame {
            data: Arc::from(frame),
            capture_time: start + Duration::from_millis(16 * i as u64),
            keyframe: i == 0,
        }));
        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while received.len() < sent.len() {
        match tokio::time::timeout_at(deadline.into(), receiver.events.recv()).await {
            Ok(Some(TransportEvent::Frame(f))) => received.push(f),
            Ok(Some(TransportEvent::Closed { reason })) => panic!("closed: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) => panic!("receiver ended"),
            Err(_) => break,
        }
    }
    assert_eq!(received.len(), sent.len(), "all frames should arrive on loopback");
    assert!(received[0].keyframe, "first frame is detected as keyframe");
    assert!(received.iter().skip(1).all(|f| !f.keyframe));
    for (s, r) in sent.iter().zip(&received) {
        assert_eq!(nals(s), nals(&r.data), "payload mismatch");
    }
    // 16 ms apart at 90 kHz = 1440 ticks.
    let dt = received[1].rtp_time.wrapping_sub(received[0].rtp_time);
    assert!((1400..=1480).contains(&dt), "rtp delta {dt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyframe_requests_reach_the_sender() {
    let (mut sender, mut receiver) = connect(Codec::H265).await;
    wait_connected(&mut sender).await;
    wait_connected(&mut receiver).await;

    // The receiver needs to have seen the sender's SSRC before it can send PLI.
    let start = Instant::now();
    for i in 0..5u32 {
        let mut f = vec![0, 0, 0, 1, 0x26, 0x01]; // HEVC IDR_W_RADL
        f.extend((0..500).map(|j| ((j + i) % 200 + 1) as u8));
        sender.send_frame(EncodedFrame {
            data: Arc::from(f),
            capture_time: start + Duration::from_millis(16 * i as u64),
            keyframe: true,
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    receiver.request_keyframe();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline.into(), sender.events.recv()).await {
            Ok(Some(TransportEvent::KeyframeRequested)) => break,
            Ok(Some(_)) => {}
            other => panic!("no keyframe request: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_sender_closes_the_receiver() {
    let (mut sender, mut receiver) = connect(Codec::Av1).await;
    wait_connected(&mut sender).await;
    wait_connected(&mut receiver).await;
    drop(sender);
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        match tokio::time::timeout_at(deadline.into(), receiver.events.recv()).await {
            Ok(Some(TransportEvent::Closed { .. })) | Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => panic!("receiver did not notice the sender going away"),
        }
    }
}
