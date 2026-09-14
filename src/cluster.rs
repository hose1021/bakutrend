//! Grouping items into stories. Lexical, deterministic, and deliberately free of models.

use std::collections::{BTreeMap, HashMap};

use crate::store::ItemRow;
use crate::text::{Entity, entities, tokens};

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

/// Keys an item is indexed under: its tokens, plus its entities behind a `#`. The entities
/// have to be indexed too, because a lexical match now requires a shared name and two items
/// can share a name without sharing any of the words the tokenizer kept. The prefix keeps the
/// two namespaces apart — it is unreachable from `tokens`, which only yields alphanumeric runs
/// — so an entity can never masquerade as a token and make a group look like a candidate for
/// the wrong reason.
fn index_keys(tokens: &[String], entities: &[Entity]) -> Vec<String> {
    let mut keys: Vec<String> = tokens.to_vec();
    keys.extend(entities.iter().map(|entity| format!("#{}", entity.text)));
    keys
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
    /// When each outlet first carried the story, over the group's whole history.
    ///
    /// Kept apart from `items` because a scoring window trims `items` to its own range, and a
    /// trimmed item is not an arrival: an outlet that reported before the window and repeated
    /// inside it has been carrying the story all along, and counting its repeat as a fresh
    /// pickup would invent spread that did not happen. Built from every item the group has
    /// ever absorbed, so trimming cannot change it.
    pub outlet_first: BTreeMap<i64, i64>,
    /// Entities of the group's canonical item — the earliest one. Deliberately not the union
    /// of its members: a union grows with every arrival, and an item could then join through a
    /// chain of pairwise agreements that no two members of the group actually share.
    entities: Vec<Entity>,
    /// Running sum of the member vectors, so the centroid costs one addition per arrival
    /// instead of one stored vector per member.
    embedding_sum: Vec<f32>,
}

impl Group {
    fn new(
        item: &ItemRow,
        tokens: Vec<String>,
        entities: Vec<Entity>,
        embedding: Option<&Vec<f32>>,
    ) -> Self {
        let mut group = Self {
            key: group_key(item, &tokens),
            title: item.title.clone(),
            tokens,
            item_ids: vec![item.item_id],
            newest: item.published_at,
            oldest: item.published_at,
            items: vec![item.clone()],
            outlet_first: BTreeMap::from([(item.outlet_id, item.published_at)]),
            entities,
            embedding_sum: Vec::new(),
        };
        group.absorb(embedding);
        group
    }

    /// Centre of what the group is about, which is what a new item is compared against — not
    /// one member that happens to be first. `None` when no member had a vector.
    fn centroid(&self) -> Option<Vec<f32>> {
        crate::embed::unit(&self.embedding_sum)
    }

    fn absorb(&mut self, embedding: Option<&Vec<f32>>) {
        let Some(vector) = embedding else { return };
        if self.embedding_sum.is_empty() {
            self.embedding_sum = vec![0.0; vector.len()];
        }
        // A vector of another width comes from another model. Adding it would corrupt the mean
        // rather than refine it, so it is dropped and the centroid stays the mean of the
        // vectors that agree on a space.
        if self.embedding_sum.len() != vector.len() {
            return;
        }
        for (sum, x) in self.embedding_sum.iter_mut().zip(vector) {
            *sum += x;
        }
    }

    fn push(&mut self, item: &ItemRow, embedding: Option<&Vec<f32>>) {
        self.item_ids.push(item.item_id);
        // `group_items` feeds items oldest first, so this only fires for the public
        // `assign`; either way the title is the earliest published headline.
        if item.published_at < self.oldest {
            self.title = item.title.clone();
        }
        self.newest = self.newest.max(item.published_at);
        self.oldest = self.oldest.min(item.published_at);
        self.items.push(item.clone());
        self.outlet_first
            .entry(item.outlet_id)
            .and_modify(|first| *first = (*first).min(item.published_at))
            .or_insert(item.published_at);
        self.absorb(embedding);
    }
}

/// Lexical threshold a caller's non-finite value falls back to.
const DEFAULT_THRESHOLD: f64 = 0.40;

/// Semantic threshold a caller's non-finite value falls back to.
const DEFAULT_SEMANTIC_THRESHOLD: f64 = 0.80;

/// Clamp a threshold into its meaningful range, or take `fallback` for a non-finite value.
///
/// Above 1.0 nothing can ever match — similarity tops out at 1.0 — so identical headlines
/// would stay in separate groups and hand Task 12 duplicate keys. `f64::clamp` alone does not
/// catch NaN (`f64::NAN.clamp(0.0, 1.0)` is NaN, and every comparison against it is false).
fn clamp_threshold(threshold: f64, fallback: f64) -> f64 {
    if threshold.is_finite() {
        threshold.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// True when one token set contains the other: the same headline carried with more, or less,
/// detail. `Bakıda yollar bağlıdır` and `Bakıda yollar bağlıdır - Sürücülərin nəzərinə` are one
/// story with a trailing note.
///
/// This is the strongest lexical evidence short of equality, and it is deliberately checked
/// before the entity gate. When every word of the shorter headline appears in the longer one,
/// the two cannot be describing different events — the difference is detail, not subject. Two
/// headlines that each carry a word the other lacks are the case worth guarding, and that is
/// what [`shares_evidence`] decides.
fn nested(a: &[String], b: &[String]) -> bool {
    a.iter().all(|token| b.contains(token)) || b.iter().all(|token| a.contains(token))
}

/// True when two entity sets name something specific in common.
///
/// One shared place is deliberately not enough. `Bakıda güclü yağış səbəbindən yollar
/// bağlandı` and `Bakıda güclü külək səbəbindən yollar bağlandı` share the city and every
/// other word, and are still two events: the weather is the news. A shared person,
/// organization or team does name an event; so do two shared names of any kind.
fn shares_evidence(a: &[Entity], b: &[Entity]) -> bool {
    // One side names nothing at all, so the gate has nothing to weigh and the token score
    // decides alone. Refusing here would stop two identical headlines from merging whenever
    // neither names a place or a person — the most confident match there is.
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let mut shared = 0;
    let mut specific = false;
    for entity in a {
        let Some(other) = b.iter().find(|candidate| candidate.text == entity.text) else {
            continue;
        };
        shared += 1;
        if !entity.kind.is_location() || !other.kind.is_location() {
            specific = true;
        }
    }
    specific || shared >= 2
}

pub struct Clusterer {
    threshold: f64,
    semantic_threshold: f64,
    embeddings: HashMap<i64, Vec<f32>>,
}

impl Clusterer {
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold: clamp_threshold(threshold, DEFAULT_THRESHOLD),
            semantic_threshold: DEFAULT_SEMANTIC_THRESHOLD,
            embeddings: HashMap::new(),
        }
    }

    /// Add semantic similarity. `embeddings` maps item id to its vector, for whichever items
    /// the caller managed to embed; an item without one keeps the lexical rule alone, which is
    /// the normal state of a database that has never run with a provider.
    ///
    /// Spec §3: this is additive. Nothing here can make the clusterer fail, and a caller that
    /// passes an empty map gets exactly the lexical behaviour.
    pub fn with_semantic(
        mut self,
        semantic_threshold: f64,
        embeddings: HashMap<i64, Vec<f32>>,
    ) -> Self {
        self.semantic_threshold = clamp_threshold(semantic_threshold, DEFAULT_SEMANTIC_THRESHOLD);
        self.embeddings = embeddings;
        self
    }

    /// Whether `item` is the same story as `group`.
    ///
    /// Three routes, in order of strength. Semantic similarity can join two headlines that
    /// share no word at all, and is consulted first because it is the strongest evidence — but
    /// it only exists when a provider produced vectors for both items. A nested token set is
    /// the same headline with more or less detail. Otherwise the words must overlap enough AND
    /// something must name the event, which is what keeps two sentences about different events
    /// from merging on shared vocabulary alone.
    fn links(
        &self,
        item: &ItemRow,
        item_tokens: &[String],
        item_entities: &[Entity],
        group: &Group,
    ) -> bool {
        if let (Some(vector), Some(centroid)) =
            (self.embeddings.get(&item.item_id), group.centroid())
            && crate::embed::cosine(vector, &centroid) >= self.semantic_threshold
        {
            return true;
        }
        similarity(item_tokens, &group.tokens) >= self.threshold
            && (nested(item_tokens, &group.tokens)
                || shares_evidence(item_entities, &group.entities))
    }

    /// Every group worth comparing against one item.
    ///
    /// With a vector for the item, the index cannot bound the set: a semantic match needs no
    /// shared word, so any group could match and every one is compared.
    /// ponytail: O(items x groups) when embeddings are on, which is a local cosine and only
    /// when a provider is configured. Bound it by entities or an ANN index if that ever hurts.
    fn candidates(
        &self,
        item_id: i64,
        keys: &[String],
        index: &HashMap<String, Vec<usize>>,
        group_count: usize,
    ) -> Vec<usize> {
        if self.embeddings.contains_key(&item_id) {
            return (0..group_count).collect();
        }
        let mut indexed: Vec<usize> = Vec::new();
        for key in keys {
            if let Some(owners) = index.get(key) {
                indexed.extend_from_slice(owners);
            }
        }
        indexed.sort_unstable();
        indexed.dedup();
        indexed
    }

    /// Place `item` in the most similar existing group, or start a new one.
    /// Returns the index of the group it landed in.
    pub fn assign(&self, groups: &mut Vec<Group>, item: &ItemRow) -> usize {
        let item_tokens = tokens(&item.title);
        let item_entities = entities(&item.title);
        let embedding = self.embeddings.get(&item.item_id);
        let mut best: Option<(usize, f64)> = None;
        for (index, group) in groups.iter().enumerate() {
            if !self.links(item, &item_tokens, &item_entities, group) {
                continue;
            }
            let score = similarity(&item_tokens, &group.tokens);
            if best.is_none_or(|(_, top)| score > top) {
                best = Some((index, score));
            }
        }
        match best {
            Some((index, _)) => {
                groups[index].push(item, embedding);
                index
            }
            None => {
                groups.push(Group::new(item, item_tokens, item_entities, embedding));
                groups.len() - 1
            }
        }
    }

    /// Group a whole item set. An inverted index over tokens and entities keeps this
    /// linear-ish rather than quadratic: a new item is only compared against groups that share
    /// a token or a name, which is a complete candidate set for every rule that can match
    /// without a vector. An item that has one is compared against every group instead, because
    /// a semantic match needs no shared key at all.
    pub fn group_items(&self, items: &[ItemRow]) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();

        // Oldest first, so a group's title is the earliest headline of the event.
        let mut ordered: Vec<&ItemRow> = items.iter().collect();
        ordered.sort_by_key(|item| (item.published_at, item.item_id));

        for item in ordered {
            let item_tokens = tokens(&item.title);
            let item_entities = entities(&item.title);
            let embedding = self.embeddings.get(&item.item_id);
            let keys = index_keys(&item_tokens, &item_entities);
            let candidates = self.candidates(item.item_id, &keys, &index, groups.len());

            let mut best: Option<(usize, f64)> = None;
            for group_index in candidates {
                if !self.links(item, &item_tokens, &item_entities, &groups[group_index]) {
                    continue;
                }
                let score = similarity(&item_tokens, &groups[group_index].tokens);
                if best.is_none_or(|(_, top)| score > top) {
                    best = Some((group_index, score));
                }
            }

            let group_index = match best {
                Some((group_index, _)) => {
                    groups[group_index].push(item, embedding);
                    group_index
                }
                None => {
                    groups.push(Group::new(
                        item,
                        item_tokens.clone(),
                        item_entities,
                        embedding,
                    ));
                    groups.len() - 1
                }
            };
            for key in keys {
                index.entry(key).or_default().push(group_index);
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
            cited_outlet: None,
            is_backfill: false,
        }
    }

    #[test]
    fn similarity_is_jaccard_over_token_sets() {
        let a = vec!["bakida".to_string(), "yollar".to_string()];
        let b = vec![
            "bakida".to_string(),
            "yollar".to_string(),
            "baglidir".to_string(),
        ];
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
            &item(
                2,
                2,
                "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə",
                200,
            ),
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
        clusterer.assign(
            &mut groups,
            &item(2, 2, "İstinadən: Bakıda bu yollar bağlıdır", 200),
        );
        assert_eq!(groups.len(), 1);
    }

    /// Public `assign` is called with items in caller order, so the group title must be
    /// the earliest published headline, not whichever arrived first.
    #[test]
    fn assign_titles_the_group_with_the_earliest_published_item() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(&mut groups, &item(1, 1, "Bakıda bu yollar bağlıdır", 200));
        clusterer.assign(
            &mut groups,
            &item(
                2,
                2,
                "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə",
                100,
            ),
        );
        assert_eq!(
            groups[0].title, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə",
            "the earliest published headline names the story"
        );
    }

    #[test]
    fn different_events_sharing_one_word_stay_apart() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(
            &mut groups,
            &item(1, 1, "Gəncədə iki nəfər bıçaqlandı", 100),
        );
        clusterer.assign(
            &mut groups,
            &item(2, 2, "Gəncədə toy karvanı qəza etdi", 200),
        );
        assert_eq!(
            groups.len(),
            2,
            "a shared place name must not merge two events"
        );
    }

    /// The gazetteer aligns one place written two ways, which is what the entity gate needs. It
    /// does not by itself join two languages: two headlines that share only a city stay apart,
    /// in one language or two, because a shared place carries no event. This program has no
    /// translation or semantic step, so a cross-language duplicate of one event is a known miss.
    #[test]
    fn the_gazetteer_aligns_names_across_languages_without_joining_their_words() {
        assert_eq!(
            entities("Bakıda yanğın olub")
                .first()
                .map(|e| e.text.clone()),
            Some("baki".to_string())
        );
        assert_eq!(
            entities("В Баку произошёл пожар")
                .first()
                .map(|e| e.text.clone()),
            Some("baki".to_string()),
            "one city, two spellings, one entity"
        );

        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(
            &mut groups,
            &item(1, 1, "Bakıda güclü yağış yolları bağladı", 100),
        );
        clusterer.assign(
            &mut groups,
            &item(2, 2, "В Баку сильный дождь закрыл дороги", 200),
        );
        assert_eq!(
            groups.len(),
            2,
            "a shared name and no shared word is not one story"
        );
    }

    /// The limit of a lexical rule, written down as a test so a change to it is deliberate: two
    /// headlines about one event that share no word and name no place the gazetteer knows stay
    /// apart. Only a semantic or translation step could join them, and this program has none.
    #[test]
    fn an_event_phrased_with_no_shared_name_stays_apart() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(
            &mut groups,
            &item(1, 1, "Yük maşını aşıb yükü yola səpildi", 100),
        );
        clusterer.assign(
            &mut groups,
            &item(2, 2, "Truck overturned and spilled its cargo", 200),
        );
        assert_eq!(
            groups.len(),
            2,
            "no shared token and no known name leaves nothing to merge on"
        );
    }

    #[test]
    fn group_items_preserves_input_order_and_picks_the_earliest_title() {
        let clusterer = Clusterer::new(0.45);
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(
                2,
                2,
                "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə",
                200,
            ),
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
        // Threshold 0 drops the requirement that the words overlap. It does not drop the
        // requirement that something names the event: two headlines with no shared word and no
        // name in common stay apart, and both entry points must agree on that.
        let clusterer = Clusterer::new(0.0);
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Sumqayıtda toy karvanı qəza etdi", 200),
        ];

        let mut assigned = Vec::new();
        for row in &items {
            clusterer.assign(&mut assigned, row);
        }
        let grouped = clusterer.group_items(&items);

        assert_eq!(
            assigned.len(),
            2,
            "similarity alone is not evidence at any threshold"
        );
        assert_eq!(
            grouped.len(),
            assigned.len(),
            "group_items must agree with assign at threshold 0"
        );

        // A shared organization is evidence, so at threshold 0 these merge despite sharing no
        // word beyond the name itself.
        let named = vec![
            item(3, 3, "SOCAR Bakıda yeni layihə", 300),
            item(4, 4, "SOCAR Gəncədə görüş keçirdi", 400),
        ];
        assert_eq!(Clusterer::new(0.0).group_items(&named).len(), 1);
    }

    #[test]
    fn signature_is_stable_and_collision_free() {
        let tokens = vec!["a".to_string(), "b".to_string()];
        assert_eq!(signature(&tokens), "a-b");
        assert_eq!(signature(&[]), "");

        let long = |last: &str| -> Vec<String> {
            ["a", "b", "c", "d", "e", "f", last]
                .iter()
                .map(|s| s.to_string())
                .collect()
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

        assert_eq!(
            groups.len(),
            2,
            "untokenized headlines can never match anything"
        );
        assert_eq!(groups[0].key, "untokenized:1");
        assert_eq!(groups[1].key, "untokenized:2");
        assert_ne!(
            groups[0].key, groups[1].key,
            "duplicate keys overwrite each other in Task 12"
        );

        let again = clusterer.group_items(&items);
        assert_eq!(
            again[0].key, groups[0].key,
            "the key must not move between polls"
        );
    }

    #[test]
    fn an_impossible_threshold_is_clamped_into_range() {
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Bakıda bu yollar bağlıdır", 200),
        ];

        let groups = Clusterer::new(5.0).group_items(&items);

        assert_eq!(
            groups.len(),
            1,
            "5.0 clamps to 1.0, so identical headlines still merge"
        );
    }

    #[test]
    fn a_nan_threshold_falls_back_to_the_default() {
        let identical = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(
                2,
                2,
                "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə",
                200,
            ),
        ];
        let unrelated = vec![
            item(1, 1, "Gəncədə iki nəfər bıçaqlandı", 100),
            item(2, 2, "Gəncədə toy karvanı qəza etdi", 200),
        ];

        // 0.6 joins and 0.167 stays apart only at the default 0.40: a NaN threshold would put
        // every item in its own group, and 0.0 would merge both pairs.
        assert_eq!(Clusterer::new(f64::NAN).group_items(&identical).len(), 1);
        assert_eq!(Clusterer::new(f64::NAN).group_items(&unrelated).len(), 2);
    }

    #[test]
    fn a_reworded_headline_joins_on_semantics_when_vectors_exist() {
        // These two share almost nothing: two tokens out of twelve. The lexical rule keeps them
        // apart at any sane threshold, and vectors that agree join them — which is the whole
        // point of letting a provider in.
        let items = vec![
            item(1, 1, "Sabah Barcelona oyun biletləri satışa çıxacaq", 100),
            item(
                2,
                2,
                "Barcelona ilə Bakı klubu arasında qarşılaşmaya bilet satışı başlayır",
                200,
            ),
        ];

        let lexical = Clusterer::new(0.40).group_items(&items);
        assert_eq!(
            lexical.len(),
            2,
            "words alone cannot see that these describe one fixture"
        );

        let semantic = Clusterer::new(0.40).with_semantic(
            0.80,
            HashMap::from([(1, vec![1.0, 0.0]), (2, vec![0.99, 0.1])]),
        );
        assert_eq!(
            semantic.group_items(&items).len(),
            1,
            "vectors that agree join headlines that share no word"
        );
    }

    #[test]
    fn no_vectors_is_exactly_the_lexical_clusterer() {
        // The normal state of a database that has never run with a provider: an empty vector map
        // must be indistinguishable from never asking for semantic similarity at all.
        let items = vec![
            item(1, 1, "Bakıda güclü yağış səbəbindən yollar bağlandı", 100),
            item(2, 2, "Bakıda güclü külək səbəbindən yollar bağlandı", 200),
            item(3, 3, "Bakıda bu yollar bağlıdır", 300),
        ];
        let plain = Clusterer::new(0.40).group_items(&items);
        let empty = Clusterer::new(0.40)
            .with_semantic(0.80, HashMap::new())
            .group_items(&items);
        assert_eq!(
            plain.len(),
            3,
            "the two weather headlines share a city and every other word, and are still two \
             events: neither token set contains the other and only a place is shared"
        );
        assert_eq!(
            plain.iter().map(|g| g.key.clone()).collect::<Vec<_>>(),
            empty.iter().map(|g| g.key.clone()).collect::<Vec<_>>(),
            "an empty cache must change nothing"
        );
    }

    #[test]
    fn an_item_joins_what_it_matches_not_what_its_neighbour_matched() {
        // A matches B, B matches C, and A does not match C. Comparing against the group's
        // canonical item — its earliest, whose tokens and entities the group keeps and never
        // widens — is what stops the three collapsing into one story by transitivity.
        let items = vec![
            item(1, 1, "Bakıda metro təmir işləri", 100),
            item(2, 2, "Metro təmir işləri davam edir", 200),
            item(3, 3, "Təmir işləri davam edir, planlaşdırılır", 300),
        ];
        let a = tokens(&items[0].title);
        let b = tokens(&items[1].title);
        let c = tokens(&items[2].title);
        assert!(similarity(&a, &b) >= 0.40, "A and B are one story");
        assert!(similarity(&b, &c) >= 0.40, "B and C are one story");
        assert!(
            similarity(&a, &c) < 0.40,
            "A and C are not: {}",
            similarity(&a, &c)
        );

        let groups = Clusterer::new(0.40).group_items(&items);

        assert_eq!(groups.len(), 2, "C must not ride in on B's coat-tails");
        assert_eq!(groups[0].item_ids, vec![1, 2]);
        assert_eq!(groups[1].item_ids, vec![3]);
    }

    #[test]
    fn the_centroid_is_the_mean_of_the_members() {
        // Three near-identical vectors: the centre stays where they agree, which is what a new
        // item is compared against. The centroid is renormalised, so the value is the unit
        // vector of the mean, not the raw sum.
        let items = vec![
            item(1, 1, "Bakıda metro təmir işləri", 100),
            item(2, 2, "Bakıda metro təmir işləri davam edir", 200),
        ];
        let vectors = HashMap::from([(1, vec![1.0, 0.0]), (2, vec![0.0, 1.0])]);
        let groups = Clusterer::new(0.40)
            .with_semantic(0.80, vectors)
            .group_items(&items);
        assert_eq!(groups.len(), 1);
        let sum = vec![1.0f32, 1.0];
        let expected = crate::embed::unit(&sum).unwrap();
        let actual = groups[0].centroid().unwrap();
        for (a, b) in actual.iter().zip(&expected) {
            assert!((a - b).abs() < 1e-6, "centroid {actual:?} != {expected:?}");
        }
    }
}
