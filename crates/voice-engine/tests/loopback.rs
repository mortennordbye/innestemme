//! End-to-end transport test: a UDP client streams PCM with loss, reordering and a duplicate
//! through a passthrough server and checks what comes back.
//!
//! `AllocDisabler` makes any allocation inside the server's `no_alloc` sections abort the test
//! process, so a pass also proves the packet path and the model-thread loop do not allocate.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;
use voice_engine::{serve, Config, Metrics, Passthrough, UdpTransport};
use voice_proto::{pcm_from_bytes, pcm_to_bytes, Codec, Header, Kind, HEADER_LEN, MAX_DATAGRAM, PACKET_SAMPLES};

#[global_allocator]
static ALLOC: voice_rt::noalloc::AllocDisabler = voice_rt::noalloc::AllocDisabler;

const SESSION: u16 = 7;
const PACKETS: u32 = 100;

fn packet_pcm(seq: u32) -> [i16; PACKET_SAMPLES] {
    std::array::from_fn(|i| (seq as usize * PACKET_SAMPLES + i) as i16)
}

fn datagram(kind: Kind, seq: u32) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_LEN];
    Header { kind, codec: Codec::PcmS16, session: SESSION, seq, ts_samples: seq * PACKET_SAMPLES as u32 }
        .write(&mut buf);
    if kind == Kind::Audio {
        buf.resize(HEADER_LEN + PACKET_SAMPLES * 2, 0);
        pcm_to_bytes(&packet_pcm(seq), &mut buf[HEADER_LEN..]);
    }
    buf
}

#[tokio::test(flavor = "current_thread")]
async fn loopback_with_loss_and_reorder() {
    let metrics = Arc::new(Metrics::default());
    let transport = UdpTransport::bind("127.0.0.1:0".parse().unwrap(), 1 << 20, None).unwrap();
    let server_addr = transport.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(transport, Box::new(Passthrough), Config::default(), metrics.clone(), async {
        let _ = stop_rx.await;
    }));

    let client = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    client.connect(server_addr).await.unwrap();
    let mut rx = [0u8; MAX_DATAGRAM];

    client.send(&datagram(Kind::Hello, 0)).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut rx)).await.expect("hello reply").unwrap();
    let (hello, _) = Header::parse(&rx[..n]).unwrap();
    assert_eq!((hello.kind, hello.session), (Kind::Hello, SESSION));
    // The server refuses audio until its model thread has acknowledged the new session.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 5% loss, every 7th pair swapped, one duplicate.
    let lost = |seq: u32| seq % 20 == 13;
    let mut order: Vec<u32> = (0..PACKETS).filter(|&s| !lost(s)).collect();
    for i in (0..order.len() - 1).step_by(7) {
        order.swap(i, i + 1);
    }
    order.insert(50, order[49]);
    // Sent from its own task so the replies are read as they arrive. Left unread until the end,
    // 100 datagrams overflow Linux's default socket receive buffer and the kernel drops the tail.
    let sender = tokio::spawn({
        let client = client.clone();
        async move {
            for seq in order {
                client.send(&datagram(Kind::Audio, seq)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            client.send(&datagram(Kind::Bye, PACKETS)).await.unwrap();
        }
    });

    let mut received = BTreeMap::new();
    while received.len() < PACKETS as usize {
        let n = timeout(Duration::from_secs(2), client.recv(&mut rx))
            .await
            .unwrap_or_else(|_| panic!("timed out with {} of {PACKETS} packets", received.len()))
            .unwrap();
        let (header, payload) = Header::parse(&rx[..n]).unwrap();
        assert_eq!(header.kind, Kind::Audio);
        assert_eq!(header.ts_samples, header.seq * PACKET_SAMPLES as u32);
        let mut pcm = [0i16; PACKET_SAMPLES];
        assert_eq!(pcm_from_bytes(payload, &mut pcm), PACKET_SAMPLES);
        assert!(received.insert(header.seq, pcm).is_none(), "output seq {} repeated", header.seq);
    }

    sender.await.unwrap();

    // Output packet k carries input packet k: bit-exact when it arrived, silence when it was lost.
    for (seq, pcm) in &received {
        let expected = if lost(*seq) { [0; PACKET_SAMPLES] } else { packet_pcm(*seq) };
        assert!(*pcm == expected, "packet {seq}: first sample {} expected {}", pcm[0], expected[0]);
    }

    assert_eq!(metrics.lost.load(Relaxed), 5);
    assert_eq!(metrics.duplicate.load(Relaxed) + metrics.late.load(Relaxed), 1);
    assert_eq!(metrics.frames.load(Relaxed), u64::from(PACKETS) / 4);
    assert_eq!(metrics.overruns.load(Relaxed), 0);
    assert_eq!(metrics.process_errors.load(Relaxed), 0);
    assert_eq!(metrics.frame_total.count(), u64::from(PACKETS) / 4);
    let p99 = metrics.frame_total.quantile_upper_bound(0.99).expect("latency samples");
    assert!(p99 <= Duration::from_millis(50), "server-added latency p99 bucket {p99:?}");

    // A second client is refused while the first session is merely closing? No: closing frees the slot.
    let intruder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    intruder.connect(server_addr).await.unwrap();
    intruder.send(&datagram(Kind::Hello, 0)).await.unwrap();
    let n = timeout(Duration::from_secs(2), intruder.recv(&mut rx)).await.expect("second hello").unwrap();
    assert_eq!(Header::parse(&rx[..n]).unwrap().0.kind, Kind::Hello);
    // The first client no longer owns the session, so its audio is rejected.
    let rejected = metrics.rejected.load(Relaxed);
    client.send(&datagram(Kind::Audio, 0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(metrics.rejected.load(Relaxed), rejected + 1);

    assert!(metrics.render().contains("voice_frames_total 25"));
    stop_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}
