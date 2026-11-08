//! Host bridge for igris-ai.
//!
//! The assistant brain (core/nlu/online) needs three things that belong to
//! the companion app, not the brain: the chosen speaker voice, the
//! assistant identity/persona, and the list of installed command plugins
//! for entity extraction. Each companion installs the bridge once at
//! startup; the brain reads through these pointers only — so the
//! dependency stays one-way (app → igris-ai).

use std::sync::OnceLock;

/// Assistant personality presets (moved here so the persona texts travel
/// with the brain). The companion's config layer re-exports this type.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Personality {
    #[default]
    Igris,
    Alita,
    Custom(String),
}

impl Personality {
    pub fn name(&self) -> &str {
        match self {
            Personality::Igris => "IGRIS",
            Personality::Alita => "Alita",
            Personality::Custom(name) => name,
        }
    }

    pub fn speaker_id(&self) -> &str {
        match self {
            Personality::Igris => "051", // Deep male voice
            Personality::Alita => "001", // Female voice
            Personality::Custom(_) => "051",
        }
    }

    pub fn wake_word(&self) -> &str {
        match self {
            Personality::Igris => "hello",
            Personality::Alita => "alita",
            Personality::Custom(_) => "hello",
        }
    }

    /// Get all wake word variations for this personality
    pub fn wake_word_variations(&self) -> Vec<&str> {
        match self {
            Personality::Igris => vec!["hello", "hello igris", "hi igris", "igris"],
            Personality::Alita => vec!["alita", "hello alita", "hi alita", "hello"],
            Personality::Custom(_) => vec!["hello"],
        }
    }

    pub fn greeting(&self) -> &str {
        match self {
            Personality::Igris => "IGRIS at your service master. Say hello when you need me.",
            Personality::Alita => "Hey! Alita here. Just say hello when you need me!",
            Personality::Custom(name) => name,
        }
    }

    pub fn invoke_response(&self) -> &str {
        match self {
            Personality::Igris => "Yes, I'm listening. What can I do for you?",
            Personality::Alita => "Hey! What's up? How can I help?",
            Personality::Custom(_) => "I'm listening.",
        }
    }
}

impl serde::Serialize for Personality {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Personality::Igris => serializer.serialize_str("Igris"),
            Personality::Alita => serializer.serialize_str("Alita"),
            Personality::Custom(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Personality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "Igris" => Ok(Personality::Igris),
            "Alita" => Ok(Personality::Alita),
            other => Ok(Personality::Custom(other.to_string())),
        }
    }
}

/// Slim, serializable snapshot of a command plugin: only what NLU entity
/// extraction reads (name, keywords, and trigger/action/example lists).
#[derive(Debug, Clone)]
pub struct PluginSnapshot {
    pub name: String,
    pub keywords: Vec<String>,
    pub commands: Vec<PluginCommandSnapshot>,
}

#[derive(Debug, Clone)]
pub struct PluginCommandSnapshot {
    pub trigger: String,
    pub action_type: String,
    /// Alternative example phrases the NLU can match on.
    pub examples: Vec<String>,
}

/// Stage 1: host settings the brain can't derive on its own.
pub struct HostConfig {
    /// Piper speaker id for the chosen persona (e.g. "051").
    pub speaker_id: fn() -> String,
    /// Assistant display name per persona.
    pub assistant_name: fn() -> String,
    /// Active personality.
    pub personality: fn() -> Personality,
}

/// Stage 2: plugin projection from the companion's plugin system.
pub type PluginLister = fn() -> Vec<PluginSnapshot>;

struct Bridge {
    config: HostConfig,
    plugins: Option<PluginLister>,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

/// Install the host bridge (idempotent for a single process). Call before
/// any assistant engine initializes.
pub fn install(config: HostConfig, plugins: PluginLister) {
    let _ = BRIDGE.set(Bridge {
        config,
        plugins: Some(plugins),
    });
}

pub fn config() -> &'static HostConfig {
    BRIDGE.get().map(|b| &b.config).expect(
        "igris-ai host bridge not installed — call igris_ai::bridge::install() before \
         initializing the assistant",
    )
}

/// Whether the host bridge has been installed. Useful for tools, examples, and
/// tests that run the brain without a companion app.
pub fn is_installed() -> bool {
    BRIDGE.get().is_some()
}

/// Plugin snapshots for entity extraction; empty when the host provides none.
pub fn plugin_snapshots() -> Vec<PluginSnapshot> {
    BRIDGE
        .get()
        .and_then(|b| b.plugins)
        .map(|f| f())
        .unwrap_or_default()
}
