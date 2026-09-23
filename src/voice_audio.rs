use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SOURCE_RATE: f64 = 48_000.0;
const MAX_BUFFER_SAMPLES: usize = 24_000;
const START_BUFFER_SAMPLES: usize = 960;

#[derive(Clone)]
pub struct PlaybackSink(Arc<Mutex<PlaybackBuffer>>);

impl PlaybackSink {
    pub fn push(&self, samples: &[f32]) {
        if let Ok(mut buffer) = self.0.lock() {
            buffer.push(samples);
        }
    }
}

pub struct AudioPlayback {
    _stream: Stream,
}

impl AudioPlayback {
    pub fn start() -> Result<(Self, PlaybackSink)> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        let supported = device
            .default_output_config()
            .context("failed to query default output config")?;
        let channels = usize::from(supported.channels());
        if channels == 0 {
            return Err(anyhow!("output device has zero channels"));
        }
        let output_rate = f64::from(supported.sample_rate());
        let config = supported.config();
        let sink = PlaybackSink(Arc::new(Mutex::new(PlaybackBuffer::default())));
        let state = sink.0.clone();
        let error = |error| crate::logger::info(format!("Voice output stream error: {error}"));
        let stream = match supported.sample_format() {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    render(data, channels, output_rate, &state, |sample| sample)
                },
                error,
                None,
            ),
            SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    render(data, channels, output_rate, &state, |sample| {
                        (sample.clamp(-1.0, 1.0) * 32767.0) as i16
                    })
                },
                error,
                None,
            ),
            SampleFormat::U16 => device.build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    render(data, channels, output_rate, &state, |sample| {
                        ((sample.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16
                    })
                },
                error,
                None,
            ),
            format => return Err(anyhow!("unsupported output sample format: {format:?}")),
        }
        .context("failed to build output stream")?;
        stream.play().context("failed to start output stream")?;
        Ok((Self { _stream: stream }, sink))
    }
}

fn render<T: Copy + Default>(
    output: &mut [T],
    channels: usize,
    output_rate: f64,
    state: &Arc<Mutex<PlaybackBuffer>>,
    convert: impl Fn(f32) -> T,
) {
    if let Ok(mut buffer) = state.lock() {
        for frame in output.chunks_mut(channels) {
            let sample = convert(buffer.next(SOURCE_RATE / output_rate));
            frame.fill(sample);
        }
    } else {
        output.fill(convert(0.0));
    }
}

#[derive(Default)]
struct PlaybackBuffer {
    queue: VecDeque<f32>,
    current: f32,
    following: f32,
    phase: f64,
    primed: bool,
}

impl PlaybackBuffer {
    fn push(&mut self, samples: &[f32]) {
        self.queue.extend(samples.iter().copied());
        while self.queue.len() > MAX_BUFFER_SAMPLES {
            self.queue.pop_front();
        }
    }

    fn next(&mut self, ratio: f64) -> f32 {
        if !self.primed {
            if self.queue.len() < START_BUFFER_SAMPLES {
                return 0.0;
            }
            self.current = self.queue.pop_front().unwrap_or_default();
            self.following = self.queue.pop_front().unwrap_or_default();
            self.phase = 0.0;
            self.primed = true;
        }
        let sample = self.current + (self.following - self.current) * self.phase as f32;
        self.phase += ratio;
        while self.phase >= 1.0 {
            self.current = self.following;
            if let Some(next) = self.queue.pop_front() {
                self.following = next;
            } else {
                self.primed = false;
                self.following = 0.0;
                break;
            }
            self.phase -= 1.0;
        }
        sample
    }
}
