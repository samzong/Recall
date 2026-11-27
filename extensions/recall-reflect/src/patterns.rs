use crate::model::{ObservedPattern, TimelineMoment};

pub(crate) fn detect_observed_patterns(moments: &[TimelineMoment]) -> Vec<ObservedPattern> {
    let scope_signals = ["scope", "don't expand", "do not expand", "keep it small"];

    let matched: Vec<&str> = moments
        .iter()
        .filter(|m| {
            let lower = m.summary.to_lowercase();
            scope_signals.iter().any(|sig| lower.contains(&sig.to_lowercase()))
        })
        .map(|m| m.id.as_str())
        .collect();

    if matched.len() >= 2 {
        vec![ObservedPattern {
            id: "pattern-scope-boundary".to_string(),
            summary: "Scope boundary reminders appeared in multiple timeline moments.".to_string(),
            timeline_moments: matched.into_iter().map(String::from).collect(),
            discussion_prompt:
                "Is this a real workflow issue worth calibrating, or are these unrelated scope reminders?"
                    .to_string(),
        }]
    } else {
        Vec::new()
    }
}
