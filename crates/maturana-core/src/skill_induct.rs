//! Auto-skill induction — Maturana's zero-trust answer to Hermes' "the agent
//! writes its own skills from your repeated workflows".
//!
//! It surfaces RECURRING task patterns from the trajectory store and PROPOSES a
//! skill draft for each. It never installs anything. An agent observing its own
//! repetition must not be able to grant itself new automation, so a proposal is
//! a draft a human routes through `maturana-security-review` before it can become
//! a real skill. Induction is host-side analysis over data the host already has.

use crate::improvement::Trajectory;

/// A proposed skill, induced from repeated task inputs. A draft, not a skill.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillProposal {
    /// Slug for the proposal directory, e.g. `induced-list-web-frameworks`.
    pub slug: String,
    /// Human title, e.g. "List Web Frameworks".
    pub title: String,
    /// The normalized signature the cluster shares.
    pub signature: String,
    /// How many trajectories matched.
    pub occurrences: usize,
    /// Which agents produced them (sorted, deduped).
    pub agents: Vec<String>,
    /// A few representative raw inputs.
    pub examples: Vec<String>,
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "you", "your", "this", "that", "from", "into",
    "please", "can", "will", "would", "could", "should", "are", "was", "were",
    "his", "her", "its", "our", "their", "what", "when", "where", "which", "who",
    "how", "why", "all", "any", "each", "out", "use", "using", "via", "one",
    "two", "three", "exactly", "popular", "list", "give", "make", "write",
];

/// Normalize a task input to a clustering signature: lowercase, keep alphabetic
/// words of length >= 3 that aren't stopwords, take the first `words` of them.
/// Similar tasks collapse to the same signature; numbers/punctuation are dropped.
pub fn signature(input: &str, words: usize) -> String {
    input
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .take(words)
        .collect::<Vec<_>>()
        .join("-")
}

fn slugify(signature: &str) -> String {
    let core: String = signature
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("induced-{}", core.trim_matches('-'))
}

fn titlecase(signature: &str) -> String {
    signature
        .split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Group trajectories by signature and return a proposal for every signature
/// that recurs at least `min_occurrences` times, most frequent first. A signature
/// with too few significant words (< 2) is ignored as too generic to be a skill.
pub fn induct(trajectories: &[Trajectory], min_occurrences: usize, sig_words: usize) -> Vec<SkillProposal> {
    use std::collections::BTreeMap;
    struct Cluster {
        count: usize,
        agents: Vec<String>,
        examples: Vec<String>,
    }
    let mut clusters: BTreeMap<String, Cluster> = BTreeMap::new();
    for traj in trajectories {
        let sig = signature(&traj.input, sig_words);
        if sig.split('-').filter(|w| !w.is_empty()).count() < 2 {
            continue; // too generic to be a useful skill
        }
        let entry = clusters.entry(sig).or_insert_with(|| Cluster {
            count: 0,
            agents: Vec::new(),
            examples: Vec::new(),
        });
        entry.count += 1;
        if !entry.agents.contains(&traj.agent_id) {
            entry.agents.push(traj.agent_id.clone());
        }
        if entry.examples.len() < 3 {
            let ex = traj.input.trim();
            let ex = ex.chars().take(160).collect::<String>();
            if !entry.examples.contains(&ex) {
                entry.examples.push(ex);
            }
        }
    }
    let mut proposals: Vec<SkillProposal> = clusters
        .into_iter()
        .filter(|(_, c)| c.count >= min_occurrences)
        .map(|(sig, c)| {
            let mut agents = c.agents;
            agents.sort();
            SkillProposal {
                slug: slugify(&sig),
                title: titlecase(&sig),
                signature: sig,
                occurrences: c.count,
                agents,
                examples: c.examples,
            }
        })
        .collect();
    // Most frequent first; ties broken by signature for stable output.
    proposals.sort_by(|a, b| b.occurrences.cmp(&a.occurrences).then(a.signature.cmp(&b.signature)));
    proposals
}

/// Render a proposed SKILL.md DRAFT. It is clearly marked as a proposal and is
/// NEVER a usable skill until a human reviews it via `maturana-security-review`
/// and installs it deliberately.
pub fn render_proposal(p: &SkillProposal) -> String {
    let mut out = String::new();
    out.push_str(&format!("# PROPOSED skill: {}\n\n", p.title));
    out.push_str(
        "> ⚠️ PROPOSAL — induced from repeated tasks, NOT installed and NOT active.\n\
         > An agent cannot grant itself automation. Review this draft with\n\
         > `maturana-security-review`, edit it, then install it deliberately with\n\
         > `maturana skill codex-prompts` (or move it into `skills/`). Do not run\n\
         > it as-is.\n\n",
    );
    out.push_str(&format!(
        "Induced because this task pattern recurred **{} times** across agents: {}.\n\n",
        p.occurrences,
        if p.agents.is_empty() { "(unknown)".to_string() } else { p.agents.join(", ") }
    ));
    out.push_str("## Observed examples\n\n");
    for ex in &p.examples {
        out.push_str(&format!("- {ex}\n"));
    }
    out.push_str("\n## Draft procedure (fill in)\n\n");
    out.push_str(
        "Describe the repeatable steps the agent took for these tasks, the tools it\n\
         used, and the inputs/outputs. Then shape it into a real skill (when-to-use,\n\
         grounding, actions, evidence, recovery, boundaries) before installing.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn traj(agent: &str, input: &str) -> Trajectory {
        Trajectory {
            id: format!("t-{input}-{agent}"),
            agent_id: agent.to_string(),
            session_id: "s".to_string(),
            kind: "turn".to_string(),
            input: input.to_string(),
            output: String::new(),
            tool_calls: String::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn signature_drops_stopwords_numbers_and_punctuation() {
        // "list", "exactly", "popular", "three" are stopwords; "3" is a number.
        let sig = signature("List exactly 3 popular Rust web frameworks!", 6);
        assert_eq!(sig, "rust-web-frameworks");
        // Different phrasings of the same task collapse together.
        assert_eq!(
            signature("Please list the popular rust web frameworks", 6),
            "rust-web-frameworks"
        );
    }

    #[test]
    fn induct_clusters_by_signature_and_thresholds() {
        // Real repeated tasks share a prefix; the signature is the first few
        // significant words, so these three collapse to one cluster.
        let trajectories = vec![
            traj("codex", "List 3 popular Rust web frameworks"),
            traj("claude", "please list the popular rust web frameworks"),
            traj("opencode", "rust web frameworks"),
            traj("codex", "summarize today's emails"), // only once → below threshold
        ];
        let proposals = induct(&trajectories, 3, 6);
        assert_eq!(proposals.len(), 1, "only the rust-web-frameworks pattern recurs 3x");
        let p = &proposals[0];
        assert_eq!(p.occurrences, 3);
        assert_eq!(p.slug, "induced-rust-web-frameworks");
        assert_eq!(p.agents, vec!["claude", "codex", "opencode"]);
        assert!(p.title.contains("Rust"));
    }

    #[test]
    fn render_proposal_is_clearly_a_gated_draft() {
        let p = induct(
            &[
                traj("codex", "deploy the rust web service"),
                traj("codex", "please deploy the rust web service"),
                traj("claude", "deploy rust web service"),
            ],
            3,
            6,
        )
        .into_iter()
        .next()
        .unwrap();
        let md = render_proposal(&p);
        assert!(md.contains("PROPOSAL"));
        assert!(md.contains("maturana-security-review"));
        assert!(md.contains("NOT installed"));
        assert!(md.contains("recurred"));
    }

    #[test]
    fn too_generic_signatures_are_ignored() {
        // A single significant word after stopwords → not a skill.
        let trajectories = vec![
            traj("codex", "please help"),
            traj("codex", "please help"),
            traj("codex", "please help"),
        ];
        assert!(induct(&trajectories, 2, 6).is_empty());
    }
}
