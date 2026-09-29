//! Safe adapter around the P25 CAI state machine and Phase-I IMBE vocoder.

use imbe::{ImbeDecoder, consts::SAMPLES_PER_FRAME, frame::ReceivedFrame};
use p25::{
    error::P25Error,
    message::{
        nid::NetworkId,
        receiver::{MessageEvent, MessageReceiver},
    },
    trunking::tsbk::TsbkFields,
    voice::{
        control::LinkControlFields, crypto::CryptoAlgorithm, frame::VoiceFrame,
        header::VoiceHeaderFields,
    },
};

// Audio frames are carried inline: one 320-byte array per 20 ms is cheaper
// than a heap allocation per frame on the decode hot path.
#[allow(clippy::large_enum_variant)]
pub enum P25Event {
    Error(P25Error),
    NetworkId(NetworkId),
    Trunking(TsbkFields),
    VoiceHeader(VoiceHeaderFields),
    LinkControl(LinkControlFields),
    Crypto(CryptoAlgorithm),
    Audio([i16; SAMPLES_PER_FRAME]),
    VoiceTerm(LinkControlFields),
}

pub struct P25Decoder {
    messages: MessageReceiver,
    imbe: ImbeDecoder,
}

impl P25Decoder {
    pub fn new() -> Self {
        Self {
            messages: MessageReceiver::new(),
            imbe: ImbeDecoder::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn feed<F>(&mut self, samples: &[f32], mut on_event: F)
    where
        F: FnMut(P25Event),
    {
        for &sample in samples {
            let Some(event) = self.messages.feed(sample) else {
                continue;
            };

            match event {
                MessageEvent::Error(error) => on_event(P25Event::Error(error)),
                MessageEvent::PacketNID(nid) => on_event(P25Event::NetworkId(nid)),
                MessageEvent::TrunkingControl(tsbk) => on_event(P25Event::Trunking(tsbk)),
                MessageEvent::VoiceHeader(header) => on_event(P25Event::VoiceHeader(header)),
                MessageEvent::LinkControl(link) => on_event(P25Event::LinkControl(link)),
                MessageEvent::CryptoControl(crypto) => {
                    on_event(P25Event::Crypto(crypto.alg()));
                }
                MessageEvent::VoiceFrame(frame) => {
                    on_event(P25Event::Audio(self.decode_voice(frame)));
                }
                MessageEvent::VoiceTerm(link) => on_event(P25Event::VoiceTerm(link)),
                MessageEvent::LowSpeedDataFragment(_) => {}
            }
        }
    }

    fn decode_voice(&mut self, frame: VoiceFrame) -> [i16; SAMPLES_PER_FRAME] {
        let received = ReceivedFrame::new(frame.chunks, frame.errors);
        let mut float_samples = [0.0_f32; SAMPLES_PER_FRAME];
        self.imbe.decode(received, &mut float_samples);

        let mut pcm = [0_i16; SAMPLES_PER_FRAME];
        for (target, sample) in pcm.iter_mut().zip(float_samples) {
            let normalized = (sample / 8192.0).clamp(-1.0, 1.0);
            *target = (normalized * i16::MAX as f32) as i16;
        }
        pcm
    }
}

impl Default for P25Decoder {
    fn default() -> Self {
        Self::new()
    }
}
