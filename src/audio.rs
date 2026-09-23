use crate::logger;
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use tokio::sync::mpsc::{Sender, UnboundedSender};

enum AudioSink {
    Unbounded(UnboundedSender<Vec<u8>>),
    Bounded(Sender<Vec<u8>>),
}

impl AudioSink {
    fn send(&self, chunk: Vec<u8>) {
        match self {
            Self::Unbounded(sender) => { let _ = sender.send(chunk); }
            Self::Bounded(sender) => { let _ = sender.try_send(chunk); }
        }
    }
}

pub struct AudioCapture {
    stream: Option<Stream>,
    input_sample_rate: u32,
    output_sample_rate: u32,
}

impl AudioCapture {
    pub fn start(sender: UnboundedSender<Vec<u8>>, output_sample_rate: u32) -> Result<Self> {
        Self::start_inner(AudioSink::Unbounded(sender), output_sample_rate)
    }

    pub fn start_bounded(sender: Sender<Vec<u8>>, output_sample_rate: u32) -> Result<Self> {
        Self::start_inner(AudioSink::Bounded(sender), output_sample_rate)
    }

    fn start_inner(sender: AudioSink, output_sample_rate: u32) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default input device"))?;
        #[allow(deprecated)]
        let device_name = device
            .name()
            .unwrap_or_else(|_| "unknown input device".to_string());
        let config = device
            .default_input_config()
            .context("failed to query default input config")?;
        let input_sample_rate = config.sample_rate();
        let channels = config.channels();
        if channels == 0 {
            return Err(anyhow!("input device reported zero channels"));
        }
        if output_sample_rate == 0 {
            return Err(anyhow!("output sample rate must be greater than zero"));
        }

        logger::info(format!(
            "Audio input selected: device={device_name} input_sample_rate={input_sample_rate} output_sample_rate={output_sample_rate} channels={channels} format={:?} resampler={}",
            config.sample_format(),
            if input_sample_rate == output_sample_rate {
                "disabled"
            } else {
                "linear"
            }
        ));

        let stream_config = config.config();
        let err_fn = |err| logger::info(format!("Audio stream error: {err}"));
        let stream = match config.sample_format() {
            SampleFormat::F32 => {
                let mut converter =
                    AudioConverter::new(channels, input_sample_rate, output_sample_rate);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _| converter.send_f32(data, &sender),
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut converter =
                    AudioConverter::new(channels, input_sample_rate, output_sample_rate);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| converter.send_i16(data, &sender),
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut converter =
                    AudioConverter::new(channels, input_sample_rate, output_sample_rate);
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _| converter.send_u16(data, &sender),
                    err_fn,
                    None,
                )
            }
            other => return Err(anyhow!("Unsupported sample format: {other:?}")),
        }
        .context("failed to build input stream")?;

        stream.play().context("failed to start input stream")?;
        logger::info("Audio capture started");
        Ok(Self {
            stream: Some(stream),
            input_sample_rate,
            output_sample_rate,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    pub fn stop(&mut self) {
        self.stream.take();
        logger::info(format!(
            "Audio capture stopped input_sample_rate={} output_sample_rate={}",
            self.input_sample_rate, self.output_sample_rate
        ));
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if self.stream.is_some() {
            self.stop();
        }
    }
}

struct AudioConverter {
    channels: usize,
    input_sample_rate: u32,
    output_sample_rate: u32,
    previous_sample: Option<f32>,
    position: f64,
}

impl AudioConverter {
    fn new(channels: u16, input_sample_rate: u32, output_sample_rate: u32) -> Self {
        Self {
            channels: channels as usize,
            input_sample_rate,
            output_sample_rate,
            previous_sample: None,
            position: 0.0,
        }
    }

    fn send_f32(&mut self, data: &[f32], sender: &AudioSink) {
        let mono = self.mix_to_mono(data.iter().copied());
        self.send_mono(mono, sender);
    }

    fn send_i16(&mut self, data: &[i16], sender: &AudioSink) {
        let mono = self.mix_to_mono(data.iter().map(|sample| *sample as f32 / 32768.0));
        self.send_mono(mono, sender);
    }

    fn send_u16(&mut self, data: &[u16], sender: &AudioSink) {
        let mono = self.mix_to_mono(
            data.iter()
                .map(|sample| (*sample as f32 - 32768.0) / 32768.0),
        );
        self.send_mono(mono, sender);
    }

    fn mix_to_mono<I>(&self, samples: I) -> Vec<f32>
    where
        I: IntoIterator<Item = f32>,
    {
        let mut mono = Vec::new();
        let mut frame = Vec::with_capacity(self.channels);
        for sample in samples {
            frame.push(sample);
            if frame.len() == self.channels {
                mono.push(frame.iter().sum::<f32>() / self.channels as f32);
                frame.clear();
            }
        }
        mono
    }

    fn send_mono(&mut self, mono: Vec<f32>, sender: &AudioSink) {
        if mono.is_empty() {
            return;
        }

        let samples = if self.input_sample_rate == self.output_sample_rate {
            mono
        } else {
            self.resample(mono)
        };
        let mut chunk = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            let sample = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            chunk.extend_from_slice(&sample.to_le_bytes());
        }
        if !chunk.is_empty() {
            sender.send(chunk);
        }
    }

    fn resample(&mut self, mono: Vec<f32>) -> Vec<f32> {
        let mut source = Vec::with_capacity(mono.len() + 1);
        if let Some(previous_sample) = self.previous_sample {
            source.push(previous_sample);
        } else if let Some(first) = mono.first().copied() {
            source.push(first);
        }
        source.extend(mono);

        if source.len() < 2 {
            self.previous_sample = source.last().copied();
            return Vec::new();
        }

        let ratio = self.input_sample_rate as f64 / self.output_sample_rate as f64;
        let mut out = Vec::new();
        while self.position + 1.0 < source.len() as f64 {
            let index = self.position.floor() as usize;
            let frac = (self.position - index as f64) as f32;
            let a = source[index];
            let b = source[index + 1];
            out.push(a + (b - a) * frac);
            self.position += ratio;
        }

        let consumed = self.position.floor() as usize;
        let keep_index = consumed.min(source.len() - 1);
        self.previous_sample = Some(source[keep_index]);
        self.position -= keep_index as f64;
        out
    }
}
