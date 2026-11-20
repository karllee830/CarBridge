// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The phone's side against a sink written from the protocol description.

use std::net::Ipv4Addr;

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use super::*;

const SINK_PARAMETERS: &str = "wfd_car_display_mode: mode=0;display=2560:1600;dpi=320;fps=60\r\n\
    wfd_video_formats: 28 00 01 01 ffffffff ffffffff ffffffff 00 0000 0000 00 none none\r\n\
    wfd_audio_codecs: AAC 0000000F 00\r\n\
    wfd_client_rtp_ports: RTP/AVP/TCP;unicast 19000 0 mode=play\r\n\
    wfd_audio_latency: 400\r\n";

async fn expect_request(sink: &mut Rtsp, method: &str) -> Message {
    let message = sink.recv().await.unwrap();
    match &message {
        Message::Request { method: m, .. } if m == method => message,
        other => panic!("expected {method}, got {other:?}"),
    }
}

async fn expect_ok(sink: &mut Rtsp) -> Message {
    let message = sink.recv().await.unwrap();
    assert!(
        matches!(message, Message::Response { status: 200, .. }),
        "{message:?}"
    );
    message
}

/// Runs the sink's side up to PLAY; returns the RTSP connection and the RTP connection the phone opened.
async fn sink(phone_rtsp: SocketAddr) -> (Rtsp, TcpStream, Vec<String>) {
    let rtp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let rtp_port = rtp_listener.local_addr().unwrap().port();
    let mut sink = Rtsp::new(TcpStream::connect(phone_rtsp).await.unwrap());
    let mut bodies = Vec::new();

    let m1 = expect_request(&mut sink, "OPTIONS").await;
    assert_eq!(m1.headers().get("Require"), Some("org.wfa.wfd1.0"));
    sink.ok(
        &m1,
        &[(
            "Public",
            "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER".into(),
        )],
    )
    .await
    .unwrap();
    sink.request("OPTIONS", "*", None).await.unwrap();
    let m2 = expect_ok(&mut sink).await;
    assert!(m2.headers().get("Public").unwrap().contains("SETUP"));

    let m3 = expect_request(&mut sink, "GET_PARAMETER").await;
    assert!(m3.body().contains("wfd_video_formats"));
    let mut answer =
        Message::ok(m3.headers().cseq().unwrap()).with_parameters(SINK_PARAMETERS.into());
    answer.headers_mut().push("X", "y");
    sink.send(&answer).await.unwrap();

    let m4 = expect_request(&mut sink, "SET_PARAMETER").await;
    bodies.push(m4.body().to_owned());
    sink.ok(&m4, &[]).await.unwrap();
    let m5 = expect_request(&mut sink, "SET_PARAMETER").await;
    bodies.push(m5.body().to_owned());
    sink.ok(&m5, &[]).await.unwrap();

    sink.cseq += 1;
    let mut setup = Message::request("SETUP", "rtsp://127.0.0.1/wfd1.0/streamid=0", sink.cseq);
    setup.headers_mut().push(
        "Transport",
        format!("RTP/AVP/TCP;unicast;client_port={rtp_port}"),
    );
    sink.send(&setup).await.unwrap();
    let (rtp, _) = rtp_listener.accept().await.unwrap();
    let setup_ok = expect_ok(&mut sink).await;
    assert_eq!(
        setup_ok.headers().get("Session"),
        Some("4d7a1c2e;timeout=30")
    );
    assert!(setup_ok
        .headers()
        .get("Transport")
        .unwrap()
        .ends_with("server_port=19000-19001"));

    sink.request("PLAY", "rtsp://x.x.x.x:x/wfd1.0/streamid=0", None)
        .await
        .unwrap();
    expect_ok(&mut sink).await;
    (sink, rtp, bodies)
}

#[tokio::test]
async fn negotiation_and_stream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sink = tokio::spawn(sink(addr));
    let (stream, _) = listener.accept().await.unwrap();
    let mut offered = None;
    let cast = negotiate(
        stream,
        &Choices::new(Ipv4Addr::new(192, 168, 49, 117)),
        |offers| {
            offered = Some(offers);
            Codec::H265
        },
    )
    .await
    .unwrap();
    let (mut sink, mut rtp, bodies) = sink.await.unwrap();

    assert!(cast
        .sink_parameters
        .iter()
        .any(|(name, value)| name == "wfd_audio_latency" && value == "400"));
    assert!(!cast.offers_hevc());
    assert_eq!(offered, Some(false));
    assert_eq!(cast.codec, Codec::H265);
    assert!(bodies[0]
        .contains("wfd_presentation_URL: rtsp://192.168.49.117:7236/wfd1.0/streamid=0 none\r\n"));
    assert!(bodies[0].contains("port=4321;uibc_encrypted=true"));
    assert!(!bodies[0].contains("ovm_control"));
    assert!(bodies[0].contains("iccoa_support_hevc: supported\r\n"));
    assert_eq!(bodies[1], "wfd_trigger_method: SETUP\r\n");

    let mut streamer = Streamer::new(cast.rtp, Codec::H264, false);
    let mut frame = vec![0, 0, 0, 1, 0x65, 0x88];
    frame.extend(vec![0x11; 3000]);
    streamer.video(&frame, 9000, true).await.unwrap();
    let mut len = [0u8; 2];
    rtp.read_exact(&mut len).await.unwrap();
    let mut packet = vec![0u8; usize::from(u16::from_be_bytes(len))];
    rtp.read_exact(&mut packet).await.unwrap();
    assert_eq!(packet[1], 33);
    assert_eq!(packet[12], 0x47);
    assert_eq!(packet.len(), 12 + 7 * 188);

    let (events_tx, mut events) = mpsc::channel(4);
    let rtsp = tokio::spawn(Cast::serve_rtsp(cast.rtsp, events_tx));
    sink.request(
        "SET_PARAMETER",
        CONTROL_URI,
        Some("wfd_idr_request\r\n".into()),
    )
    .await
    .unwrap();
    expect_ok(&mut sink).await;
    assert_eq!(events.recv().await, Some(SinkEvent::IdrRequest));
    drop(sink);
    assert_eq!(rtsp.await.unwrap().unwrap(), SinkEvent::Closed);
}

#[test]
fn transport_ports() {
    assert_eq!(
        client_port("RTP/AVP/TCP;unicast;client_port=15550"),
        Some(15550)
    );
    assert_eq!(
        client_port("RTP/AVP/UDP;unicast;client_port=15550-15551"),
        Some(15550)
    );
    assert_eq!(client_port("RTP/AVP/TCP;interleaved=0-1"), None);
}

#[test]
fn video_modes() {
    let mode = |width, height, fps| VideoMode { width, height, fps };
    // 1280x720p30 and p60, as head units name them in M3.
    assert_eq!(
        mode(1280, 720, 30).format(),
        "28 00 01 01 00000020 00000000 00000000 00 0000 0000 00 none none"
    );
    assert_eq!(
        mode(1280, 720, 60).format(),
        "30 00 01 02 00000040 00000000 00000000 00 0000 0000 00 none none"
    );
    assert_eq!(mode(1280, 720, 45).timing(), 60);
    assert_eq!(mode(1280, 720, 25).timing(), 30);
    // A head unit's cast area: a level that holds it.
    assert_eq!(mode(2458, 1484, 30).level(), (0x20, "5"));
    assert_eq!(mode(2458, 1176, 30).level(), (0x20, "5"));
    assert_eq!(mode(2458, 1176, 60).level(), (0x40, "5.1"));
    assert_eq!(mode(1920, 1080, 30).level(), (0x04, "4"));
    assert_eq!(mode(1920, 1080, 60).level(), (0x10, "4.2"));
    assert_eq!(mode(3840, 2160, 60).level(), (0x80, "5.2"));
    assert_eq!(mode(7680, 4320, 60).level(), (0x80, "5.2"));
    // A long, low picture needs the level for its longest side.
    assert_eq!(mode(3840, 240, 30).level(), (0x04, "4"));
    assert!(mode(2458, 1484, 30)
        .format()
        .starts_with("28 00 01 20 00000020 "));
    assert_eq!(
        mode(2458, 1484, 30).to_string(),
        "1280x720p30, H.264 level 5 for 2458x1484 at 30 fps"
    );
    assert_eq!(
        parameters(&format!(
            "wfd_video_formats: {}\r\n",
            mode(1280, 720, 60).format()
        ))[0]
            .1,
        mode(1280, 720, 60).format()
    );
}

#[test]
fn m4_chooses_hevc() {
    let choices = Choices::new(Ipv4Addr::new(192, 168, 49, 117));
    assert!(!choices.m4(Codec::H264).contains("iccoa_support_hevc"));
    let m4 = choices.m4(Codec::H265);
    assert!(m4.contains("\r\niccoa_support_hevc: supported\r\n"));
    assert!(m4.contains("\r\nwfd_car_display_mode: mode=0;display=1280:720;dpi=320;fps=30\r\n"));
    assert!(m4.contains("\r\nwfd_standby_resume_capability: none\r\n"));
    assert_eq!(parameters(&m4).len(), 10);
    assert_eq!(parameters(&choices.m4(Codec::H264)).len(), 7);
    assert!(offers_hevc(&parameters(&m4)));
    assert!(!offers_hevc(&parameters(SINK_PARAMETERS)));
}

#[test]
fn stalls_counted_by_length() {
    let stalls = Stalls::default();
    for ms in [0, 49, 50, 99, 100, 199, 200, 499, 500, 2000] {
        stalls.add(Duration::from_millis(ms));
    }
    assert_eq!(
        stalls.take(),
        StallCounts {
            counts: [2, 2, 2, 2],
            longest_ms: 2000
        }
    );
    // Taking starts the counts again.
    assert!(stalls.take().is_empty());
    stalls.add(Duration::from_millis(10));
    assert_eq!(stalls.take(), StallCounts::default());
}
