//! The Whisper model, after candle-transformers' `whisper::quantized_model`
//! (Apache-2.0/MIT), changed for speed and beam search: the decoder's view of
//! the audio is prepared once per piece, each step only computes the new
//! token (its earlier ones are cached), and the beams of a search run
//! together as one batch.
//!
//! Nothing here makes candle copy weights: its matrix product copies a
//! matrix that is "broadcast" to a batch, which for the beams of a search
//! meant hundreds of megabytes per word.

use std::fs::File;
use std::path::Path;

use candle_core::quantized::QMatMul;
use candle_core::quantized::gguf_file::Content;
use candle_core::{D, Device, Module, Result, Tensor};
use candle_nn::{Conv1d, Conv1dConfig, Embedding, LayerNorm};
use serde::Deserialize;

/// The model's sizes (`config.json`).
#[derive(Deserialize)]
pub struct Config {
    pub num_mel_bins: usize,
    pub max_source_positions: usize,
    pub d_model: usize,
    pub encoder_attention_heads: usize,
    pub encoder_layers: usize,
    pub vocab_size: usize,
    pub max_target_positions: usize,
    pub decoder_attention_heads: usize,
    pub decoder_layers: usize,
    /// Tokens Whisper never writes.
    #[serde(default)]
    pub suppress_tokens: Vec<u32>,
}

/// The weights file (GGUF); each tensor is read when asked for.
struct Weights {
    file: File,
    content: Content,
}

impl Weights {
    /// A tensor as plain numbers (the small ones: norms, positions, …).
    fn get(&mut self, name: &str) -> Result<Tensor> {
        self.content.tensor(&mut self.file, name, &Device::Cpu)?.dequantize(&Device::Cpu)
    }

    fn linear(&mut self, name: &str, bias: bool) -> Result<Linear> {
        let bias = if bias { Some(self.get(&format!("{name}.bias"))?) } else { None };
        let weight = self.content.tensor(&mut self.file, &format!("{name}.weight"), &Device::Cpu)?;
        Ok(Linear { weight: QMatMul::from_qtensor(weight)?, bias })
    }

    fn layer_norm(&mut self, name: &str) -> Result<LayerNorm> {
        Ok(LayerNorm::new(self.get(&format!("{name}.weight"))?, self.get(&format!("{name}.bias"))?, 1e-5))
    }

    fn conv1d(&mut self, name: &str, stride: usize) -> Result<Conv1d> {
        let config = Conv1dConfig { padding: 1, stride, groups: 1, dilation: 1, cudnn_fwd_algo: None };
        Ok(Conv1d::new(self.get(&format!("{name}.weight"))?, Some(self.get(&format!("{name}.bias"))?), config))
    }
}

/// A linear layer, its weights still quantized (as they come in the file):
/// multiplied that way they take a quarter of the memory, which also makes
/// writing word after word faster than with plain numbers.
struct Linear {
    weight: QMatMul,
    bias: Option<Tensor>,
}

impl Module for Linear {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let y = self.weight.forward(x)?;
        match &self.bias {
            Some(bias) => y.broadcast_add(bias),
            None => Ok(y),
        }
    }
}

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    out: Linear,
    heads: usize,
    /// For attention to the audio: its keys and values, ready to multiply.
    audio: Option<(Tensor, Tensor)>,
    /// In the decoder, the keys and values of the tokens so far: each step
    /// only computes the new token's.
    earlier: Option<(Tensor, Tensor)>,
}

impl Attention {
    fn load(w: &mut Weights, name: &str, heads: usize) -> Result<Self> {
        Ok(Self {
            query: w.linear(&format!("{name}.q_proj"), true)?,
            key: w.linear(&format!("{name}.k_proj"), false)?,
            value: w.linear(&format!("{name}.v_proj"), true)?,
            out: w.linear(&format!("{name}.out_proj"), true)?,
            heads,
            audio: None,
            earlier: None,
        })
    }

    fn heads(&self, x: &Tensor) -> Result<Tensor> {
        let (batch, ctx, state) = x.dims3()?;
        x.reshape((batch, ctx, self.heads, state / self.heads))?.transpose(1, 2)
    }

    /// Keys (transposed) and values split into heads, both scaled and contiguous.
    fn keys_values(&self, x: &Tensor) -> Result<(Tensor, Tensor)> {
        let scale = ((x.dim(D::Minus1)? / self.heads) as f64).powf(-0.25);
        let k = (self.heads(&self.key.forward(x)?)?.transpose(2, 3)? * scale)?.contiguous()?;
        let v = self.heads(&self.value.forward(x)?)?.contiguous()?;
        Ok((k, v))
    }

    /// `x`: the tokens from position `at` on. `mask` (causal) and `at` are
    /// for the decoder's attention to its own tokens.
    fn forward(&mut self, x: &Tensor, audio: Option<&Tensor>, mask: Option<&Tensor>, flush: bool, at: usize) -> Result<Tensor> {
        // Attention to the audio is the same for every token of every beam:
        // they all go in one row, so the audio's keys and values (one row)
        // needn't be copied for each beam.
        if audio.is_some() && x.dim(0)? > 1 {
            let (batch, ctx, state) = x.dims3()?;
            return self.forward(&x.reshape((1, batch * ctx, state))?, audio, mask, flush, at)?.reshape((batch, ctx, state));
        }
        let (k, v) = match audio {
            None if mask.is_some() => {
                let (k, v) = self.keys_values(x)?;
                let (k, v) = match self.earlier.take().filter(|_| at > 0) {
                    Some((k0, v0)) => (Tensor::cat(&[&k0, &k], 3)?, Tensor::cat(&[&v0, &v], 2)?),
                    None => (k, v),
                };
                self.earlier = Some((k.clone(), v.clone()));
                (k, v)
            }
            None => self.keys_values(x)?,
            Some(a) => {
                if flush || self.audio.is_none() {
                    self.audio = Some(self.keys_values(a)?);
                }
                self.audio.clone().expect("just set")
            }
        };
        let (_, ctx, state) = x.dims3()?;
        let scale = ((state / self.heads) as f64).powf(-0.25);
        let q = (self.heads(&self.query.forward(x)?)? * scale)?;
        let mut qk = q.matmul(&k)?;
        if let Some(mask) = mask.filter(|_| ctx > 1) {
            qk = qk.broadcast_add(&mask.narrow(0, at, ctx)?.narrow(1, 0, at + ctx)?)?;
        }
        let w = candle_nn::ops::softmax_last_dim(&qk)?;
        let wv = w.matmul(&v)?.transpose(1, 2)?.flatten_from(2)?;
        self.out.forward(&wv)
    }

    /// The batch's rows of the earlier tokens, newly picked: row i is `rows[i]`.
    fn reorder(&mut self, rows: &Tensor) -> Result<()> {
        if let Some((k, v)) = self.earlier.take() {
            self.earlier = Some((k.index_select(rows, 0)?, v.index_select(rows, 0)?));
        }
        Ok(())
    }
}

struct Block {
    attn: Attention,
    attn_ln: LayerNorm,
    cross: Option<(Attention, LayerNorm)>,
    mlp1: Linear,
    mlp2: Linear,
    mlp_ln: LayerNorm,
}

impl Block {
    fn load(w: &mut Weights, name: &str, heads: usize, cross: bool) -> Result<Self> {
        let cross = if cross {
            Some((Attention::load(w, &format!("{name}.encoder_attn"), heads)?, w.layer_norm(&format!("{name}.encoder_attn_layer_norm"))?))
        } else {
            None
        };
        Ok(Self {
            attn: Attention::load(w, &format!("{name}.self_attn"), heads)?,
            attn_ln: w.layer_norm(&format!("{name}.self_attn_layer_norm"))?,
            cross,
            mlp1: w.linear(&format!("{name}.fc1"), true)?,
            mlp2: w.linear(&format!("{name}.fc2"), true)?,
            mlp_ln: w.layer_norm(&format!("{name}.final_layer_norm"))?,
        })
    }

    fn forward(&mut self, x: &Tensor, audio: Option<&Tensor>, mask: Option<&Tensor>, flush: bool, at: usize) -> Result<Tensor> {
        let mut x = (x + self.attn.forward(&self.attn_ln.forward(x)?, None, mask, flush, at)?)?;
        if let Some((attn, ln)) = &mut self.cross {
            x = (&x + attn.forward(&ln.forward(&x)?, audio, None, flush, at)?)?;
        }
        let mlp = x.apply(&self.mlp_ln)?.apply(&self.mlp1)?.gelu()?.apply(&self.mlp2)?;
        x + mlp
    }
}

fn sinusoids(length: usize, channels: usize) -> Result<Tensor> {
    let step = 10000f32.ln() / (channels / 2 - 1) as f32;
    let inv: Vec<f32> = (0..channels / 2).map(|i| (i as f32 * -step).exp()).collect();
    let inv = Tensor::new(inv.as_slice(), &Device::Cpu)?.unsqueeze(0)?;
    let t = Tensor::arange(0, length as u32, &Device::Cpu)?.to_dtype(candle_core::DType::F32)?.unsqueeze(1)?;
    let shape = (length, channels / 2);
    let scaled = (t.broadcast_as(shape)? * inv.broadcast_as(shape)?)?;
    Tensor::cat(&[scaled.sin()?, scaled.cos()?], 1)
}

pub struct Encoder {
    conv1: Conv1d,
    conv2: Conv1d,
    positions: Tensor,
    blocks: Vec<Block>,
    ln: LayerNorm,
}

impl Encoder {
    fn load(w: &mut Weights, cfg: &Config) -> Result<Self> {
        let name = "model.encoder";
        Ok(Self {
            conv1: w.conv1d(&format!("{name}.conv1"), 1)?,
            conv2: w.conv1d(&format!("{name}.conv2"), 2)?,
            positions: sinusoids(cfg.max_source_positions, cfg.d_model)?,
            blocks: (0..cfg.encoder_layers)
                .map(|i| Block::load(w, &format!("{name}.layers.{i}"), cfg.encoder_attention_heads, false))
                .collect::<Result<_>>()?,
            ln: w.layer_norm(&format!("{name}.layer_norm"))?,
        })
    }

    /// The audio's features, from its mel spectrogram.
    pub fn forward(&mut self, mel: &Tensor) -> Result<Tensor> {
        let x = self.conv1.forward(mel)?.gelu()?;
        let x = self.conv2.forward(&x)?.gelu()?.transpose(1, 2)?;
        let len = x.dim(1)?;
        let mut x = x.broadcast_add(&self.positions.narrow(0, 0, len)?)?;
        for block in &mut self.blocks {
            x = block.forward(&x, None, None, true, 0)?;
        }
        self.ln.forward(&x)
    }
}

pub struct Decoder {
    tokens: Embedding,
    positions: Tensor,
    blocks: Vec<Block>,
    ln: LayerNorm,
    mask: Tensor,
}

impl Decoder {
    fn load(w: &mut Weights, cfg: &Config) -> Result<Self> {
        let name = "model.decoder";
        let ctx = cfg.max_target_positions;
        let mask: Vec<f32> = (0..ctx).flat_map(|i| (0..ctx).map(move |j| if j > i { f32::NEG_INFINITY } else { 0.0 })).collect();
        Ok(Self {
            tokens: Embedding::new(w.get(&format!("{name}.embed_tokens.weight"))?, cfg.d_model),
            positions: w.get(&format!("{name}.embed_positions.weight"))?,
            blocks: (0..cfg.decoder_layers)
                .map(|i| Block::load(w, &format!("{name}.layers.{i}"), cfg.decoder_attention_heads, true))
                .collect::<Result<_>>()?,
            ln: w.layer_norm(&format!("{name}.layer_norm"))?,
            mask: Tensor::from_vec(mask, (ctx, ctx), &Device::Cpu)?,
        })
    }

    /// The tokens from position `at` on (the ones before were passed
    /// earlier; 0 starts again). `flush`: `audio` is new.
    pub fn forward(&mut self, tokens: &Tensor, audio: &Tensor, flush: bool, at: usize) -> Result<Tensor> {
        let len = tokens.dim(D::Minus1)?;
        let mut x = self.tokens.forward(tokens)?.broadcast_add(&self.positions.narrow(0, at, len)?)?;
        for block in &mut self.blocks {
            x = block.forward(&x, Some(audio), Some(&self.mask), flush, at)?;
        }
        self.ln.forward(&x)
    }

    /// Beam search: which sequence each row of the batch continues
    /// (`rows[i]`: the old row of the new row i).
    pub fn reorder(&mut self, rows: &[u32]) -> Result<()> {
        let rows = Tensor::new(rows, &Device::Cpu)?;
        for block in &mut self.blocks {
            block.attn.reorder(&rows)?;
        }
        Ok(())
    }

    /// The score of every token of the vocabulary, for each row of `x`
    /// (rows by model width).
    pub fn final_linear(&self, x: &Tensor) -> Result<Tensor> {
        // Embeddings times the rows as columns: the embeddings (160 MB) are
        // used as they lie in memory, not copied.
        self.tokens.embeddings().matmul(&x.t()?.contiguous()?)?.t()?.contiguous()
    }
}

pub struct Whisper {
    pub encoder: Encoder,
    pub decoder: Decoder,
    pub config: Config,
}

impl Whisper {
    /// Reads the weights (a GGUF file).
    pub fn load(weights: &Path, config: Config) -> Result<Self> {
        let mut file = File::open(weights)?;
        let content = Content::read(&mut file)?;
        let mut w = Weights { file, content };
        Ok(Self { encoder: Encoder::load(&mut w, &config)?, decoder: Decoder::load(&mut w, &config)?, config })
    }
}
