//! Whisper with quantized weights, after candle-transformers'
//! `whisper::quantized_model` (Apache-2.0/MIT), changed for speed and beam
//! search: the decoder's view of the audio is prepared once per piece, each
//! step only computes the new token (its earlier ones are cached), and the
//! beams of a search run together as one batch.

use candle_core::{D, Device, Module, Result, Tensor};
use candle_nn::{Conv1d, Conv1dConfig, LayerNorm};
use candle_transformers::models::whisper::Config;
use candle_transformers::quantized_nn::{Embedding, Linear, layer_norm, linear, linear_no_bias};
use candle_transformers::quantized_var_builder::VarBuilder;

fn conv1d(inputs: usize, outputs: usize, config: Conv1dConfig, vb: VarBuilder) -> Result<Conv1d> {
    let weight = vb.get((outputs, inputs, 3), "weight")?.dequantize(vb.device())?;
    let bias = vb.get(outputs, "bias")?.dequantize(vb.device())?;
    Ok(Conv1d::new(weight, Some(bias), config))
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
    fn load(state: usize, heads: usize, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            query: linear(state, state, vb.pp("q_proj"))?,
            key: linear_no_bias(state, state, vb.pp("k_proj"))?,
            value: linear(state, state, vb.pp("v_proj"))?,
            out: linear(state, state, vb.pp("out_proj"))?,
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
        // Broadcast: several beams (a batch) attend to the same audio (one).
        let mut qk = q.broadcast_matmul(&k)?;
        if let Some(mask) = mask.filter(|_| ctx > 1) {
            qk = qk.broadcast_add(&mask.narrow(0, at, ctx)?.narrow(1, 0, at + ctx)?)?;
        }
        let w = candle_nn::ops::softmax_last_dim(&qk)?;
        let wv = w.broadcast_matmul(&v)?.transpose(1, 2)?.flatten_from(2)?;
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
    fn load(state: usize, heads: usize, cross: bool, vb: VarBuilder) -> Result<Self> {
        let cross = if cross {
            Some((Attention::load(state, heads, vb.pp("encoder_attn"))?, layer_norm(state, 1e-5, vb.pp("encoder_attn_layer_norm"))?))
        } else {
            None
        };
        Ok(Self {
            attn: Attention::load(state, heads, vb.pp("self_attn"))?,
            attn_ln: layer_norm(state, 1e-5, vb.pp("self_attn_layer_norm"))?,
            cross,
            mlp1: linear(state, state * 4, vb.pp("fc1"))?,
            mlp2: linear(state * 4, state, vb.pp("fc2"))?,
            mlp_ln: layer_norm(state, 1e-5, vb.pp("final_layer_norm"))?,
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
    fn load(vb: VarBuilder, cfg: &Config) -> Result<Self> {
        let conv = |stride| Conv1dConfig { padding: 1, stride, groups: 1, dilation: 1, cudnn_fwd_algo: None };
        let state = cfg.d_model;
        Ok(Self {
            conv1: conv1d(cfg.num_mel_bins, state, conv(1), vb.pp("conv1"))?,
            conv2: conv1d(state, state, conv(2), vb.pp("conv2"))?,
            positions: sinusoids(cfg.max_source_positions, state)?,
            blocks: (0..cfg.encoder_layers)
                .map(|i| Block::load(state, cfg.encoder_attention_heads, false, vb.pp(format!("layers.{i}"))))
                .collect::<Result<_>>()?,
            ln: layer_norm(state, 1e-5, vb.pp("layer_norm"))?,
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
    fn load(vb: VarBuilder, cfg: &Config) -> Result<Self> {
        let (state, ctx) = (cfg.d_model, cfg.max_target_positions);
        let mask: Vec<f32> = (0..ctx).flat_map(|i| (0..ctx).map(move |j| if j > i { f32::NEG_INFINITY } else { 0.0 })).collect();
        Ok(Self {
            tokens: Embedding::new(cfg.vocab_size, state, vb.pp("embed_tokens"))?,
            positions: vb.get((ctx, state), "embed_positions.weight")?.dequantize(vb.device())?,
            blocks: (0..cfg.decoder_layers)
                .map(|i| Block::load(state, cfg.decoder_attention_heads, true, vb.pp(format!("layers.{i}"))))
                .collect::<Result<_>>()?,
            ln: layer_norm(state, 1e-5, vb.pp("layer_norm"))?,
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

    pub fn final_linear(&self, x: &Tensor) -> Result<Tensor> {
        let w = self.tokens.embeddings().broadcast_left(x.dim(0)?)?;
        x.matmul(&w.t()?)
    }
}

pub struct Whisper {
    pub encoder: Encoder,
    pub decoder: Decoder,
    pub config: Config,
}

impl Whisper {
    pub fn load(vb: &VarBuilder, config: Config) -> Result<Self> {
        Ok(Self { encoder: Encoder::load(vb.pp("model.encoder"), &config)?, decoder: Decoder::load(vb.pp("model.decoder"), &config)?, config })
    }
}
