//! Playback: ALAC packet feed → symphonia decode → rodio output, with a
//! decode-only fallback for machines without an audio device.

use crate::fairplay;
use anyhow::{anyhow, Result};
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;

/// Decodes a *decrypted, fragmented* MP4 stream (e.g. the live output of
/// [`crate::stream::ProgressiveSource`]) by feeding raw ALAC packets to
/// symphonia. Blocks on reads when the download frontier catches up — that
/// pacing is what streams audio in lockstep with the fetch worker.
pub struct PacketSource {
    reader: Box<dyn Read + Send>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    /// f32 interleaved samples pending output.
    pending: Vec<f32>,
    pos: usize,
    pub sample_rate: u32,
    pub channels: usize,
    pub frames_decoded: u64,
    /// Set on drop so the fetch worker of a skipped source stops early.
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl PacketSource {
    pub fn with_cancel(
        reader: Box<dyn Read + Send>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self> {
        let mut s = Self::new(reader)?;
        s.cancel = Some(cancel);
        Ok(s)
    }

    pub fn new(mut reader: Box<dyn Read + Send>) -> Result<Self> {
        // init: ftyp + moov
        let mut init_raw = Vec::new();
        for _ in 0..2 {
            let (h, payload) = fairplay::read_box_pub(&mut reader)?;
            init_raw.extend_from_slice(&h.assemble_pub(&payload));
        }
        let alac = fairplay::find_alac_cookie(&init_raw)
            .ok_or_else(|| anyhow!("no ALAC cookie in stream init"))?;
        let channels = if alac.channels == 1 {
            symphonia::core::audio::Channels::FRONT_LEFT
        } else {
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT
        };
        let mut params = symphonia::core::codecs::CodecParameters::new();
        params
            .for_codec(symphonia::core::codecs::CODEC_TYPE_ALAC)
            .with_sample_rate(alac.sample_rate)
            .with_channels(channels)
            .with_max_frames_per_packet(alac.frame_len as u64);
        if let Some(x) = fairplay::alac_extra_data(&init_raw) {
            params.with_extra_data(x);
        }
        let decoder = symphonia::default::get_codecs().make(&params, &Default::default())?;
        Ok(Self {
            reader: Box::new(reader),
            decoder,
            pending: Vec::with_capacity(alac.frame_len as usize * 2),
            pos: 0,
            sample_rate: alac.sample_rate,
            channels: alac.channels as usize,
            frames_decoded: 0,
            cancel: None,
        })
    }

    fn decode_more(&mut self) -> bool {
        use symphonia::core::formats::Packet;
        // Reads exactly one fragment: `read_fragment` returns None at EOF, so a
        // single pass feeds one packet batch (the old `loop` never iterated).
        let frag = match fairplay::read_fragment(&mut self.reader) {
            Ok(Some(f)) => f,
            Ok(None) => return false, // clean EOF
            Err(_) => return false,   // treat stream errors as end
        };
        let mut ts = frag.base_time;
        for (i, sample) in frag.samples.iter().enumerate() {
            let dur = (*frag.durations.get(i).unwrap_or(&0)).max(1) as u64;
            let pkt = Packet::new_from_boxed_slice(1, ts, dur, sample.clone().into_boxed_slice());
            ts += dur;
            match self.decoder.decode(&pkt) {
                Ok(decoded) => {
                    let frames = decoded.frames().max(1);
                    let spec = *decoded.spec();
                    let cap = frames * spec.channels.count();
                    let mut buf =
                        symphonia::core::audio::SampleBuffer::<f32>::new(cap as u64, spec);
                    buf.copy_interleaved_ref(decoded);
                    self.pending.extend_from_slice(buf.samples());
                    self.frames_decoded += frames as u64;
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(_) => return false,
            }
        }
        true
    }
}

impl Iterator for PacketSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
            if !self.decode_more() {
                return None;
            }
            if self.pos >= self.pending.len() {
                return None;
            }
        }
        let s = self.pending[self.pos];
        self.pos += 1;
        Some(s)
    }
}

impl rodio::Source for PacketSource {
    fn current_frame_len(&self) -> Option<usize> {
        Some((self.pending.len() - self.pos).div_ceil(self.channels.max(1)))
    }

    fn channels(&self) -> u16 {
        self.channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Owns the audio output. Falls back to decode-only mode (progress to
/// stdout) when no device is available — e.g. a headless VPS.
/// Cheaply cloneable handle (single-threaded; rodio's `OutputStream` is `!Send`).
#[derive(Clone)]
pub struct Player {
    inner: Rc<Inner>,
    pub decode_only: bool,
}

struct Inner {
    // Held only to keep the audio device open: dropping the `OutputStream`
    // tears the device down and stops playback, so it is kept though unread.
    #[allow(dead_code)]
    stream: Option<rodio::OutputStream>,
    sink: Option<rodio::Sink>,
}

impl Player {
    pub fn new() -> Self {
        match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => {
                let sink = rodio::Sink::try_new(&handle).ok();
                Self {
                    inner: Rc::new(Inner {
                        stream: Some(stream),
                        sink,
                    }),
                    decode_only: false,
                }
            }
            Err(_) => Self {
                inner: Rc::new(Inner {
                    stream: None,
                    sink: None,
                }),
                decode_only: true,
            },
        }
    }

    pub fn sink(&self) -> Option<&rodio::Sink> {
        self.inner.sink.as_ref()
    }

    /// Append a decoded source to the queue.
    pub fn enqueue(&self, src: PacketSource) {
        if let Some(sink) = &self.inner.sink {
            sink.append(src);
        }
    }

    pub fn toggle_pause(&self) {
        if let Some(sink) = &self.inner.sink {
            if sink.is_paused() {
                sink.play();
            } else {
                sink.pause();
            }
        }
    }

    pub fn volume(&self, v: f32) {
        if let Some(sink) = &self.inner.sink {
            sink.set_volume(v.clamp(0.0, 1.0));
        }
    }

    /// Elapsed time of the current source, if playing.
    pub fn position(&self) -> Option<std::time::Duration> {
        self.inner.sink.as_ref().map(|s| s.get_pos())
    }

    /// Drop the current source (skip to next queued one).
    pub fn skip(&self) {
        if let Some(sink) = &self.inner.sink {
            sink.skip_one();
        }
    }

    pub fn is_idle(&self) -> bool {
        self.inner.sink.as_ref().map(|s| s.empty()).unwrap_or(true)
    }
}
