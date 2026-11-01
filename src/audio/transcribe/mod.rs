//! Voice messages to text, on this computer: Whisper "small" (quantized,
//! 259 MB, downloaded once on first use), run with candle (pure Rust, so no
//! C++ tools are needed to build it). The audio never leaves the computer.

mod whisper;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use candle_core::{Device, Tensor};
use candle_transformers::models::whisper::{self as m, Config, audio};
use candle_transformers::quantized_var_builder::VarBuilder;
use sha2::{Digest, Sha256};

use whisper::Whisper;

/// Mel filter bank of the Whisper models with 80 bins (from candle's example).
const MEL_FILTERS: &[u8] = include_bytes!("melfilters.bytes");

/// The model files: Whisper small, its weights quantized to 8 bits for
/// candle, the rest from OpenAI; each pinned to one revision and checked
/// against these SHA-256 sums.
const OPENAI: &str = "https://huggingface.co/openai/whisper-small/resolve/973afd24965f72e36ca33b3055d56a652f456b4d";
const GGUF: &str = "https://huggingface.co/oxide-lab/whisper-small-GGUF/resolve/67b1bf8be07d7a0b836fbf1ecd354b19383ac288";
const FILES: [(&str, &str, &str); 3] = [
    (OPENAI, "config.json", "e6a2b489da1b5aed65a8eb8d1e7466fa867ad5643a8bc138ba708bd56b2875c4"),
    (OPENAI, "tokenizer.json", "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566"),
    (GGUF, "whisper-small-q8_0.gguf", "26d317490c2eb1b06ab36c28a13a0cf6fedb04e019d43fbe80bdec37b71cebf4"),
];
const WEIGHTS: &str = FILES[2].1;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Where the model is kept (with the settings: the cache is cleaned up).
fn model_dir() -> Option<PathBuf> {
    crate::system::secret::data_dir().map(|d| d.join("whisper").join("small"))
}

/// The "base" model used before, directly in `whisper/`: deleted (78 MB).
fn remove_base(dir: &Path) {
    if let Some(old) = dir.parent() {
        for f in ["config.json", "tokenizer.json", "whisper-base-q8_0.gguf"] {
            let _ = std::fs::remove_file(old.join(f));
        }
    }
}

/// Whether the model has been downloaded.
pub fn ready() -> bool {
    model_dir().is_some_and(|d| FILES.iter().all(|(_, f, _)| d.join(f).exists()))
}

/// Downloads the model files that are missing.
pub async fn fetch() -> Result<(), String> {
    let dir = model_dir().ok_or("no folder for the speech model")?;
    std::fs::create_dir_all(&dir).map_err(err)?;
    remove_base(&dir);
    let http = reqwest::Client::builder()
        .user_agent(crate::discord::api::user_agent())
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(err)?;
    for (repo, name, sum) in FILES {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        let resp = http.get(format!("{repo}/{name}")).send().await.map_err(|e| format!("couldn't download the speech model: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("couldn't download the speech model (HTTP {})", resp.status().as_u16()));
        }
        let data = resp.bytes().await.map_err(|e| format!("couldn't download the speech model: {e}"))?;
        if hex(&Sha256::digest(&data)) != sum {
            return Err(format!("the downloaded speech model ({name}) is not the expected file; not used"));
        }
        // Written under another name first: a broken download is never taken for the model.
        let part = dir.join(format!("{name}.part"));
        std::fs::write(&part, &data).map_err(err)?;
        std::fs::rename(&part, &path).map_err(err)?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The text spoken in a voice message (Ogg/Opus or WAV). `context` (the
/// chat's recent messages and names) is given to Whisper as the text that
/// came before: it then spells names and words the way the chat does.
/// Loads the model, so it takes a few seconds; run it off the UI thread.
pub fn transcribe(data: &[u8], context: &str) -> Result<String, String> {
    let (rate, pcm) = crate::audio::decode(data)?;
    if rate != 16_000 {
        return Err(format!("WAV must be 16 kHz to be transcribed (this is {rate} Hz)"));
    }
    if pcm.is_empty() {
        return Err("the voice message is empty".into());
    }
    let dir = model_dir().ok_or("no folder for the speech model")?;
    // Loaded for each message (in about a second) and dropped after: its
    // ~600 MB of memory are only in use while transcribing.
    Model::load(&dir)?.transcribe(&pcm, context)
}

struct Model {
    whisper: Whisper,
    tokens: Tokens,
    filters: Vec<f32>,
    /// Added to the scores: -∞ for the tokens Whisper never writes.
    suppress: Vec<f32>,
}

impl Model {
    fn load(dir: &Path) -> Result<Self, String> {
        let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json")).map_err(err)?).map_err(err)?;
        let tokens = Tokens::load(&dir.join("tokenizer.json"))?;
        let vb = VarBuilder::from_gguf(dir.join(WEIGHTS), &Device::Cpu).map_err(err)?;
        let suppress: Vec<f32> =
            (0..config.vocab_size as u32).map(|i| if config.suppress_tokens.contains(&i) { f32::NEG_INFINITY } else { 0.0 }).collect();
        let filters = MEL_FILTERS.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let whisper = Whisper::load(&vb, config).map_err(err)?;
        Ok(Self { whisper, tokens, filters, suppress })
    }

    /// The text spoken in `pcm` (16 kHz mono), in its own language.
    fn transcribe(&mut self, pcm: &[f32], context: &str) -> Result<String, String> {
        let bins = self.whisper.config.num_mel_bins;
        let mel = audio::pcm_to_mel(&self.whisper.config, pcm, &self.filters);
        let frames = mel.len() / bins;
        let mel = Tensor::from_vec(mel, (1, bins, frames), &Device::Cpu).map_err(err)?;
        // What came before: the context, then (as Whisper does it) what was
        // understood in the pieces so far.
        let mut before = if context.trim().is_empty() { Vec::new() } else { self.tokens.encode(&format!(" {}", context.trim())) };
        // The spectrogram is padded with silence: stop at the end of the audio.
        let content = pcm.len() / m::HOP_LENGTH;
        let mut text = String::new();
        let mut seek = 0;
        while seek < content {
            let len = m::N_FRAMES.min(frames - seek);
            let segment = mel.narrow(2, seek, len).map_err(err)?;
            seek += len;
            let said = self.segment(&segment, &before).map_err(err)?;
            let part = self.tokens.text(&said);
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&part);
                before.extend(said);
            }
        }
        Ok(text)
    }

    /// One 30-second piece: its language, then the words, found by beam
    /// search as OpenAI's Whisper does it (`BEAMS` candidates at a time).
    /// `before`: the text before it, as tokens (its end is used, up to half
    /// of what the model can take in).
    fn segment(&mut self, mel: &Tensor, before: &[u32]) -> candle_core::Result<Vec<u32>> {
        let t = &self.tokens;
        let features = self.whisper.encoder.forward(mel)?;
        let decoder = &mut self.whisper.decoder;
        // The language: the most likely language token after the start.
        let start = scores(decoder, &features, &[t.sot], 1, 0, true)?.remove(0);
        let langs = t.sot as usize + 1..t.translate as usize;
        let lang = langs.clone().max_by(|&a, &b| start[a].total_cmp(&start[b])).unwrap_or(langs.start) as u32;
        let no_speech = log_probs(&start, None)[t.no_speech as usize].exp() as f64;

        let max = self.whisper.config.max_target_positions;
        let mut prompt = Vec::new();
        if !before.is_empty() {
            prompt.push(t.prev);
            prompt.extend_from_slice(&before[before.len().saturating_sub(max / 2 - 1)..]);
        }
        prompt.extend([t.sot, lang, t.transcribe, t.no_timestamps]);

        // Candidates still going (tokens, summed log-probability), and finished ones.
        let mut beams: Vec<(Vec<u32>, f64)> = vec![(Vec::new(), 0.0)];
        let mut done: Vec<(Vec<u32>, f64)> = Vec::new();
        let mut rows = scores(decoder, &features, &prompt, 1, 0, false)?;
        let mut at = prompt.len();
        while done.len() < BEAMS {
            // Every candidate's best next tokens, best overall first.
            let mut next: Vec<(f64, usize, u32)> = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                let lp = log_probs(row, Some(&self.suppress));
                for (token, p) in best(&lp, BEAMS + 1) {
                    next.push((beams[i].1 + p as f64, i, token));
                }
            }
            next.sort_by(|a, b| b.0.total_cmp(&a.0));
            let mut kept: Vec<(Vec<u32>, f64)> = Vec::new();
            let mut from: Vec<u32> = Vec::new();
            for (score, i, token) in next {
                if kept.len() == BEAMS {
                    break;
                }
                let mut tokens = beams[i].0.clone();
                if token == t.eot {
                    if done.len() < BEAMS {
                        done.push((tokens, score));
                    }
                    continue;
                }
                tokens.push(token);
                if let Some(keep) = looping(&tokens) {
                    // Stuck repeating itself: ends here, with the run once.
                    tokens.truncate(keep);
                    done.push((tokens, score));
                    continue;
                }
                kept.push((tokens, score));
                from.push(i as u32);
            }
            if kept.is_empty() || at + 1 >= max || kept[0].0.len() >= max / 2 {
                done.extend(kept);
                break;
            }
            decoder.reorder(&from)?;
            let last: Vec<u32> = kept.iter().map(|(tokens, _)| *tokens.last().expect("just pushed")).collect();
            rows = scores(decoder, &features, &last, kept.len(), at, false)?;
            at += 1;
            beams = kept;
        }
        // The best by average log-probability, so short ones aren't favoured.
        let avg = |(tokens, score): &(Vec<u32>, f64)| score / tokens.len().max(1) as f64;
        let Some(said) = done.into_iter().max_by(|a, b| avg(a).total_cmp(&avg(b))) else { return Ok(Vec::new()) };
        // Silence or noise: Whisper's own rule for leaving a piece out.
        if no_speech > m::NO_SPEECH_THRESHOLD && avg(&said) < m::LOGPROB_THRESHOLD {
            return Ok(Vec::new());
        }
        Ok(said.0)
    }
}

/// Candidates followed at a time (as in OpenAI's Whisper).
const BEAMS: usize = 5;

/// The scores of the next token for each of `rows` sequences: `input` holds
/// their tokens from position `at` on, row after row.
fn scores(decoder: &mut whisper::Decoder, audio: &Tensor, input: &[u32], rows: usize, at: usize, flush: bool) -> candle_core::Result<Vec<Vec<f32>>> {
    let len = input.len() / rows;
    let ys = decoder.forward(&Tensor::from_slice(input, (rows, len), &Device::Cpu)?, audio, flush, at)?;
    decoder.final_linear(&ys.narrow(1, len - 1, 1)?)?.squeeze(1)?.to_vec2()
}

/// Log-probabilities from scores; `suppress` rules tokens out (-∞).
fn log_probs(scores: &[f32], suppress: Option<&[f32]>) -> Vec<f32> {
    let s: Vec<f32> = match suppress {
        Some(sup) => scores.iter().zip(sup).map(|(a, b)| a + b).collect(),
        None => scores.to_vec(),
    };
    let max = s.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = s.iter().map(|x| (x - max).exp()).sum();
    let log_sum = max + sum.ln();
    s.into_iter().map(|x| x - log_sum).collect()
}

/// The `n` most likely tokens, most likely first.
fn best(log_probs: &[f32], n: usize) -> Vec<(u32, f32)> {
    let mut top: Vec<(u32, f32)> = Vec::with_capacity(n + 1);
    for (i, &p) in log_probs.iter().enumerate() {
        if top.len() < n || p > top[top.len() - 1].1 {
            let pos = top.partition_point(|&(_, q)| q >= p);
            top.insert(pos, (i as u32, p));
            top.truncate(n);
        }
    }
    top
}

/// Greedy decoding can get stuck repeating itself ("ja ja ja ja …"): when
/// the end repeats a run of up to 10 tokens 4 times, the length to keep
/// (with the run once).
fn looping(tokens: &[u32]) -> Option<usize> {
    (1..=10).find_map(|n| {
        let len = tokens.len();
        if len < 4 * n {
            return None;
        }
        let run = &tokens[len - n..];
        (1..4).all(|k| &tokens[len - (k + 1) * n..len - k * n] == run).then(|| len - 3 * n)
    })
}

/// Whisper's tokens: byte-level BPE, as GPT-2's, read from `tokenizer.json`.
struct Tokens {
    text: Vec<Option<Vec<u8>>>,
    /// For encoding: the id of each token, and the rank of each merge.
    ids: HashMap<String, u32>,
    merges: HashMap<(String, String), usize>,
    prev: u32,
    sot: u32,
    eot: u32,
    translate: u32,
    transcribe: u32,
    no_timestamps: u32,
    no_speech: u32,
}

impl Tokens {
    fn load(path: &Path) -> Result<Self, String> {
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(path).map_err(err)?).map_err(err)?;
        let bytes = byte_chars();
        let mut text: Vec<Option<Vec<u8>>> = Vec::new();
        let mut put = |id: usize, t: Option<Vec<u8>>| {
            if text.len() <= id {
                text.resize(id + 1, None);
            }
            text[id] = t;
        };
        let mut ids = HashMap::new();
        for (s, id) in json["model"]["vocab"].as_object().ok_or("tokenizer.json: no vocabulary")? {
            let Some(id) = id.as_u64() else { continue };
            put(id as usize, s.chars().map(|c| bytes.get(&c).copied()).collect::<Option<Vec<u8>>>());
            ids.insert(s.clone(), id as u32);
        }
        // "Ġ t" (older files) or ["Ġ", "t"].
        let merges = json["model"]["merges"]
            .as_array()
            .ok_or("tokenizer.json: no merges")?
            .iter()
            .filter_map(|m| match m {
                serde_json::Value::String(s) => s.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())),
                serde_json::Value::Array(p) => Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())),
                _ => None,
            })
            .enumerate()
            .map(|(rank, pair)| (pair, rank))
            .collect();
        let mut special = HashMap::new();
        for t in json["added_tokens"].as_array().ok_or("tokenizer.json: no special tokens")? {
            if let (Some(id), Some(s)) = (t["id"].as_u64(), t["content"].as_str()) {
                put(id as usize, None);
                special.insert(s.to_string(), id as u32);
            }
        }
        let id = |s: &str| special.get(s).copied().ok_or(format!("tokenizer.json: no {s}"));
        Ok(Self {
            prev: id("<|startofprev|>")?,
            sot: id(m::SOT_TOKEN)?,
            eot: id(m::EOT_TOKEN)?,
            translate: id(m::TRANSLATE_TOKEN)?,
            transcribe: id(m::TRANSCRIBE_TOKEN)?,
            no_timestamps: id(m::NO_TIMESTAMPS_TOKEN)?,
            no_speech: m::NO_SPEECH_TOKENS.iter().find_map(|s| id(s).ok()).ok_or("tokenizer.json: no no-speech token")?,
            text,
            ids,
            merges,
        })
    }

    /// The tokens of `text` (no special tokens).
    fn encode(&self, text: &str) -> Vec<u32> {
        let chars = byte_chars();
        let mut of_byte = ['\0'; 256];
        for (c, b) in chars {
            of_byte[b as usize] = c;
        }
        let mut out = Vec::new();
        for piece in pieces(text) {
            // Each byte as its character, then merged by rank, lowest first.
            let mut parts: Vec<String> = piece.bytes().map(|b| of_byte[b as usize].to_string()).collect();
            loop {
                let best = parts
                    .windows(2)
                    .enumerate()
                    .filter_map(|(i, w)| self.merges.get(&(w[0].clone(), w[1].clone())).map(|r| (*r, i)))
                    .min();
                let Some((_, i)) = best else { break };
                let joined = format!("{}{}", parts[i], parts[i + 1]);
                parts.splice(i..i + 2, [joined]);
            }
            out.extend(parts.iter().filter_map(|p| self.ids.get(p)));
        }
        out
    }

    /// The text of `ids`; special tokens are left out.
    fn text(&self, ids: &[u32]) -> String {
        let bytes: Vec<u8> = ids.iter().filter_map(|&i| self.text.get(i as usize).cloned().flatten()).flatten().collect();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

/// Splits text as GPT-2 does before BPE: runs of letters, of digits and of
/// other characters, each with the space before it; English contractions
/// ('s 't 're 've 'm 'll 'd) apart; other whitespace on its own.
fn pieces(text: &str) -> Vec<String> {
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Letter,
        Digit,
        Other,
        Space,
    }
    let kind = |c: char| match c {
        c if c.is_whitespace() => Kind::Space,
        c if c.is_alphabetic() => Kind::Letter,
        c if c.is_numeric() => Kind::Digit,
        _ => Kind::Other,
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let rest: String = chars[i + 1..chars.len().min(i + 3)].iter().collect::<String>().to_lowercase();
            if let Some(n) = ["ll", "re", "ve", "s", "t", "m", "d"].iter().find(|s| rest.starts_with(**s)).map(|s| s.len()) {
                out.push(chars[i..=i + n].iter().collect());
                i += n + 1;
                continue;
            }
        }
        let mut start = i;
        if kind(c) == Kind::Space {
            let end = (i..chars.len()).find(|&j| kind(chars[j]) != Kind::Space).unwrap_or(chars.len());
            // The last space before a word belongs to the word.
            let attach = end < chars.len() && chars[end - 1] == ' ';
            let stop = if attach { end - 1 } else { end };
            if stop > i {
                out.push(chars[i..stop].iter().collect());
            }
            if !attach {
                i = end;
                continue;
            }
            start = stop;
            i = end;
        }
        let k = kind(chars[i]);
        let end = (i..chars.len()).find(|&j| kind(chars[j]) != k).unwrap_or(chars.len());
        out.push(chars[start..end].iter().collect());
        i = end;
    }
    out
}

/// GPT-2's mapping of bytes to printable characters, reversed.
fn byte_chars() -> HashMap<char, u8> {
    let mut map = HashMap::new();
    let mut n = 0u32;
    for b in 0..=255u8 {
        let printable = matches!(b, b'!'..=b'~' | 0xA1..=0xAC | 0xAE..=0xFF);
        let c = if printable {
            char::from(b)
        } else {
            n += 1;
            char::from_u32(255 + n).unwrap_or('\0')
        };
        map.insert(c, b);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::{byte_chars, looping, pieces};

    #[test]
    fn picks_the_best() {
        let lp = super::log_probs(&[1.0, 3.0, 2.0, f32::NEG_INFINITY], None);
        assert!((lp.iter().map(|p| p.exp()).sum::<f32>() - 1.0).abs() < 1e-5);
        let top = super::best(&lp, 2);
        assert_eq!(top.iter().map(|t| t.0).collect::<Vec<_>>(), [1, 2]);
        // Suppressed tokens never win.
        let lp = super::log_probs(&[5.0, 1.0], Some(&[f32::NEG_INFINITY, 0.0]));
        assert_eq!(super::best(&lp, 1)[0].0, 1);
    }

    #[test]
    fn splits_like_gpt2() {
        assert_eq!(pieces(" Hallo Anna, wie geht's?"), [" Hallo", " Anna", ",", " wie", " geht", "'s", "?"]);
        assert_eq!(pieces("um 15:30  Uhr\nok"), ["um", " 15", ":", "30", " ", " Uhr", "\n", "ok"]);
    }

    /// Needs the downloaded model (`t` once): encoding then decoding gives the text back.
    #[test]
    #[ignore]
    fn encodes_and_decodes() {
        let dir = super::model_dir().unwrap();
        let t = super::Tokens::load(&dir.join("tokenizer.json")).unwrap();
        for s in ["Hallo Anna, wie geht's? Übung fällt aus 😅", "Algorithmen und Datenstrukturen, Raum H 26"] {
            let ids = t.encode(s);
            assert!(ids.len() < s.len(), "{ids:?}");
            assert_eq!(t.text(&ids), s);
        }
        // Common words are single tokens.
        assert_eq!(t.encode(" the").len(), 1);
    }

    #[test]
    fn stops_repeating() {
        assert_eq!(looping(&[1, 2, 3]), None);
        assert_eq!(looping(&[9, 5, 5, 5, 5]), Some(2));
        assert_eq!(looping(&[9, 1, 2, 1, 2, 1, 2, 1, 2]), Some(3));
    }

    #[test]
    fn maps_every_byte() {
        let map = byte_chars();
        assert_eq!(map.len(), 256);
        // GPT-2 writes a space as "Ġ" (U+0120).
        assert_eq!(map[&'\u{120}'], b' ');
        assert_eq!(map[&'a'], b'a');
    }
}
