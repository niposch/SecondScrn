//! HEVC access units as the Windows encoder produces them (AUD, parameter
//! sets on keyframes, pictures in several large slices) must arrive as exactly one frame
//! each, byte for byte. A frame split into several pieces makes the decoder
//! treat each piece as a picture: ghost cursors and smeared blocks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use secondscrn_protocol::Codec;
use secondscrn_transport::*;

fn nal(header: [u8; 2], len: usize, seed: u8) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, header[0], header[1]];
    // Avoid start-code emulation in the payload.
    v.extend((0..len).map(|i| ((i as u8).wrapping_mul(7).wrapping_add(seed)) | 0x10));
    v
}

fn access_unit(i: u64, key: bool, slice: usize) -> Vec<u8> {
    let mut f = nal([0x46, 0x01], 1, 0x50); // AUD
    if key {
        f.extend(nal([0x40, 0x01], 20, 1)); // VPS
        f.extend(nal([0x42, 0x01], 40, 2)); // SPS
        f.extend(nal([0x44, 0x01], 8, 3)); // PPS
        f.extend(nal([0x26, 0x01], slice * 4, i as u8)); // IDR_W_RADL
    } else {
        // Two slices per picture, as NVENC produces at faster presets.
        f.extend(nal([0x02, 0x01], slice, i as u8)); // TRAIL_R
        f.extend(nal([0x02, 0x01], slice, i as u8 ^ 0x55)); // TRAIL_R, second slice
    }
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hevc_access_units_arrive_whole() {
    let (pending, offer) =
        PendingSender::create(SenderConfig { codec: Codec::H265, interfaces: InterfacePolicy::loopback_only() })
            .await
            .unwrap();
    let (mut receiver, answer) = accept_offer(
        ReceiverConfig {
            codec: Codec::H265,
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
    let frames = sender.frame_sender();
    let start = Instant::now();
    let mut sent = Vec::new();
    for i in 0..60u64 {
        // Mix of small (cursor move) and large (fragmented) slices.
        // As the host sends them: without access unit delimiters.
        let data = secondscrn_graphics::bitstream::strip_delimiters(
            Codec::H265,
            &access_unit(i, i == 0, if i % 3 == 0 { 30_000 } else { 400 }),
        );
        sent.push(data.clone());
        frames.send_frame(EncodedFrame {
            data: Arc::from(data),
            capture_time: start + Duration::from_millis(i * 16),
            keyframe: i == 0,
        });
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && got.len() < sent.len() + 10 {
        match tokio::time::timeout(Duration::from_millis(300), receiver.events.recv()).await {
            Ok(Some(TransportEvent::Frame(f))) => got.push(f.data.to_vec()),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert_eq!(
        got.len(),
        sent.len(),
        "frames received vs sent (sizes: {:?})",
        got.iter().map(|g| g.len()).collect::<Vec<_>>()
    );
    for (i, (g, s)) in got.iter().zip(&sent).enumerate() {
        assert_eq!(g, s, "frame {i} differs ({} vs {} bytes)", g.len(), s.len());
    }
}
