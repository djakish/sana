//! Full-text analysis, postings, and BM25 scoring.
//!
//! The first Stage 7 index is intentionally conservative: each flush publishes
//! a full immutable text snapshot with fixed-size term blocks and field stats.
//! Querying can use that SST directly when the WAL is fully indexed; otherwise
//! the query path falls back to scoring the strong materialized document
//! snapshot.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::sst::{SstReader, SstWriter};
use crate::value::{Document, Id, Value};

const KEY_KIND_FIELD_STATS: u8 = 0;
const KEY_KIND_TERM_META: u8 = 1;
const KEY_KIND_TERM_BLOCK: u8 = 2;

const DEFAULT_MAX_TOKEN_LEN: usize = 39;
const POSTING_BLOCK_TARGET: usize = 256;
const SCORE_BATCH_TARGET: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizerConfig {
    #[serde(default = "default_lowercase")]
    pub lowercase: bool,
    #[serde(default = "default_max_token_len")]
    pub max_token_len: usize,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        Self {
            lowercase: true,
            max_token_len: DEFAULT_MAX_TOKEN_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bm25Params {
    #[serde(default = "default_k1")]
    pub k1: f32,
    #[serde(default = "default_b")]
    pub b: f32,
    #[serde(default = "default_k3")]
    pub k3: f32,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self {
            k1: default_k1(),
            b: default_b(),
            k3: default_k3(),
        }
    }
}

impl Bm25Params {
    pub fn validate(self) -> Result<Self> {
        if !self.k1.is_finite() || self.k1 <= 0.0 {
            return Err(Error::InvalidQuery("BM25 k1 must be finite and > 0".into()));
        }
        if !self.b.is_finite() || !(0.0..=1.0).contains(&self.b) {
            return Err(Error::InvalidQuery(
                "BM25 b must be finite and in [0, 1]".into(),
            ));
        }
        if !self.k3.is_finite() || self.k3 <= 0.0 {
            return Err(Error::InvalidQuery("BM25 k3 must be finite and > 0".into()));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermFrequency {
    pub term: String,
    pub frequency: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextDocumentStats {
    pub doc_len: u32,
    pub terms: Vec<TermFrequency>,
}

#[derive(Clone, Debug)]
pub struct BuiltTextSst {
    pub bytes: Vec<u8>,
    pub entry_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextHit {
    pub id: Id,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextTermStats {
    pub doc_freq: u64,
    pub block_count: u32,
    pub max_score: f32,
    pub block_max_scores: Vec<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextSearchStats {
    pub blocks_read: u64,
    pub blocks_skipped: u64,
    pub score_batches: u64,
    pub postings_scored: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextSearchOutcome {
    pub hits: Vec<TextHit>,
    pub stats: TextSearchStats,
}

#[derive(Clone, Debug, Default)]
struct TextIndexData {
    field_stats: BTreeMap<String, FieldStats>,
    postings: BTreeMap<(String, String), Vec<TermPosting>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct FieldStats {
    doc_count: u64,
    total_doc_len: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct TermPosting {
    id: Id,
    term_frequency: u32,
    doc_len: u32,
}

#[derive(Clone, Copy, Debug)]
struct Bm25TermScorer {
    idf: f32,
    avg_doc_len: f32,
    k1: f32,
    b: f32,
}

impl Bm25TermScorer {
    fn new(avg_doc_len: f32, doc_count: u64, doc_freq: u64, params: Bm25Params) -> Option<Self> {
        if avg_doc_len <= 0.0 || doc_count == 0 || doc_freq == 0 {
            return None;
        }
        let n = doc_count as f32;
        let df = doc_freq.min(doc_count) as f32;
        Some(Self {
            idf: (1.0 + (n - df + 0.5) / (df + 0.5)).ln(),
            avg_doc_len,
            k1: params.k1,
            b: params.b,
        })
    }

    fn score(self, term_frequency: u32, doc_len: u32) -> f32 {
        if term_frequency == 0 || doc_len == 0 {
            return 0.0;
        }
        let tf = term_frequency as f32;
        let norm = 1.0 - self.b + self.b * (doc_len as f32 / self.avg_doc_len);
        self.idf * (tf * (self.k1 + 1.0)) / (tf + self.k1 * norm)
    }
}

#[derive(Clone, Debug)]
struct ScoredId {
    score: f32,
    id: Id,
}

impl PartialEq for ScoredId {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.id == other.id
    }
}

impl Eq for ScoredId {}

impl PartialOrd for ScoredId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

#[derive(Clone, Debug)]
struct TopKTracker {
    k: usize,
    by_id: BTreeMap<Id, f32>,
    by_score: BTreeSet<ScoredId>,
}

impl TopKTracker {
    fn new(k: usize) -> Self {
        Self {
            k,
            by_id: BTreeMap::new(),
            by_score: BTreeSet::new(),
        }
    }

    fn observe(&mut self, id: Id, score: f32) {
        if self.k == 0 {
            return;
        }
        if let Some(previous) = self.by_id.remove(&id) {
            self.by_score.remove(&ScoredId {
                score: previous,
                id: id.clone(),
            });
            self.insert(id, score);
            return;
        }
        if self.by_id.len() < self.k {
            self.insert(id, score);
            return;
        }
        let Some(threshold) = self.threshold() else {
            return;
        };
        if score > threshold {
            self.insert(id, score);
            self.trim();
        }
    }

    fn threshold(&self) -> Option<f32> {
        if self.by_id.len() < self.k {
            return None;
        }
        self.by_score.iter().next().map(|entry| entry.score)
    }

    fn insert(&mut self, id: Id, score: f32) {
        self.by_id.insert(id.clone(), score);
        self.by_score.insert(ScoredId { score, id });
    }

    fn trim(&mut self) {
        while self.by_id.len() > self.k {
            let Some(entry) = self.by_score.iter().next().cloned() else {
                break;
            };
            self.by_score.remove(&entry);
            self.by_id.remove(&entry.id);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredTermStats {
    doc_freq: u64,
    block_count: u32,
    max_score: f32,
    block_max_scores: Vec<f32>,
}

#[derive(Serialize, Deserialize)]
struct PostingBlock {
    max_score: f32,
    postings: Vec<TermPosting>,
}

fn default_lowercase() -> bool {
    true
}

fn default_max_token_len() -> usize {
    DEFAULT_MAX_TOKEN_LEN
}

fn default_k1() -> f32 {
    1.2
}

fn default_b() -> f32 {
    0.75
}

fn default_k3() -> f32 {
    8.0
}

pub fn tokenize(text: &str, config: TokenizerConfig) -> Vec<String> {
    let max_token_len = config.max_token_len.max(1);
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if config.lowercase {
                current.extend(ch.to_lowercase());
            } else {
                current.push(ch);
            }
        } else {
            finish_token(&mut tokens, &mut current, max_token_len);
        }
    }
    finish_token(&mut tokens, &mut current, max_token_len);
    tokens
}

pub fn analyze_text(text: &str, config: TokenizerConfig) -> TextDocumentStats {
    analyze_tokens(tokenize(text, config))
}

pub fn analyze_tokens(tokens: impl IntoIterator<Item = String>) -> TextDocumentStats {
    let mut doc_len = 0u32;
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for token in tokens {
        doc_len = doc_len.saturating_add(1);
        counts
            .entry(token)
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
    }
    TextDocumentStats {
        doc_len,
        terms: counts
            .into_iter()
            .map(|(term, frequency)| TermFrequency { term, frequency })
            .collect(),
    }
}

pub fn bm25_term_score(
    term_frequency: u32,
    doc_len: u32,
    avg_doc_len: f32,
    doc_count: u64,
    doc_freq: u64,
    params: Bm25Params,
) -> f32 {
    if term_frequency == 0 || doc_len == 0 || avg_doc_len <= 0.0 || doc_count == 0 || doc_freq == 0
    {
        return 0.0;
    }

    let params = params.validate().unwrap_or_default();
    Bm25TermScorer::new(avg_doc_len, doc_count, doc_freq, params)
        .map(|scorer| scorer.score(term_frequency, doc_len))
        .unwrap_or(0.0)
}

pub fn build_text_sst(docs: &BTreeMap<Id, Document>) -> Result<Option<BuiltTextSst>> {
    let data = build_index_data(docs, TokenizerConfig::default());
    if data.field_stats.is_empty() {
        return Ok(None);
    }

    let mut entries: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (column, stats) in &data.field_stats {
        entries.insert(
            field_stats_key(column),
            postcard::to_allocvec(stats).map_err(|e| Error::Codec(e.to_string()))?,
        );
    }
    for ((column, term), postings) in &data.postings {
        let field_stats = data.field_stats.get(column).copied().unwrap_or_default();
        let avg_doc_len = average_doc_len(field_stats);
        let doc_freq = postings.len() as u64;
        let scorer = Bm25TermScorer::new(
            avg_doc_len,
            field_stats.doc_count,
            doc_freq,
            Bm25Params::default(),
        );
        let mut posting_blocks = Vec::new();
        let mut block_max_scores = Vec::new();
        for (block_id, block_postings) in postings.chunks(POSTING_BLOCK_TARGET).enumerate() {
            let block_max_score = block_postings
                .iter()
                .map(|posting| {
                    scorer
                        .map(|scorer| scorer.score(posting.term_frequency, posting.doc_len))
                        .unwrap_or(0.0)
                })
                .fold(0.0f32, f32::max);
            block_max_scores.push(block_max_score);
            posting_blocks.push((block_id as u32, block_max_score, block_postings.to_vec()));
        }
        let block_count = block_max_scores.len() as u32;
        let term_max_score = block_max_scores.iter().copied().fold(0.0f32, f32::max);
        entries.insert(
            term_meta_key(column, term),
            postcard::to_allocvec(&StoredTermStats {
                doc_freq,
                block_count,
                max_score: term_max_score,
                block_max_scores,
            })
            .map_err(|e| Error::Codec(e.to_string()))?,
        );
        for (block_id, block_max_score, block_postings) in posting_blocks {
            entries.insert(
                term_block_key(column, term, block_id),
                postcard::to_allocvec(&PostingBlock {
                    max_score: block_max_score,
                    postings: block_postings,
                })
                .map_err(|e| Error::Codec(e.to_string()))?,
            );
        }
    }

    let entry_count = entries.len() as u64;
    let mut writer = SstWriter::new();
    for (key, value) in entries {
        writer.add(&key, &value)?;
    }

    Ok(Some(BuiltTextSst {
        bytes: writer.finish()?,
        entry_count,
    }))
}

pub fn search_sst(
    reader: &SstReader,
    column: &str,
    query: &str,
    params: Bm25Params,
) -> Result<Vec<TextHit>> {
    let params = params.validate()?;
    let query_stats = analyze_text(query, TokenizerConfig::default());
    if query_stats.terms.is_empty() {
        return Ok(Vec::new());
    }

    let Some(stats_bytes) = reader.get(&field_stats_key(column))? else {
        return Ok(Vec::new());
    };
    let field_stats: FieldStats =
        postcard::from_bytes(&stats_bytes).map_err(|e| Error::Codec(e.to_string()))?;
    score_terms(query_stats.terms, params, field_stats, |term| {
        let Some(stats) = term_stats(reader, column, term)? else {
            return Ok((0, Vec::new()));
        };
        let mut postings = Vec::with_capacity(stats.doc_freq as usize);
        for block_id in 0..stats.block_count {
            let block = read_posting_block(reader, column, term, block_id)?;
            postings.extend(block.postings);
        }
        Ok((stats.doc_freq, postings))
    })
}

pub fn search_sst_top_k(
    reader: &SstReader,
    column: &str,
    query: &str,
    k: usize,
    params: Bm25Params,
) -> Result<Vec<TextHit>> {
    Ok(search_sst_top_k_with_stats(reader, column, query, k, params)?.hits)
}

pub fn search_sst_top_k_with_stats(
    reader: &SstReader,
    column: &str,
    query: &str,
    k: usize,
    params: Bm25Params,
) -> Result<TextSearchOutcome> {
    let params = params.validate()?;
    if k == 0 {
        return Ok(TextSearchOutcome {
            hits: Vec::new(),
            stats: TextSearchStats::default(),
        });
    }
    if params != Bm25Params::default() {
        let mut hits = search_sst(reader, column, query, params)?;
        hits.truncate(k);
        return Ok(TextSearchOutcome {
            hits,
            stats: TextSearchStats::default(),
        });
    }

    let query_stats = analyze_text(query, TokenizerConfig::default());
    if query_stats.terms.is_empty() {
        return Ok(TextSearchOutcome {
            hits: Vec::new(),
            stats: TextSearchStats::default(),
        });
    }
    let Some(stats_bytes) = reader.get(&field_stats_key(column))? else {
        return Ok(TextSearchOutcome {
            hits: Vec::new(),
            stats: TextSearchStats::default(),
        });
    };
    let field_stats: FieldStats =
        postcard::from_bytes(&stats_bytes).map_err(|e| Error::Codec(e.to_string()))?;
    if field_stats.doc_count == 0 || field_stats.total_doc_len == 0 {
        return Ok(TextSearchOutcome {
            hits: Vec::new(),
            stats: TextSearchStats::default(),
        });
    }

    let mut terms = Vec::new();
    for query_term in query_stats.terms {
        let Some(stats) = term_stats(reader, column, &query_term.term)? else {
            continue;
        };
        let block_count = stats.block_count as usize;
        if block_count != stats.block_max_scores.len() {
            return Err(Error::Corrupt(format!(
                "text term metadata for {column}:{} has {} blocks but {} max scores",
                query_term.term,
                stats.block_count,
                stats.block_max_scores.len()
            )));
        }
        let qtf = query_term_frequency_weight(query_term.frequency, params.k3);
        terms.push(QueryTerm {
            term: query_term.term,
            doc_freq: stats.doc_freq,
            qtf,
            global_max_score: stats.max_score * qtf,
            block_max_scores: stats
                .block_max_scores
                .into_iter()
                .map(|score| score * qtf)
                .collect(),
        });
    }
    if terms.is_empty() {
        return Ok(TextSearchOutcome {
            hits: Vec::new(),
            stats: TextSearchStats::default(),
        });
    }

    terms.sort_by(|a, b| {
        b.global_max_score
            .total_cmp(&a.global_max_score)
            .then_with(|| a.term.cmp(&b.term))
    });
    let total_global_bound = terms.iter().map(|term| term.global_max_score).sum::<f32>();
    let avg_doc_len = average_doc_len(field_stats);
    let mut scores: BTreeMap<Id, f32> = BTreeMap::new();
    let mut top_k = TopKTracker::new(k);
    let mut stats = TextSearchStats::default();

    for term in terms {
        let other_terms_bound = (total_global_bound - term.global_max_score).max(0.0);
        let Some(scorer) =
            Bm25TermScorer::new(avg_doc_len, field_stats.doc_count, term.doc_freq, params)
        else {
            continue;
        };
        for (block_id, block_max_score) in term.block_max_scores.iter().copied().enumerate() {
            if top_k
                .threshold()
                .is_some_and(|score| block_max_score + other_terms_bound < score)
            {
                stats.blocks_skipped += 1;
                continue;
            }

            let block = read_posting_block(reader, column, &term.term, block_id as u32)?;
            stats.blocks_read += 1;
            for batch in block.postings.chunks(SCORE_BATCH_TARGET) {
                stats.score_batches += 1;
                stats.postings_scored +=
                    score_posting_batch(batch, scorer, term.qtf, &mut scores, &mut top_k);
            }
        }
    }

    let mut hits = hits_from_scores(scores);
    hits.truncate(k);
    Ok(TextSearchOutcome { hits, stats })
}

pub fn term_stats(reader: &SstReader, column: &str, term: &str) -> Result<Option<TextTermStats>> {
    let Some(bytes) = reader.get(&term_meta_key(column, term))? else {
        return Ok(None);
    };
    let stats: StoredTermStats =
        postcard::from_bytes(&bytes).map_err(|e| Error::Codec(e.to_string()))?;
    Ok(Some(TextTermStats {
        doc_freq: stats.doc_freq,
        block_count: stats.block_count,
        max_score: stats.max_score,
        block_max_scores: stats.block_max_scores,
    }))
}

pub fn score_documents(
    docs: &BTreeMap<Id, Document>,
    column: &str,
    query: &str,
    params: Bm25Params,
) -> Result<Vec<TextHit>> {
    let params = params.validate()?;
    let data = build_index_data(docs, TokenizerConfig::default());
    let Some(field_stats) = data.field_stats.get(column).copied() else {
        return Ok(Vec::new());
    };
    let query_stats = analyze_text(query, TokenizerConfig::default());
    score_terms(query_stats.terms, params, field_stats, |term| {
        let postings = data
            .postings
            .get(&(column.to_string(), term.to_string()))
            .cloned()
            .unwrap_or_default();
        Ok((postings.len() as u64, postings))
    })
}

fn build_index_data(docs: &BTreeMap<Id, Document>, config: TokenizerConfig) -> TextIndexData {
    let mut data = TextIndexData::default();
    for (id, doc) in docs {
        for (column, value) in &doc.attributes {
            let tokens = tokens_for_value(value, config);
            if tokens.is_empty() {
                continue;
            }
            let analyzed = analyze_tokens(tokens);
            if analyzed.doc_len == 0 {
                continue;
            }
            let stats = data.field_stats.entry(column.clone()).or_default();
            stats.doc_count = stats.doc_count.saturating_add(1);
            stats.total_doc_len = stats
                .total_doc_len
                .saturating_add(u64::from(analyzed.doc_len));
            for term in analyzed.terms {
                data.postings
                    .entry((column.clone(), term.term))
                    .or_default()
                    .push(TermPosting {
                        id: id.clone(),
                        term_frequency: term.frequency,
                        doc_len: analyzed.doc_len,
                    });
            }
        }
    }
    data
}

fn score_terms(
    query_terms: Vec<TermFrequency>,
    params: Bm25Params,
    field_stats: FieldStats,
    mut postings_for_term: impl FnMut(&str) -> Result<(u64, Vec<TermPosting>)>,
) -> Result<Vec<TextHit>> {
    if query_terms.is_empty() || field_stats.doc_count == 0 || field_stats.total_doc_len == 0 {
        return Ok(Vec::new());
    }
    let avg_doc_len = average_doc_len(field_stats);
    let mut scores: BTreeMap<Id, f32> = BTreeMap::new();
    for query_term in query_terms {
        let (doc_freq, postings) = postings_for_term(&query_term.term)?;
        if doc_freq == 0 {
            continue;
        }
        let qtf = query_term_frequency_weight(query_term.frequency, params.k3);
        let Some(scorer) =
            Bm25TermScorer::new(avg_doc_len, field_stats.doc_count, doc_freq, params)
        else {
            continue;
        };
        for posting in postings {
            let score = scorer.score(posting.term_frequency, posting.doc_len) * qtf;
            if score > 0.0 {
                *scores.entry(posting.id).or_default() += score;
            }
        }
    }

    let mut hits: Vec<TextHit> = scores
        .into_iter()
        .map(|(id, score)| TextHit { id, score })
        .collect();
    sort_hits(&mut hits);
    Ok(hits)
}

fn average_doc_len(field_stats: FieldStats) -> f32 {
    if field_stats.doc_count == 0 {
        return 0.0;
    }
    field_stats.total_doc_len as f32 / field_stats.doc_count as f32
}

#[derive(Clone, Debug)]
struct QueryTerm {
    term: String,
    doc_freq: u64,
    qtf: f32,
    global_max_score: f32,
    block_max_scores: Vec<f32>,
}

fn read_posting_block(
    reader: &SstReader,
    column: &str,
    term: &str,
    block_id: u32,
) -> Result<PostingBlock> {
    let Some(bytes) = reader.get(&term_block_key(column, term, block_id))? else {
        return Err(Error::Corrupt(format!(
            "missing text posting block {block_id} for {column}:{term}"
        )));
    };
    postcard::from_bytes(&bytes).map_err(|e| Error::Codec(e.to_string()))
}

fn score_posting_batch(
    postings: &[TermPosting],
    scorer: Bm25TermScorer,
    query_weight: f32,
    scores: &mut BTreeMap<Id, f32>,
    top_k: &mut TopKTracker,
) -> u64 {
    let mut scored = 0u64;
    for posting in postings {
        let score = scorer.score(posting.term_frequency, posting.doc_len) * query_weight;
        if score <= 0.0 {
            continue;
        }
        let new_score = {
            let entry = scores.entry(posting.id.clone()).or_default();
            *entry += score;
            *entry
        };
        top_k.observe(posting.id.clone(), new_score);
        scored += 1;
    }
    scored
}

fn hits_from_scores(scores: BTreeMap<Id, f32>) -> Vec<TextHit> {
    let mut hits: Vec<TextHit> = scores
        .into_iter()
        .map(|(id, score)| TextHit { id, score })
        .collect();
    sort_hits(&mut hits);
    hits
}

fn query_term_frequency_weight(frequency: u32, k3: f32) -> f32 {
    let qtf = frequency as f32;
    (qtf * (k3 + 1.0)) / (qtf + k3)
}

fn sort_hits(hits: &mut [TextHit]) {
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
}

fn tokens_for_value(value: &Value, config: TokenizerConfig) -> Vec<String> {
    match value {
        Value::String(text) => tokenize(text, config),
        Value::Array(values) => values
            .iter()
            .filter_map(|value| match value {
                Value::String(text) => Some(tokenize(text, config)),
                _ => None,
            })
            .flatten()
            .collect(),
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Vec::new(),
    }
}

fn finish_token(tokens: &mut Vec<String>, current: &mut String, max_token_len: usize) {
    if current.is_empty() {
        return;
    }
    if current.len() <= max_token_len {
        tokens.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

fn field_stats_key(column: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4 + column.len());
    key.push(KEY_KIND_FIELD_STATS);
    push_len_prefixed(column.as_bytes(), &mut key);
    key
}

fn term_meta_key(column: &str, term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 8 + column.len() + term.len());
    key.push(KEY_KIND_TERM_META);
    push_len_prefixed(column.as_bytes(), &mut key);
    push_len_prefixed(term.as_bytes(), &mut key);
    key
}

fn term_block_key(column: &str, term: &str, block_id: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 12 + column.len() + term.len());
    key.push(KEY_KIND_TERM_BLOCK);
    push_len_prefixed(column.as_bytes(), &mut key);
    push_len_prefixed(term.as_bytes(), &mut key);
    key.extend_from_slice(&block_id.to_be_bytes());
    key
}

fn push_len_prefixed(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}
