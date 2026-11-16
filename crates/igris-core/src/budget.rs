//! Task-class budgets (plan P3 item 9: "reduce unnecessary model
//! rounds"). Live evidence (benchmark 2026-11-16/17, four data points
//! S4-15F, S3-11C, S3-13C, S5-20C): every miss is the 10-round tool cap
//! exhausting BEFORE the deliverable exists - reports never written,
//! answers never composed - while the rounds themselves were spent
//! honestly. The fix is not a bigger cap but a shaped one:
//!
//! - a per-task-class cap triple (rounds / tool calls / web calls);
//! - delivery-deadline notices injected into history when 2 rounds and
//!   when 1 round remain, so the model delivers instead of investigating
//!   to the wall;
//! - deterministic refusals once the web or tool-call budget is spent
//!   (the model never sees an unbounded page stream - the context
//!   cannot overflow from budgeted calls);
//! - a retry block for tools that reported a DEFINITIVE
//!   unsupported-capability error (the U1 probe burned 5 rounds hunting
//!   for a recycle-bin folder that no tool could see; a tool that says
//!   "unavailable on this system" must not be retried).
//!
//! Classification is deliberately cheap keyword matching with a safe
//! bias: a task misclassified into a LOOSER class only ever gets more
//! headroom than it needs, and a tighter class still keeps the same
//! 10-round default the whole benchmark suite was measured under. Only
//! report-shaped tasks (the four data points' class) get the raised
//! round budget.
//!
//! All of it is pure and hermetically tested; [`crate::llm`]'s agent
//! loop is the only caller.

/// Error-string prefix a tool uses to flag a DEFINITIVE
/// unsupported-capability failure (system gap, not a transient error):
/// e.g. a power plan that powercfg does not offer, a machine with no
/// default audio device. The agent loop turns the first such result
/// into a retry block for that tool, so the model cannot spend rounds
/// re-calling something the system can never answer.
pub const UNSUPPORTED_PREFIX: &str = "unsupported: ";

/// How a request is classified for budgeting purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClass {
    /// Single-tool Q&A and device/control asks ("what time is it",
    /// "set the volume"). The default class and the benchmark baseline.
    Simple,
    /// Web-research asks (latest versions, docs, comparisons without a
    /// file deliverable). Same round cap as Simple, a wider call budget
    /// for the extra fetch/search rounds, and a bounded web budget.
    Research,
    /// Report/file-deliverable asks ("...write it to report.md"). The
    /// loosest budget: the four cap-exhaustion data points were all
    /// report-shaped.
    Report,
}

/// The per-task cap triple (plan P3 item 9): maximum tool rounds,
/// maximum tool calls (parallel fan-out included), maximum web
/// calls (fetch + search together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_rounds: usize,
    pub max_tool_calls: usize,
    pub max_web_calls: usize,
}

impl Budget {
    /// The Simple-class budget doubles as the loop's historical
    /// default: `max_rounds` must equal
    /// [`crate::llm::MAX_TOOL_ITERATIONS`] so a Simple-classified turn
    /// behaves exactly as measured by the benchmark suite.
    pub fn simple() -> Budget {
        budget_for(TaskClass::Simple)
    }
}

pub fn budget_for(class: TaskClass) -> Budget {
    match class {
        TaskClass::Simple => Budget {
            max_rounds: 10,
            max_tool_calls: 12,
            max_web_calls: 4,
        },
        TaskClass::Research => Budget {
            max_rounds: 10,
            max_tool_calls: 24,
            max_web_calls: 6,
        },
        TaskClass::Report => Budget {
            max_rounds: 12,
            max_tool_calls: 32,
            max_web_calls: 8,
        },
    }
}

/// The page-fetching tools the web budget counts together. Other
/// network tools (github_repo_info) return small structured payloads,
/// not page streams, so they count against the tool-call budget only.
pub fn is_web_tool(name: &str) -> bool {
    name == "web_fetch" || name == "web_search"
}

/// Deterministic classification of one user request. Only the request
/// text is read - never tool output - so the class is stable across the
/// whole turn. Misclassification is safe by construction: Report is the
/// loosest class, so a false "report" match only adds headroom, and
/// every class keeps the 10-round default except Report (12).
pub fn classify(prompt: &str) -> TaskClass {
    let p = prompt.to_lowercase();
    const REPORT_WORDS: &[&str] = &[
        "report",
        "audit",
        "comparison file",
        "summary file",
        "write a file",
        "write the file",
        "write to a file",
        "create a file",
        "save it to",
        "save to a file",
        "save the file",
        ".txt",
        ".md",
        ".csv",
        ".json",
        ".xlsx",
        ".docx",
    ];
    if REPORT_WORDS.iter().any(|w| p.contains(w)) {
        return TaskClass::Report;
    }
    const RESEARCH_WORDS: &[&str] = &[
        "latest",
        "newest",
        "version",
        "release",
        "news",
        "compare",
        "comparison",
        "framework",
        "docs",
        "documentation",
        "how do i",
        "how to",
        "what is",
        "who is",
        "price",
        "weather",
        "search the web",
        "look up",
        "on the internet",
        "online",
    ];
    if RESEARCH_WORDS.iter().any(|w| p.contains(w)) {
        return TaskClass::Research;
    }
    TaskClass::Simple
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_budget_matches_the_historical_default() {
        let b = Budget::simple();
        assert_eq!(b.max_rounds, crate::llm::MAX_TOOL_ITERATIONS);
        assert_eq!(budget_for(TaskClass::Simple), b);
    }

    #[test]
    fn caps_widen_by_class() {
        let s = budget_for(TaskClass::Simple);
        let r = budget_for(TaskClass::Research);
        let f = budget_for(TaskClass::Report);
        assert_eq!(s.max_rounds, r.max_rounds, "research keeps the measured default");
        assert!(f.max_rounds > r.max_rounds, "reports get headroom to deliver");
        assert!(s.max_tool_calls < r.max_tool_calls && r.max_tool_calls < f.max_tool_calls);
        assert!(s.max_web_calls < r.max_web_calls && r.max_web_calls < f.max_web_calls);
    }

    #[test]
    fn classify_sorts_the_benchmark_task_shapes() {
        /* the four cap-exhaustion data points are report-shaped */
        assert_eq!(
            classify("Investigate the IGRIS architecture and write the findings to a report"),
            TaskClass::Report
        );
        assert_eq!(
            classify("Compare three AI frameworks and save a comparison file"),
            TaskClass::Report
        );
        assert_eq!(
            classify("Run a workstation audit and save report.md on the Desktop"),
            TaskClass::Report
        );
        assert_eq!(
            classify("find latest version of llama.cpp"),
            TaskClass::Research
        );
        assert_eq!(classify("what time is it"), TaskClass::Simple);
        assert_eq!(classify("set the volume to 30"), TaskClass::Simple);
        assert_eq!(classify("open notepad"), TaskClass::Simple);
    }

    #[test]
    fn classify_is_case_insensitive_and_stable() {
        assert_eq!(
            classify("WRITE THE FILE NOW"),
            classify("write the file now")
        );
    }

    #[test]
    fn web_tool_set_is_exactly_the_page_tools() {
        assert!(is_web_tool("web_fetch"));
        assert!(is_web_tool("web_search"));
        assert!(!is_web_tool("github_repo_info"));
        assert!(!is_web_tool("recycle_bin"));
        assert!(!is_web_tool("read_file"));
    }

    #[test]
    fn unsupported_prefix_is_the_marker() {
        assert_eq!(UNSUPPORTED_PREFIX, "unsupported: ");
    }
}
