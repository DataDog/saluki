//! Create order and start order, as levels of participants that do not depend on each other.

use std::collections::BTreeSet;

/// Groups `participants` into levels, each after every participant an edge puts before it.
///
/// Each edge is a pair: the participant that comes first, and the one after it.
///
/// # Errors
///
/// Returns the chain of a cycle, such as `a -> b -> a`.
pub(super) fn levels<I>(participants: I, edges: &BTreeSet<(String, String)>) -> Result<Vec<Vec<String>>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut remaining: BTreeSet<String> = participants.into_iter().collect();
    let waits = |participant: &String, remaining: &BTreeSet<String>| {
        edges
            .iter()
            .find(|(before, after)| after == participant && remaining.contains(before))
            .map(|(before, _)| before.clone())
    };
    let mut levels = Vec::new();
    while let Some(first) = remaining.first().cloned() {
        let level: Vec<String> = remaining
            .iter()
            .filter(|participant| waits(participant, &remaining).is_none())
            .cloned()
            .collect();
        if level.is_empty() {
            // Every remaining participant waits for another remaining one, so walking back from any of them repeats.
            let mut chain = vec![first];
            while let Some(before) = chain.last().and_then(|last| waits(last, &remaining)) {
                let seen = chain.iter().position(|participant| *participant == before);
                chain.push(before);
                if let Some(start) = seen {
                    let cycle: Vec<&str> = chain[start..].iter().rev().map(String::as_str).collect();
                    return Err(cycle.join(" -> "));
                }
            }
            return Err(chain.join(" -> "));
        }
        for participant in &level {
            remaining.remove(participant);
        }
        levels.push(level);
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(participants: &[&str], edges: &[(&str, &str)]) -> Vec<Vec<String>> {
        let edges = edges
            .iter()
            .map(|(before, after)| (before.to_string(), after.to_string()))
            .collect();
        levels(participants.iter().map(ToString::to_string), &edges).unwrap()
    }

    #[test]
    fn independent_participants_share_one_level() {
        assert_eq!(plan(&["a", "b", "c"], &[]), [["a", "b", "c"]]);
    }

    #[test]
    fn a_chain_gives_each_participant_a_level_in_order() {
        assert_eq!(plan(&["a", "b", "c"], &[("c", "b"), ("b", "a")]), [["c"], ["b"], ["a"]]);
    }

    #[test]
    fn a_diamond_puts_its_two_middles_in_one_level() {
        let edges = [
            ("top", "left"),
            ("top", "right"),
            ("left", "bottom"),
            ("right", "bottom"),
        ];
        assert_eq!(
            plan(&["bottom", "left", "right", "top"], &edges),
            [vec!["top"], vec!["left", "right"], vec!["bottom"]]
        );
    }
}
