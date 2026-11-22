//! Lightweight workflow skills for the two shipped local models.
//!
//! A "skill" here is just a short instruction block appended to the system
//! prompt when the user's request matches a workflow. No loader, no files,
//! no progressive disclosure — small 2B/4B models do better with a few
//! explicit rules than with a skill-selection meta-task. Skill choice is
//! made once at session seed so the KV-cache prefix stays stable.
//!
//! The wording targets failures measured in `benchmark.md`: wrong tool
//! choice for memory/file tasks, CDP-session confusion for browser tasks,
//! and repeated web searches on research tasks.

/// Browser/CDP workflow: `open_url` launches a browser but creates no
/// session, so a following `browser_read` fails. Playback needs a click.
const BROWSER: &str = "\
## Workflow skill: browser\n\
Pick the most direct browser tool for the request. Do not repeat a tool call with identical \
arguments - if its result is already present, use it. A search-results page is not playback: \
a video must be opened or clicked before claiming it is playing. If a click fails, change the \
selector or report the failure instead of retrying unchanged.";

/// File workflow: an explicit directory should be listed, not searched
/// recursively; and the answer should summarise, not echo whole files.
const FILES: &str = "\
## Workflow skill: files\n\
Use the most direct file tool that answers the request; avoid extra calls when a listing is \
enough. If you do read a file, summarise the relevant part instead of pasting the whole file. \
Do not repeat a tool call with identical arguments.";

/// Media workflow: volume and transport keys are dedicated tools.
const MEDIA: &str = "\
## Workflow skill: media\n\
Use set_volume for a percentage and media_key for play_pause / next_track / previous_track / stop. \
Do not just say you skipped a song - call media_key, then confirm briefly.";

/// Memory workflow: the measured LFM3 failure was answering from memory
/// instead of calling memory_save for 'remember X'.
const MEMORY: &str = "\
## Workflow skill: memory\n\
When the user says remember / note / keep in mind, or states a personal fact or date, call \
memory_save. When asked what/where/when about something personal, call memory_recall first - \
never claim you do not know before checking. Use memory_save for storing and memory_recall \
for retrieving; they are not interchangeable.";

/// Research workflow: limit search rounds and cite sources.
const RESEARCH: &str = "\
## Workflow skill: research\n\
Call web_search once and answer from the snippets. Do not repeat the same search query in \
later rounds, and do not open extra pages unless the user asks for a specific document. \
Summarise briefly with the source URL; prefer official domains for version and release questions.";

/// Generic skill for the always-on voice loop, where the first utterance
/// is not known when the system prompt is seeded.
const VOICE: &str = "\
## Workflow skill: voice\n\
Keep replies short and spoken. When an action is needed, call exactly one tool with the \
correct name and arguments, then use its result to answer. Do not repeat a call with the \
same arguments - if a tool result is already present, use it.";

/// Pick a skill from the user's text. Returns `None` when no workflow
/// matches, so the system prompt stays byte-identical to the base for
/// unrelated chats.
pub fn skill_for(text: &str) -> Option<&'static str> {
    let t = text.to_ascii_lowercase();
    if [
        "browser", "youtube", "spotify", "chrome", "brave", "firefox", "edge",
        "web page", "webpage", "website", "open http", "click",
    ]
    .iter()
    .any(|k| t.contains(k))
    {
        return Some(BROWSER);
    }
    if ["song", "music", "volume", "pause", "skip", "next track", "play "]
        .iter()
        .any(|k| t.contains(k))
    {
        return Some(MEDIA);
    }
    if ["remember", "recall", "my exam", "note that", "don't forget"]
        .iter()
        .any(|k| t.contains(k))
    {
        return Some(MEMORY);
    }
    if ["find", "read", "file", "folder", "directory", "project structure", "repo"]
        .iter()
        .any(|k| t.contains(k))
    {
        return Some(FILES);
    }
    if ["search", "research", "latest", "look up", "summarize", "summarise", "news"]
        .iter()
        .any(|k| t.contains(k))
    {
        return Some(RESEARCH);
    }
    None
}

/// Skill used by the voice assistant, which seeds the system prompt before
/// the first utterance exists.
pub fn voice_skill() -> &'static str {
    VOICE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_selection_is_specific_and_stable() {
        assert_eq!(skill_for("Open a page in Chrome"), Some(BROWSER));
        assert_eq!(skill_for("play rabhata on youtube"), Some(BROWSER));
        assert_eq!(skill_for("Skip this song"), Some(MEDIA));
        assert_eq!(skill_for("Remember my exam is March 3rd"), Some(MEMORY));
        assert_eq!(skill_for("Find my README"), Some(FILES));
        assert_eq!(skill_for("Search latest AI models"), Some(RESEARCH));
        assert_eq!(skill_for("hello there"), None);
        assert!(voice_skill().contains("Do not repeat a call"));
    }
}
