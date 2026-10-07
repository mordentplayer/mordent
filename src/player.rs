//! Playback: symphonia decode → rodio output, plus a decode-only fallback
//! for machines without an audio device.

use anyhow::{anyhow, Result};
use std::time::Duration;

/// A seekable-ish media stream decodable by symphonia.
pub struct SymphoniaSource {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    buf: symphonia::core::audio::SampleBuffer<f32>,
    spec: symphonia::core::audio::SignalSpec,
    pos: usize,
    sample_rate: u32,
    channels: usize,
    finished: bool,
}

impl SymphoniaSource {
    pub fn new(media: Box<dyn symphonia::core::io::MediaSource>) -> Result<Self> {
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;

        let mss = MediaSourceStream::new(media, Default::default());
        let mut hint = Hint::new();
        hint.with_extension("m4a");
        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let mut format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track"))?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs().make(
            &params,
            &symphonia::core::codecs::DecoderOptions::default(),
        )?;
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params.channels.map(|c| c.count()).unwrap_or(2);
        Ok(Self {
            format,
            decoder,
            track_id,
            buf: symphonia::core::audio::SampleBuffer::<f32>::new(8192, Default::default()),
            spec: Default::default(),
            pos: 0,
            sample_rate,
            channels,
            finished: false,
        })
    }
}

impl Iterator for SymphoniaSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos < self.buf.samples().len() {
            let s = self.buf.samples()[self.pos];
            self.pos += 1;
            return Some(s);
        }
        if self.finished {
            return None;
        }
        loop {
            match self.format.next_packet() {
                Ok(pkt) if pkt.track_id() != self.track_id => continue,
                Ok(pkt) => match self.decoder.decode(&pkt) {
                    Ok(decoded) => {
                        let spec = *decoded.spec();
                        let cap = (decoded.frames().max(1) as usize) * spec.channels.count();
                        if self.buf.capacity() < cap {
                            self.buf = symphonia::core::audio::SampleBuffer::<f32>::new(
                                cap as u64,
                                spec,
                            );
                        } else {
                            self.buf = symphonia::core::audio::SampleBuffer::<f32>::new(
                                self.buf.capacity(),
                                spec,
                            );
                        }
                        self.buf.copy_interleaved_ref(decoded);
                        self.spec = spec;
                        self.pos = 0;
                        if self.buf.samples().is_empty() {
                            continue;
                        }
                        let s = self.buf.samples()[0];
                        self.pos = 1;
                        return Some(s);
                    }
                    Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                    Err(_) => {
                        self.finished = true;
                        return None;
                    }
                },
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.finished = true;
                    return None;
                }
                Err(symphonia::core::errors::Error::ResetRequired) | Err(_) => {
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}

impl rodio::Source for SymphoniaSource {
    fn current_frame_len(&self) -> Option<usize> {
        let remaining = self.buf.samples().len().saturating_sub(self.pos);
        Some(remaining.div_ceil(self.channels.max(1)))
    }

    fn channels(&self) -> u16 {
        self.channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Owns the audio output. Falls back to decode-only mode (progress to
/// stdout) when no device is available — e.g. a headless VPS.
pub struct Player {
    stream: Option<rodio::OutputStream>,
    sink: Option<rodio::Sink>,
    pub decode_only: bool,
}

impl Player {
    pub fn new() -> Self {
        match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => {
                let sink = rodio::Sink::try_new(&handle).ok();
                Self {
                    stream: Some(stream),
                    sink,
                    decode_only: false,
                }
            }
            Err(_) => Self {
                stream: None,
                sink: None,
                decode_only: true,
            },
        }
    }

    pub fn has_output(&self) -> bool {
        self.sink.is_some()
    }

    pub fn sink(&self) -> Option<&rodio::Sink> {
        self.sink.as_ref()
    }

    /// Append a decoded source to the queue.
    pub fn enqueue(&self, src: SymphoniaSource) {
        if let Some(sink) = &self.sink {
            sink.append(src);
        }
    }
}
