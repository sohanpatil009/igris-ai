//! Local, privacy-first memory store for IGRIS AI.
//!
//! Facts are stored with a 384-dim SBERT embedding (the same fallback
//! embedding used by the NLU engine) so `search_facts` ranks by cosine
//! similarity instead of a raw substring test. Legacy `memory.json` files
//! whose `facts` array contains strings are migrated in place (a one-time
//! `.bak` is kept) on first load.

use crate::nlu::sbert::SbertEngine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTurn {
    pub role: String,
    pub text: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryFact {
    id: u64,
    text: String,
    #[serde(default)]
    embedding: Vec<f32>,
}

#[derive(Serialize, Deserialize)]
struct MemoryState {
    #[serde(default)]
    turns: Vec<MemoryTurn>,
    #[serde(default)]
    facts: Vec<MemoryFact>,
    #[serde(default = "default_next_fact_id")]
    next_fact_id: u64,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            turns: Vec::new(),
            facts: Vec::new(),
            next_fact_id: default_next_fact_id(),
        }
    }
}

fn default_next_fact_id() -> u64 {
    1
}

pub struct MemoryStore {
    path: PathBuf,
    state: RwLock<MemoryState>,
}

impl MemoryStore {
    pub fn load() -> Self {
        let base = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("igris-ai");
        let _ = fs::create_dir_all(&base);
        let path = base.join("memory.json");
        let state = load_state(&path);
        Self {
            path,
            state: RwLock::new(state),
        }
    }

    pub fn add_turn(&self, role: &str, text: &str) {
        let turn = MemoryTurn {
            role: role.to_string(),
            text: text.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(mut s) = self.state.write() {
            s.turns.push(turn);
            if s.turns.len() > 500 {
                s.turns.drain(0..100);
            }
            persist(&self.path, &*s);
        }
    }

    pub fn recent(&self, n: usize) -> Vec<MemoryTurn> {
        self.state
            .read()
            .map(|s| {
                let take = n.min(s.turns.len());
                s.turns[s.turns.len() - take..].to_vec()
            })
            .unwrap_or_default()
    }

    /// Remember a fact. Returns the fact id so callers can `forget <id>`.
    pub fn add_fact(&self, fact: &str) -> u64 {
        let fact = fact.trim();
        if fact.is_empty() {
            return 0;
        }
        let embedding = embed(fact);
        if let Ok(mut s) = self.state.write() {
            let id = s.next_fact_id;
            s.next_fact_id = s.next_fact_id.saturating_add(1);
            s.facts.push(MemoryFact {
                id,
                text: fact.to_string(),
                embedding,
            });
            persist(&self.path, &*s);
            id
        } else {
            0
        }
    }

    /// List stored facts as (id, text).
    pub fn facts(&self) -> Vec<(u64, String)> {
        self.state
            .read()
            .map(|s| s.facts.iter().map(|f| (f.id, f.text.clone())).collect())
            .unwrap_or_default()
    }

    /// Forget a fact by id. Returns true if it existed.
    pub fn forget(&self, id: u64) -> bool {
        if let Ok(mut s) = self.state.write() {
            let before = s.facts.len();
            s.facts.retain(|f| f.id != id);
            let changed = s.facts.len() != before;
            if changed {
                persist(&self.path, &*s);
            }
            changed
        } else {
            false
        }
    }

    /// Semantic fact search. Ranks by cosine similarity with a lexical
    /// bonus so exact recall keeps working even when embeddings are weak.
    pub fn search_facts(&self, query: &str) -> Vec<String> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        let q_emb = embed(query);
        let q_low = query.to_lowercase();
        let mut scored: Vec<(f32, String)> = self
            .state
            .read()
            .map(|s| {
                s.facts
                    .iter()
                    .map(|f| {
                        let mut score = SbertEngine::cosine_similarity(&q_emb, &f.embedding);
                        let t = f.text.to_lowercase();
                        if t.contains(&q_low) {
                            score += 0.5;
                        }
                        // Token-overlap bonus.
                        for tok in q_low.split_whitespace() {
                            if tok.len() > 2 && t.contains(tok) {
                                score += 0.1;
                            }
                        }
                        (score, f.text.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // Keep facts above a loose threshold or, failing that, the best few.
        let mut out: Vec<String> = scored
            .iter()
            .filter(|(s, _)| *s > 0.15)
            .map(|(_, t)| t.clone())
            .collect();
        if out.is_empty() && !scored.is_empty() {
            out.push(scored[0].1.clone());
        }
        out
    }

    pub fn clear(&self) {
        if let Ok(mut s) = self.state.write() {
            s.turns.clear();
            s.facts.clear();
            s.next_fact_id = 1;
            persist(&self.path, &*s);
        }
    }
}

fn embed(text: &str) -> Vec<f32> {
    let mut sbert = SbertEngine::new();
    sbert.embed(text)
}

fn persist(path: &PathBuf, state: &MemoryState) {
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            let _ = fs::write(path, json);
        }
        Err(e) => eprintln!("[memory] failed to serialize state: {e}"),
    }
}

fn load_state(path: &PathBuf) -> MemoryState {
    if !path.exists() {
        return MemoryState::default();
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return MemoryState::default();
    };
    let Ok(v): Result<Value, _> = serde_json::from_str(&raw) else {
        return MemoryState::default();
    };

    // Parse turns (best-effort).
    let turns: Vec<MemoryTurn> = v
        .get("turns")
        .and_then(|x| serde_json::from_value(x.clone()).ok())
        .unwrap_or_default();

    // Facts may be legacy strings or new objects. Migrate strings on the fly.
    let facts_value = v.get("facts").cloned().unwrap_or(Value::Array(vec![]));
    let facts = match facts_value {
        Value::Array(arr) if arr.iter().all(|x| x.is_string()) => {
            // Legacy layout: back up the original file once, then synthesize
            // embeddings so search becomes semantic.
            let backup = path.with_extension("json.bak");
            if !backup.exists() {
                let _ = fs::write(&backup, &raw);
            }
            let mut next_id = 1u64;
            arr.into_iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .map(|text| {
                    let id = next_id;
                    next_id += 1;
                    MemoryFact {
                        id,
                        text: text.clone(),
                        embedding: embed(&text),
                    }
                })
                .collect()
        }
        Value::Array(arr) => {
            // New layout; ensure every fact carries an embedding.
            arr.into_iter()
                .filter_map(|x| serde_json::from_value::<MemoryFact>(x).ok())
                .map(|mut f| {
                    if f.embedding.is_empty() {
                        f.embedding = embed(&f.text);
                    }
                    f
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let requested_next = v.get("next_fact_id").and_then(Value::as_u64).unwrap_or(0);
    let max_id = facts.iter().map(|f| f.id).max().unwrap_or(0);
    let next_fact_id = requested_next.max(max_id.saturating_add(1)).max(1);

    MemoryState {
        turns,
        facts,
        next_fact_id,
    }
}
