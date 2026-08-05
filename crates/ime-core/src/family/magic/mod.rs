//! MagicFamily — `#`-prefixed magic commands, unified as a registry of members.
//!
//! Every command is a [`MagicMember`]:
//! - **static** members resolve to a fixed expansion inline (`#date`, `#password`),
//! - **live** members own an interactive session (`#asr` voice anchor, `#req` HTTP
//!   request) — after the trigger completes, the FSM enters `ComposeState::Magic`
//!   and routes keys + async ticks to the spawned member instance.
//!
//! The matcher entries, prediction hints and activation dispatch are ALL generated
//! from this registry — adding a command is one struct + one registration, with no
//! engine / FSM special-casing.

mod member;
mod req;
mod voice;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use member::{preview_text, CANDIDATE_PREVIEW_MAX, MagicMember, MemberAction};
pub use req::{ReqFetcher, DEFAULT_REQ_BASE};
pub use voice::{SubmitMember, VoiceMember};

use req::ReqMember;

use super::{CandidateFamily, ScoredCandidate};

/// Shared voice-session slot — written by the aura SSE client (via
/// [`MagicFamily::set_asr_buffer`], late after engine construction), read by the
/// voice-family member instances. Lives behind an `Arc` so the engine and every
/// per-context member see the same buffer.
#[derive(Default)]
pub struct VoiceSlot(Mutex<Option<Arc<crate::asr_buffer::AsrBuffer>>>);

impl VoiceSlot {
    pub fn set(&self, buf: Arc<crate::asr_buffer::AsrBuffer>) {
        *self.0.lock().unwrap() = Some(buf);
    }

    pub fn get(&self) -> Option<Arc<crate::asr_buffer::AsrBuffer>> {
        self.0.lock().unwrap().clone()
    }
}

/// Resources shared between the engine and all member instances (across input
/// contexts): the voice buffer slot and the `#req` backend config. Members grab
/// `Arc` clones at spawn, so late attachment (start-up ordering) is fine.
pub struct MagicResources {
    pub voice: Arc<VoiceSlot>,
    pub req_base: Mutex<String>,
    pub req_fetcher: Mutex<Arc<dyn ReqFetcher>>,
}

fn default_fetcher() -> Arc<dyn ReqFetcher> {
    #[cfg(feature = "http")]
    {
        Arc::new(req::ReqwestFetcher::new(std::time::Duration::from_secs(5)))
    }
    #[cfg(not(feature = "http"))]
    {
        Arc::new(req::NoopFetcher)
    }
}

impl Default for MagicResources {
    fn default() -> Self {
        MagicResources {
            voice: Arc::new(VoiceSlot::default()),
            req_base: Mutex::new(DEFAULT_REQ_BASE.to_string()),
            req_fetcher: Mutex::new(default_fetcher()),
        }
    }
}

/// A static command: fixed expansion text, no interactive session. The expansion
/// is computed on demand (matcher entries freeze it at engine build; prediction
/// hints resolve it fresh).
pub struct StaticCmd {
    pub trigger: &'static str,
    pub description: &'static str,
    expansion: Arc<dyn Fn() -> String + Send + Sync>,
}

impl StaticCmd {
    pub fn new(
        trigger: &'static str,
        description: &'static str,
        expansion: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        StaticCmd { trigger, description, expansion: Arc::new(expansion) }
    }

    pub fn expansion(&self) -> String {
        (self.expansion)()
    }
}

impl Clone for StaticCmd {
    fn clone(&self) -> Self {
        StaticCmd { trigger: self.trigger, description: self.description, expansion: Arc::clone(&self.expansion) }
    }
}

/// Today's date (YYYY-MM-DD). Naive approximation (no chrono dep) — same
/// computation the previous static implementation used.
fn today_str() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let year = 1970 + (days / 365);
    let rem = days % 365;
    let month = 1 + (rem / 30).min(11);
    let day = 1 + (rem % 30).min(27);
    format!("{year:04}-{month:02}-{day:02}")
}

pub struct MagicFamily {
    enabled: bool,
    /// Static commands (inline expansion).
    statics: Vec<StaticCmd>,
    /// Live commands, each with an activation token.
    members: Vec<Arc<dyn MagicMember>>,
    token_map: HashMap<&'static str, usize>,
    /// Shared resources for member instances (voice slot, req config).
    resources: Arc<MagicResources>,
}

impl MagicFamily {
    pub fn new() -> Self {
        let resources = Arc::new(MagicResources::default());
        let members: Vec<Arc<dyn MagicMember>> = vec![
            Arc::new(VoiceMember::new(Arc::clone(&resources))),
            Arc::new(SubmitMember::new(Arc::clone(&resources))),
            Arc::new(ReqMember::new(Arc::clone(&resources))),
        ];
        let mut token_map = HashMap::new();
        for (i, m) in members.iter().enumerate() {
            if let Some(tok) = m.activation_token() {
                token_map.insert(tok, i);
            }
        }
        MagicFamily {
            enabled: true,
            statics: vec![
                StaticCmd::new("#date", "insert today's date", today_str),
                StaticCmd::new("#password", "password manager", || {
                    "[password manager — not yet implemented]".into()
                }),
            ],
            members,
            token_map,
            resources,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// All matcher entries: static triggers → their expansion; live triggers →
    /// the activation token (plus aliases, e.g. `#flush` → voice token).
    pub fn matcher_entries(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in &self.statics {
            out.push((s.trigger.to_string(), s.expansion()));
        }
        for m in &self.members {
            let token = m.activation_token().expect("live member needs an activation token");
            out.push((format!("#{}", m.name()), token.to_string()));
            for alias in m.aliases() {
                out.push((format!("#{alias}"), token.to_string()));
            }
        }
        out
    }

    /// Spawn a fresh member instance for an activation token (matcher `Complete`).
    /// `None` if the token isn't a live command (static expansion path handles it).
    pub fn spawn(&self, token: &str) -> Option<Box<dyn MagicMember>> {
        let idx = *self.token_map.get(token)?;
        Some(self.members[idx].spawn())
    }

    /// All magic commands whose trigger is a strict extension of `prefix` — the prediction
    /// hints shown while typing `#…`. Live members carry their activation token (Space on the
    /// hint completes INTO the command's Magic mode); static commands carry `None` (Space
    /// resolves their expansion instead). The raw buffer stays a rollback candidate.
    pub fn hints(&self, prefix: &str) -> Vec<(String, Option<&'static str>)> {
        if prefix.is_empty() || !prefix.starts_with('#') {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in &self.statics {
            if s.trigger.starts_with(prefix) && s.trigger != prefix {
                out.push((s.trigger.to_string(), None));
            }
        }
        for m in &self.members {
            if let Some(token) = m.activation_token() {
                let t = format!("#{}", m.name());
                if t.starts_with(prefix) && t != prefix {
                    out.push((t.clone(), Some(token)));
                }
                for alias in m.aliases() {
                    let ta = format!("#{alias}");
                    if ta.starts_with(prefix) && ta != prefix {
                        out.push((ta.clone(), Some(token)));
                    }
                }
            }
        }
        out
    }

    /// Static expansion text for a full trigger (e.g. `#date` → today's date).
    pub fn static_expansion(&self, trigger: &str) -> Option<String> {
        self.statics.iter().find(|s| s.trigger == trigger).map(|s| s.expansion())
    }

    /// Attach the voice buffer — routed to the shared slot all voice members read.
    pub fn set_asr_buffer(&self, buf: Arc<crate::asr_buffer::AsrBuffer>) {
        self.resources.voice.set(buf);
    }

    /// `#req` backend base URL (default `http://127.0.0.1:14555/api`).
    pub fn set_req_base(&self, base: &str) {
        *self.resources.req_base.lock().unwrap() = base.to_string();
    }

    /// Inject an HTTP fetcher (tests use a fake; production default is reqwest
    /// behind the `http` feature).
    pub fn set_req_fetcher(&self, fetcher: Arc<dyn ReqFetcher>) {
        *self.resources.req_fetcher.lock().unwrap() = fetcher;
    }

    /// Shared resources — member instances and the engine talk through these.
    pub fn resources(&self) -> Arc<MagicResources> {
        Arc::clone(&self.resources)
    }
}

impl Clone for MagicFamily {
    fn clone(&self) -> Self {
        MagicFamily {
            enabled: self.enabled,
            statics: self.statics.clone(),
            members: self.members.clone(),
            token_map: self.token_map.clone(),
            resources: Arc::clone(&self.resources),
        }
    }
}

impl Default for MagicFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for MagicFamily {
    fn name(&self) -> &'static str {
        "magic"
    }

    fn priority(&self) -> u32 {
        95
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || !input.starts_with('#') {
            return Vec::new();
        }

        let mut out = Vec::new();
        for s in &self.statics {
            if s.trigger == input {
                // Exact match — resolve the expansion.
                out.push(ScoredCandidate {
                    text: s.expansion(),
                    family: "magic",
                    source: "exact",
                    raw_score: 1.0,
                });
            } else if s.trigger.starts_with(input) {
                // Prefix match — show the trigger as a hint.
                out.push(ScoredCandidate {
                    text: s.trigger.to_string(),
                    family: "magic",
                    source: "prefix",
                    raw_score: 0.9,
                });
            }
        }
        for m in &self.members {
            let trigger = format!("#{}", m.name());
            if trigger == input {
                out.push(ScoredCandidate {
                    text: format!("{} — {}", trigger, m.description()),
                    family: "magic",
                    source: "exact",
                    raw_score: 1.0,
                });
            } else if trigger.starts_with(input) {
                out.push(ScoredCandidate {
                    text: trigger,
                    family: "magic",
                    source: "prefix",
                    raw_score: 0.9,
                });
            }
        }
        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_entries_cover_all_commands() {
        let fam = MagicFamily::new();
        let entries: Vec<(String, String)> = fam.matcher_entries();
        assert!(entries.contains(&("#date".into(), today_str())), "{entries:?}");
        assert!(entries.contains(&("#asr".into(), "__ASR_BUFFER__".into())));
        assert!(entries.contains(&("#flush".into(), "__ASR_BUFFER__".into())), "alias");
        assert!(entries.contains(&("#submit".into(), "__ASR_SUBMIT__".into())));
        assert!(entries.contains(&("#req".into(), "__REQ__".into())));
    }

    #[test]
    fn spawn_resolves_live_tokens_only() {
        let fam = MagicFamily::new();
        assert!(fam.spawn("__ASR_BUFFER__").is_some());
        assert!(fam.spawn("__REQ__").is_some());
        // Static commands and unknown tokens are not live commands.
        assert!(fam.spawn("__ASR_SUBMIT__").is_some());
        assert!(fam.spawn("__NOPE__").is_none());
    }

    #[test]
    fn exact_date_command() {
        let fam = MagicFamily::new();
        let cands = fam.predict("#date");
        assert_eq!(cands.len(), 1);
        assert!(cands[0].text.starts_with("202"));
        assert!((cands[0].raw_score - 1.0).abs() < 0.001);
    }

    #[test]
    fn live_command_hint() {
        let fam = MagicFamily::new();
        let cands = fam.predict("#asr");
        assert!(cands.iter().any(|c| c.text.contains("voice input")), "{cands:?}");
        let cands = fam.predict("#req");
        assert!(cands.iter().any(|c| c.text.contains("request")), "{cands:?}");
    }

    #[test]
    fn prefix_match() {
        let fam = MagicFamily::new();
        let cands = fam.predict("#da");
        assert!(!cands.is_empty());
        assert!(cands.iter().any(|c| c.text == "#date"));
    }

    #[test]
    fn only_activated_by_hash() {
        let fam = MagicFamily::new();
        assert!(fam.predict("hello").is_empty());
        assert!(fam.predict("/greet").is_empty());
    }

    #[test]
    fn resources_are_shared_across_clones() {
        // The scorer keeps a clone; the engine keeps the original — both must see
        // the same req base / voice slot after a late set_* call.
        let fam = MagicFamily::new();
        let clone = fam.clone();
        let buf = Arc::new(crate::asr_buffer::AsrBuffer::new());
        fam.set_asr_buffer(Arc::clone(&buf));
        assert!(clone.resources().voice.get().is_some(), "voice slot shared");
        fam.set_req_base("http://example.test:9/x");
        assert_eq!(*clone.resources().req_base.lock().unwrap(), "http://example.test:9/x");
    }
}
