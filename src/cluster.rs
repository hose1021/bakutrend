//! Grouping items into stories. Lexical, deterministic, and deliberately free of models.

use std::collections::HashMap;

use crate::store::ItemRow;
use crate::text::tokens;

/// Jaccard similarity over token sets. Both inputs must be deduplicated; order does
/// not matter, so a caller cannot silently mis-count by passing unsorted tokens.
pub fn similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|token| b.contains(token)).count();
    let union = a.len() + b.len() - shared;
    shared as f64 / union as f64
}

/// Stable identifier for a story: its whole sorted, deduplicated token set, joined by `-`.
///
/// `tokens` only ever yields alphanumeric runs, so no token contains `-` and the join is
/// injective: distinct token sets always produce distinct keys. Distinct groups have distinct
/// token sets anyway — identical sets score 1.0 and would have merged — but the injectivity is
/// what makes the guarantee hold without leaning on the grouping rule. Task 12 keys rank deltas
/// on this string, where a collision means both stories report the wrong movement.
///
/// An empty set returns `""`. Callers that need one identity per untokenized item must not use
/// this; [`Group::new`] keys those by row id instead.
pub fn signature(tokens: &[String]) -> String {
    tokens.join("-")
}

/// Key for a group that a new item starts: its signature, or a row-id key when the title yields
/// no tokens. An untokenized item can never match anything at a positive threshold, so each one
/// becomes its own group; keying them all `""` would collide and Task 12 would overwrite one
/// story's rank delta with another's. `item_id` is the database row id, stable across polls
/// because items are upserted rather than re-inserted. The `:` prefix is unreachable from
/// `signature`, which only ever joins alphanumeric tokens.
fn group_key(item: &ItemRow, tokens: &[String]) -> String {
    if tokens.is_empty() {
        format!("untokenized:{}", item.item_id)
    } else {
        signature(tokens)
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub key: String,
    pub title: String,
    pub tokens: Vec<String>,
    pub item_ids: Vec<i64>,
    pub newest: i64,
    pub oldest: i64,
    pub items: Vec<ItemRow>,
}

impl Group {
    fn new(item: &ItemRow, tokens: Vec<String>) -> Self {
        Self {
            key: group_key(item, &tokens),
            title: item.title.clone(),
            tokens,
            item_ids: vec![item.item_id],
            newest: item.published_at,
            oldest: item.published_at,
            items: vec![item.clone()],
        }
    }

    fn push(&mut self, item: &ItemRow) {
        self.item_ids.push(item.item_id);
        self.newest = self.newest.max(item.published_at);
        self.oldest = self.oldest.min(item.published_at);
        self.items.push(item.clone());
    }
}

/// Similarity threshold a caller's non-finite value falls back to.
const DEFAULT_THRESHOLD: f64 = 0.45;

pub struct Clusterer {
    threshold: f64,
}

impl Clusterer {
    /// Clamp the threshold into its meaningful range. Above 1.0 nothing can ever match —
    /// similarity tops out at 1.0 — so identical headlines would stay in separate groups and
    /// hand Task 12 duplicate keys. `f64::clamp` alone does not catch NaN
    /// (`f64::NAN.clamp(0.0, 1.0)` is NaN, and every comparison against it is false), so
    /// non-finite values take `DEFAULT_THRESHOLD`.
    pub fn new(threshold: f64) -> Self {
        let threshold = if threshold.is_finite() {
            threshold.clamp(0.0, 1.0)
        } else {
            DEFAULT_THRESHOLD
        };
        Self { threshold }
    }

    /// Place `item` in the most similar existing group, or start a new one.
    /// Returns the index of the group it landed in.
    pub fn assign(&self, groups: &mut Vec<Group>, item: &ItemRow) -> usize {
        let item_tokens = tokens(&item.title);
        let mut best: Option<(usize, f64)> = None;
        for (index, group) in groups.iter().enumerate() {
            let score = similarity(&item_tokens, &group.tokens);
            if score >= self.threshold && best.is_none_or(|(_, top)| score > top) {
                best = Some((index, score));
            }
        }
        match best {
            Some((index, _)) => {
                groups[index].push(item);
                index
            }
            None => {
                groups.push(Group::new(item, item_tokens));
                groups.len() - 1
            }
        }
    }

    /// Group a whole item set. An inverted token index keeps this linear-ish rather than
    /// quadratic: a new item is only compared against groups sharing at least one token.
    pub fn group_items(&self, items: &[ItemRow]) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();

        // Oldest first, so a group's title is the earliest headline of the event.
        let mut ordered: Vec<&ItemRow> = items.iter().collect();
        ordered.sort_by_key(|item| (item.published_at, item.item_id));

        for item in ordered {
            let item_tokens = tokens(&item.title);
            // The index only covers token-sharing groups, which is a complete candidate set
            // only while a match requires a shared token. At a non-positive threshold a group
            // with nothing in common can still match, so fall back to every group and keep the
            // two entry points in agreement.
            let candidates: Vec<usize> = if self.threshold <= 0.0 {
                (0..groups.len()).collect()
            } else {
                let mut indexed: Vec<usize> = Vec::new();
                for token in &item_tokens {
                    if let Some(owners) = index.get(token) {
                        indexed.extend_from_slice(owners);
                    }
                }
                indexed.sort_unstable();
                indexed.dedup();
                indexed
            };

            let mut best: Option<(usize, f64)> = None;
            for group_index in candidates {
                let score = similarity(&item_tokens, &groups[group_index].tokens);
                if score >= self.threshold && best.is_none_or(|(_, top)| score > top) {
                    best = Some((group_index, score));
                }
            }

            let group_index = match best {
                Some((group_index, _)) => {
                    groups[group_index].push(item);
                    group_index
                }
                None => {
                    groups.push(Group::new(item, item_tokens.clone()));
                    groups.len() - 1
                }
            };
            for token in &item_tokens {
                index.entry(token.clone()).or_default().push(group_index);
            }
        }
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;
    use crate::store::ItemRow;

    fn item(id: i64, outlet_id: i64, title: &str, published_at: i64) -> ItemRow {
        ItemRow {
            item_id: id,
            source_id: outlet_id * 10,
            outlet_id,
            outlet: format!("Outlet{outlet_id}"),
            kind: SourceKind::Rss,
            title: title.to_string(),
            description: None,
            url: format!("https://example.az/{id}"),
            published_at,
            views: None,
            cited: false,
            is_backfill: false,
        }
    }

    #[test]
    fn similarity_is_jaccard_over_token_sets() {
        let a = vec!["bakida".to_string(), "yollar".to_string()];
        let b = vec!["bakida".to_string(), "yollar".to_string(), "baglidir".to_string()];
        assert!((similarity(&a, &b) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(similarity(&a, &[]), 0.0, "an empty set never matches");
    }

    #[test]
    fn a_reworded_headline_joins_the_same_story() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        let first = clusterer.assign(&mut groups, &item(1, 1, "Bakıda bu yollar bağlıdır", 100));
        let second = clusterer.assign(
            &mut groups,
            &item(2, 2, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə", 200),
        );
        assert_eq!(first, second);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].item_ids.len(), 2);
        assert_eq!(groups[0].newest, 200);
    }

    #[test]
    fn a_syndicated_restatement_joins_the_original() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(&mut groups, &item(1, 1, "Bakıda bu yollar bağlıdır", 100));
        clusterer.assign(&mut groups, &item(2, 2, "İstinadən: Bakıda bu yollar bağlıdır", 200));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn different_events_sharing_one_word_stay_apart() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(&mut groups, &item(1, 1, "Gəncədə iki nəfər bıçaqlandı", 100));
        clusterer.assign(&mut groups, &item(2, 2, "Gəncədə toy karvanı qəza etdi", 200));
        assert_eq!(groups.len(), 2, "a shared place name must not merge two events");
    }

    #[test]
    fn group_items_preserves_input_order_and_picks_the_earliest_title() {
        let clusterer = Clusterer::new(0.45);
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə", 200),
            item(3, 3, "Tamamilə fərqli bir xəbər budur", 150),
        ];
        let groups = clusterer.group_items(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "Bakıda bu yollar bağlıdır");
        assert_eq!(groups[0].item_ids, vec![1, 2]);
        assert_eq!(groups[0].oldest, 100);
    }

    #[test]
    fn zero_threshold_groups_the_same_through_both_entry_points() {
        let clusterer = Clusterer::new(0.0);
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Gəncədə toy karvanı qəza etdi", 200),
        ];

        let mut assigned = Vec::new();
        for row in &items {
            clusterer.assign(&mut assigned, row);
        }
        let grouped = clusterer.group_items(&items);

        assert_eq!(assigned.len(), 1, "at threshold 0 even a disjoint headline matches");
        assert_eq!(grouped.len(), 1, "group_items must agree with assign at threshold 0");
    }

    #[test]
    fn signature_is_stable_and_collision_free() {
        let tokens = vec!["a".to_string(), "b".to_string()];
        assert_eq!(signature(&tokens), "a-b");
        assert_eq!(signature(&[]), "");

        let long = |last: &str| -> Vec<String> {
            ["a", "b", "c", "d", "e", "f", last].iter().map(|s| s.to_string()).collect()
        };
        assert_ne!(
            signature(&long("g")),
            signature(&long("h")),
            "keys that differ only after the sixth token must not collide"
        );
        assert_ne!(
            signature(&[]),
            signature(&["empty".to_string()]),
            "an untokenized set must not collide with a title that contains the token `empty`"
        );
    }

    #[test]
    fn untokenized_titles_get_distinct_keys() {
        let clusterer = Clusterer::new(0.45);
        // Nothing here survives `tokens`: every word is under four characters or a stopword.
        let items = vec![item(1, 1, "və bu il o", 100), item(2, 2, "olan ucun", 200)];

        let groups = clusterer.group_items(&items);

        assert_eq!(groups.len(), 2, "untokenized headlines can never match anything");
        assert_eq!(groups[0].key, "untokenized:1");
        assert_eq!(groups[1].key, "untokenized:2");
        assert_ne!(groups[0].key, groups[1].key, "duplicate keys overwrite each other in Task 12");

        let again = clusterer.group_items(&items);
        assert_eq!(again[0].key, groups[0].key, "the key must not move between polls");
    }

    #[test]
    fn an_impossible_threshold_is_clamped_into_range() {
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Bakıda bu yollar bağlıdır", 200),
        ];

        let groups = Clusterer::new(5.0).group_items(&items);

        assert_eq!(groups.len(), 1, "5.0 clamps to 1.0, so identical headlines still merge");
    }

    #[test]
    fn a_nan_threshold_falls_back_to_the_default() {
        let identical = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə", 200),
        ];
        let unrelated = vec![
            item(1, 1, "Gəncədə iki nəfər bıçaqlandı", 100),
            item(2, 2, "Gəncədə toy karvanı qəza etdi", 200),
        ];

        // 0.6 joins and 0.167 stays apart only at the default 0.45: a NaN threshold would put
        // every item in its own group, and 0.0 would merge both pairs.
        assert_eq!(Clusterer::new(f64::NAN).group_items(&identical).len(), 1);
        assert_eq!(Clusterer::new(f64::NAN).group_items(&unrelated).len(), 2);
    }
}
