use crate::audio::AudioCapture;
use crate::config::AppConfig;
use crate::logger;
use crate::voice_audio::{AudioPlayback, PlaybackSink};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use opus::{Application, Channels, Decoder, Encoder};
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState, RTCSessionDescription,
};

const AUDIO_RATE: u32 = 48_000;
const FRAME_SAMPLES: usize = 960;
const MAX_OPUS_PACKET: usize = 1_500;

#[derive(Debug)]
pub enum VoiceEvent {
    Connected,
    Error(String),
    Stopped,
}

pub struct VoiceHandle {
    stop_tx: watch::Sender<bool>,
    events: Receiver<VoiceEvent>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceHandle {
    pub fn start(config: AppConfig) -> Result<Self> {
        config.validate_for_voice()?;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (event_tx, events) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("ashe-voice".to_string())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to start voice runtime")
                    .and_then(|runtime| {
                        runtime.block_on(run_voice(config, stop_rx, event_tx.clone()))
                    });
                if let Err(error) = result {
                    let _ = event_tx.send(VoiceEvent::Error(format!("{error:#}")));
                }
                let _ = event_tx.send(VoiceEvent::Stopped);
            })
            .context("failed to spawn voice thread")?;
        Ok(Self {
            stop_tx,
            events,
            thread: Some(thread),
        })
    }

    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    pub fn next_event(&self) -> Option<VoiceEvent> {
        self.events.try_recv().ok()
    }

    pub fn finish(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone)]
struct Handler {
    gathering: mpsc::UnboundedSender<()>,
    states: mpsc::UnboundedSender<RTCPeerConnectionState>,
    playback: PlaybackSink,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathering.send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.states.send(state);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let sink = self.playback.clone();
        tokio::spawn(async move {
            if let Err(error) = receive_audio(track, sink).await {
                logger::info(format!("Voice receive track stopped: {error:#}"));
            }
        });
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionAnswer {
    session_id: String,
    answer_sdp: String,
}

async fn run_voice(
    config: AppConfig,
    mut stop: watch::Receiver<bool>,
    events: Sender<VoiceEvent>,
) -> Result<()> {
    let (playback, sink) = AudioPlayback::start()?;
    let _playback = playback;
    let (gather_tx, mut gather_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let handler = Arc::new(Handler {
        gathering: gather_tx,
        states: state_tx,
        playback: sink,
    });

    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_string(),
            clock_rate: AUDIO_RATE,
            channels: 2,
            sdp_fmtp_line: "".to_string(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
    };
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(codec.clone(), RtpCodecKind::Audio)?;
    let interceptors = register_default_interceptors(Registry::new(), &mut media_engine)?;
    let peer = PeerConnectionBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(interceptors)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .context("failed to create voice peer")?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(35))
        .build()
        .context("failed to create assistant HTTP client")?;
    let mut active_session: Option<String> = None;
    let result = async {
        if *stop.borrow() { return Ok(()); }
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(), MediaStreamTrack::new(
                "ashe-voice".to_string(), "microphone".to_string(),
                "Ashe microphone".to_string(), RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(rand::random()), ..Default::default()
                    },
                    codec: codec.rtp_codec,
                    ..Default::default()
                }],
            ),
        )?);
        let sender = peer.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        let _events_channel = peer.create_data_channel("oai-events", None).await?;
        let offer = peer.create_offer(None).await?;
        peer.set_local_description(offer).await?;
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            gathered = tokio::time::timeout(Duration::from_secs(15), gather_rx.recv()) => {
                gathered.context("voice ICE gathering timed out")?
                    .ok_or_else(|| anyhow!("voice ICE gathering stopped"))?;
            }
        }
        if *stop.borrow() { return Ok(()); }
        let sdp = peer.local_description().await
            .ok_or_else(|| anyhow!("voice offer has no local description"))?.sdp;
        // Keep signaling alive until we know whether a server session exists.
        // Cancelling on stop could strand a session we cannot name.
        let answer = create_session(&client, &config, &sdp).await?;
        active_session = Some(answer.session_id);
        if *stop.borrow() { return Ok(()); }
        peer.set_remote_description(RTCSessionDescription::answer(answer.answer_sdp)?)
            .await.context("voice answer could not be applied")?;

        let connected = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(false),
                    state = state_rx.recv() => match state {
                        Some(RTCPeerConnectionState::Connected) => return Ok(true),
                        Some(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) | None =>
                            return Err(anyhow!("voice peer connection failed")),
                        _ => {}
                    }
                }
            }
        }).await.context("voice connection timed out")??;
        if !connected { return Ok(()); }
        let _ = events.send(VoiceEvent::Connected);

        let payload_type = sender.get_parameters().await?.rtp_parameters.codecs.first()
            .ok_or_else(|| anyhow!("voice sender has no negotiated codec"))?.payload_type;
        let ssrc = *track.ssrcs().await.first()
            .ok_or_else(|| anyhow!("voice sender has no SSRC"))?;
        let (pcm_tx, pcm_rx) = mpsc::channel(12);
        let mut capture = AudioCapture::start_bounded(pcm_tx, AUDIO_RATE)?;
        let outcome = send_audio(track, ssrc, payload_type, pcm_rx, &mut stop, &mut state_rx).await;
        capture.stop();
        outcome
    }.await;

    let _ = peer.close().await;
    if let Some(session) = active_session {
        let endpoint = config.worker_endpoint(&format!("/v1/assistant/sessions/{session}"))?;
        let _ = client
            .delete(endpoint)
            .bearer_auth(&config.assistant_token)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    }
    result
}

async fn create_session(
    client: &reqwest::Client,
    config: &AppConfig,
    sdp: &str,
) -> Result<SessionAnswer> {
    let endpoint = config.worker_endpoint("/v1/assistant/sessions")?;
    let response = client
        .post(&endpoint)
        .bearer_auth(&config.assistant_token)
        .json(&serde_json::json!({ "sdp": sdp }))
        .send()
        .await
        .context("assistant signaling failed")?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        return Err(anyhow!("assistant already has an active voice session"));
    }
    if response.status() != reqwest::StatusCode::CREATED {
        return Err(anyhow!(
            "assistant signaling returned HTTP {}",
            response.status()
        ));
    }
    let answer: SessionAnswer = response
        .json()
        .await
        .context("assistant signaling returned invalid answer")?;
    if !valid_session_id(&answer.session_id) {
        return Err(anyhow!("assistant returned an invalid session identifier"));
    }
    Ok(answer)
}

fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

async fn send_audio(
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut pcm_rx: mpsc::Receiver<Vec<u8>>,
    stop: &mut watch::Receiver<bool>,
    states: &mut mpsc::UnboundedReceiver<RTCPeerConnectionState>,
) -> Result<()> {
    let mut encoder = Encoder::new(AUDIO_RATE, Channels::Mono, Application::Voip)?;
    let mut pending = VecDeque::<i16>::new();
    let mut packet = [0u8; MAX_OPUS_PACKET];
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            state = states.recv() => match state {
                Some(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) | None =>
                    return Err(anyhow!("voice peer connection closed")),
                _ => {}
            },
            chunk = pcm_rx.recv() => {
                let Some(chunk) = chunk else { return Err(anyhow!("microphone stopped")); };
                for pair in chunk.as_chunks::<2>().0 {
                    pending.push_back(i16::from_le_bytes([pair[0], pair[1]]));
                }
                while pending.len() >= FRAME_SAMPLES {
                    let samples: Vec<i16> = pending.drain(..FRAME_SAMPLES).collect();
                    let length = encoder.encode(&samples, &mut packet)?;
                    track.write_sample(ssrc, payload_type, &Sample {
                        data: Bytes::copy_from_slice(&packet[..length]),
                        duration: Duration::from_millis(20),
                        ..Sample::new(Instant::now())
                    }, &[]).await?;
                }
            }
        }
    }
}

async fn receive_audio(track: Arc<dyn TrackRemote>, sink: PlaybackSink) -> Result<()> {
    if track.kind().await != RtpCodecKind::Audio {
        return Ok(());
    }
    let mut decoder = Decoder::new(AUDIO_RATE, Channels::Mono)?;
    let mut samples = [0f32; 5_760];
    while let Some(event) = track.poll().await {
        match event {
            TrackRemoteEvent::OnRtpPacket(packet) => {
                let count = decoder.decode_float(&packet.payload, &mut samples, false)?;
                sink.push(&samples[..count]);
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
    Ok(())
}
