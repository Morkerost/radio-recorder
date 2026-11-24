use super::{language::tr, AppResult};
use mp3lame_encoder::{
    max_required_buffer_size, Bitrate, Builder, Encoder, FlushGap, InterleavedPcm, Mode, MonoPcm,
    VbrMode,
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Wav,
    Mp3,
}

impl AudioFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
        }
    }
}

// Each entry also maps directly to its numeric setting and its menu label.
pub const BITRATES: &[Bitrate] = &[
    Bitrate::Kbps64,
    Bitrate::Kbps96,
    Bitrate::Kbps128,
    Bitrate::Kbps160,
    Bitrate::Kbps192,
    Bitrate::Kbps224,
    Bitrate::Kbps256,
    Bitrate::Kbps320,
];

pub fn bitrate(kbps: u16) -> AppResult<Bitrate> {
    BITRATES
        .iter()
        .copied()
        .find(|rate| *rate as u16 == kbps)
        .ok_or_else(|| tr("Unsupported MP3 bitrate", "Неподдерживаемый битрейт MP3").into())
}

pub struct Mp3Encoder {
    encoder: Encoder,
    buffer: Vec<u8>,
    channels: usize,
    input_rate: u32,
    output_rate: u32,
}

impl Mp3Encoder {
    pub fn validate_input(spec: hound::WavSpec) -> AppResult<()> {
        if !matches!(spec.channels, 1 | 2) {
            return Err(tr(
                "MP3 requires a mono or stereo input. Select a different input or WAV.",
                "Для MP3 нужен моно- или стереовход. Выберите другой вход или WAV.",
            )
            .into());
        }
        if spec.sample_rate == 0 {
            return Err(tr("Invalid sample rate", "Недопустимая частота дискретизации").into());
        }
        Ok(())
    }

    pub fn new(spec: hound::WavSpec, kbps: u16) -> AppResult<Self> {
        Self::validate_input(spec)?;
        // MPEG-1 supports every selectable CBR rate. In particular, a low-rate
        // microphone must not silently turn a requested 320 kbps file into 160 kbps.
        let output_rate = match spec.sample_rate {
            0..=32000 => 32000,
            32001..=44100 => 44100,
            _ => 48000,
        };
        let encoder = Builder::new()
            .ok_or(tr("Could not initialize LAME", "Не удалось запустить LAME"))?
            .with_sample_rate(spec.sample_rate)?
            .with_output_sample_rate(NonZeroU32::new(output_rate))?
            .with_num_channels(spec.channels as u8)?
            .with_mode(if spec.channels == 1 {
                Mode::Mono
            } else {
                Mode::JointStereo
            })?
            .with_vbr_mode(VbrMode::Off)?
            .with_brate(bitrate(kbps)?)?
            .with_to_write_vbr_tag(true)?
            .build()?;
        Ok(Self {
            encoder,
            buffer: Vec::new(),
            channels: spec.channels.into(),
            input_rate: spec.sample_rate,
            output_rate,
        })
    }

    fn write(&mut self, out: &mut BufWriter<File>, samples: &[i16]) -> AppResult<()> {
        if samples.len() % self.channels != 0 {
            return Err(tr("Incomplete audio frame", "Неполный аудиокадр").into());
        }
        for block in samples.chunks(4096 * self.channels) {
            let frames = block.len() / self.channels;
            let resampled =
                (frames as u64 * u64::from(self.output_rate)).div_ceil(u64::from(self.input_rate));
            self.buffer.clear();
            self.buffer
                .reserve(max_required_buffer_size(frames.max(resampled as usize)));
            if self.channels == 1 {
                self.encoder
                    .encode_to_vec(MonoPcm(block), &mut self.buffer)?;
            } else {
                self.encoder
                    .encode_to_vec(InterleavedPcm(block), &mut self.buffer)?;
            }
            out.write_all(&self.buffer)?;
        }
        Ok(())
    }

    fn finish(mut self, mut out: BufWriter<File>) -> AppResult<()> {
        self.buffer.clear();
        self.buffer.reserve(max_required_buffer_size(0));
        self.encoder.flush_to_vec::<FlushGap>(&mut self.buffer)?;
        out.write_all(&self.buffer)?;
        // Replace LAME's reserved first frame with this segment's actual
        // duration, encoder delay and padding for gapless-aware players.
        let tag_size = self.encoder.lame_tag_size();
        if tag_size > 0 {
            self.buffer.clear();
            self.buffer.reserve(tag_size);
            self.encoder
                .lame_tag_encode_to_vec(&mut self.buffer)
                .ok_or(tr(
                    "Could not finish the MP3 header",
                    "Не удалось завершить заголовок MP3",
                ))?;
            out.seek(SeekFrom::Start(self.encoder.id3v2_tag_size() as u64))?;
            out.write_all(&self.buffer)?;
        }
        out.flush()?;
        Ok(())
    }
}

pub enum SegmentWriter {
    Wav(hound::WavWriter<BufWriter<File>>),
    Mp3(Mp3Encoder, BufWriter<File>),
}

impl SegmentWriter {
    pub fn write_samples(&mut self, samples: &[i16]) -> AppResult<()> {
        match self {
            Self::Wav(wav) => {
                for sample in samples {
                    wav.write_sample(*sample)?;
                }
                Ok(())
            }
            Self::Mp3(encoder, file) => encoder.write(file, samples),
        }
    }

    pub fn finalize(self) -> AppResult<()> {
        match self {
            Self::Wav(wav) => Ok(wav.finalize()?),
            Self::Mp3(encoder, file) => encoder.finish(file),
        }
    }
}
