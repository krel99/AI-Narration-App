//! Playback through rodio. A paragraph is queued as consecutive buffers on
//! one rodio player, so it plays gaplessly.

use std::num::NonZero;

use crate::audio::Pcm;
use crate::model::Id;

pub struct Player {
    output: Option<(rodio::MixerDeviceSink, rodio::Player)>,
    /// Segment ids queued by the last play call, in order.
    queued: Vec<Id>,
    pub error: Option<String>,
}

impl Player {
    pub fn new() -> Self {
        match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(mut sink) => {
                sink.log_on_drop(false);
                let player = rodio::Player::connect_new(sink.mixer());
                Self {
                    output: Some((sink, player)),
                    queued: Vec::new(),
                    error: None,
                }
            }
            Err(e) => {
                log::warn!("no audio output: {e}");
                Self {
                    output: None,
                    queued: Vec::new(),
                    error: Some(format!("No audio output: {e}")),
                }
            }
        }
    }

    /// Replace whatever is playing with `items`, separated by `gap_ms` of silence.
    pub fn play(&mut self, items: Vec<(Id, Pcm)>, gap_ms: u32) {
        let Some((_, player)) = &self.output else {
            return;
        };
        player.clear();
        self.queued.clear();
        let n = items.len();
        for (i, (id, mut pcm)) in items.into_iter().enumerate() {
            if pcm.rate == 0 {
                continue;
            }
            // Silence is baked into each buffer so the queue index maps to a segment.
            if i + 1 < n {
                pcm.append_silence(gap_ms);
            }
            let rate = NonZero::new(pcm.rate).expect("non-zero rate");
            player.append(rodio::buffer::SamplesBuffer::new(
                NonZero::<u16>::MIN,
                rate,
                pcm.to_f32(),
            ));
            self.queued.push(id);
        }
        player.play();
    }

    pub fn stop(&mut self) {
        if let Some((_, player)) = &self.output {
            player.clear();
        }
        self.queued.clear();
    }

    pub fn is_playing(&self) -> bool {
        self.output.as_ref().is_some_and(|(_, p)| !p.empty())
    }

    /// Playback position inside the item that is currently audible.
    pub fn position(&self) -> std::time::Duration {
        self.output
            .as_ref()
            .map_or(std::time::Duration::ZERO, |(_, p)| p.get_pos())
    }

    /// The segment currently audible, if any.
    pub fn current(&self) -> Option<Id> {
        let (_, player) = self.output.as_ref()?;
        let remaining = player.len();
        if remaining == 0 || remaining > self.queued.len() {
            return None;
        }
        self.queued.get(self.queued.len() - remaining).copied()
    }
}
