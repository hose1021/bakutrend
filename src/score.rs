//! Popularity scoring: cross-outlet coverage, reader engagement, and freshness.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::cluster::Group;
use crate::source::SourceKind;
use crate::store::{Sample, Window};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Weights {
    pub coverage: f64,
    pub engagement: f64,
    pub freshness: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { coverage: 0.40, engagement: 0.40, freshness: 0.20 }
    }
}

#[derive(Debug, Clone)]
pub struct OutletContribution {
    pub outlet: String,
    pub weight: f64,
    pub newest: i64,
    pub views: Option<i64>,
    pub views_per_hour: Option<f64>,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ScoredStory {
    pub key: String,
    pub title: String,
    pub score: f64,
    pub coverage: f64,
    pub coverage_norm: f64,
    pub engagement: f64,
    pub engagement_norm: f64,
    pub freshness: f64,
    pub view_count: i64,
    pub newest: i64,
    pub outlets: Vec<OutletContribution>,
}

#[derive(Debug, Clone, Copy)]
pub struct ItemRate {
    pub item_id: i64,
    pub source_id: i64,
    pub rate: f64,
    pub views: i64,
}

/// Views gained per hour. With two samples at least ten minutes apart this is the observed
/// slope; otherwise it spreads the total over the item's age, floored at a quarter hour.
pub fn views_per_hour(samples: &[Sample], item_id: i64, published_at: i64, now: i64) -> Option<f64> {
    let mut owned: Vec<&Sample> = samples.iter().filter(|s| s.item_id == item_id).collect();
    if owned.is_empty() {
        return None;
    }
    owned.sort_by_key(|s| s.ts);
    let first = owned.first()?;
    let last = owned.last()?;
    if owned.len() >= 2 && last.ts - first.ts >= 600 {
        let hours = (last.ts - first.ts) as f64 / 3600.0;
        return Some(((last.views - first.views).max(0) as f64) / hours);
    }
    let age_hours = ((now - published_at).max(0) as f64 / 3600.0).max(0.25);
    Some(last.views as f64 / age_hours)
}

fn median(values: &mut Vec<f64>) -> f64 {
    if values.is_empty() {
        return 1.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

pub fn rank(
    groups: &[Group],
    samples: &[Sample],
    window: Window,
    weights: &Weights,
    now: i64,
) -> Vec<ScoredStory> {
    let mut group_of: HashMap<i64, usize> = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        for item_id in &group.item_ids {
            group_of.insert(*item_id, index);
        }
    }

    let mut rates: Vec<ItemRate> = Vec::new();
    for group in groups {
        for item in &group.items {
            if item.kind != SourceKind::Telegram {
                continue;
            }
            let Some(views) = item.views else { continue };
            let Some(rate) = views_per_hour(samples, item.item_id, item.published_at, now) else {
                continue;
            };
            rates.push(ItemRate { item_id: item.item_id, source_id: item.source_id, rate, views });
        }
    }

    // Per-channel medians: this is what stops a large channel's routine post from
    // outranking a small channel's breakout post.
    let mut per_channel: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for rate in &rates {
        per_channel.entry(rate.source_id).or_default().push(rate.rate);
    }
    let medians: BTreeMap<i64, f64> =
        per_channel.iter().map(|(id, values)| (*id, median(&mut values.clone()))).collect();

    let tau_hours = (window.seconds() as f64 / 3600.0) / 3.0;

    let mut stories: Vec<ScoredStory> = Vec::with_capacity(groups.len());
    for (group_index, group) in groups.iter().enumerate() {
        let mut per_outlet: BTreeMap<i64, OutletContribution> = BTreeMap::new();
        let mut any_uncited: BTreeMap<i64, bool> = BTreeMap::new();

        for item in &group.items {
            let entry = per_outlet.entry(item.outlet_id).or_insert_with(|| OutletContribution {
                outlet: item.outlet.clone(),
                weight: 1.0,
                newest: item.published_at,
                views: item.views,
                views_per_hour: None,
                title: item.title.clone(),
                url: item.url.clone(),
            });
            if item.published_at > entry.newest {
                entry.newest = item.published_at;
                entry.title = item.title.clone();
                entry.url = item.url.clone();
            }
            if item.kind == SourceKind::Telegram {
                if let Some(views) = item.views {
                    entry.views = Some(entry.views.unwrap_or(0).max(views));
                    entry.views_per_hour =
                        views_per_hour(samples, item.item_id, item.published_at, now);
                }
            }
            let flag = any_uncited.entry(item.outlet_id).or_insert(false);
            *flag = *flag || !item.cited;
        }

        // Weight is decided per outlet, once every item of that outlet is known: a single
        // uncited item makes the whole outlet count fully.
        let outlet_weight = |outlet_id: i64| {
            if any_uncited.get(&outlet_id).copied().unwrap_or(false) { 1.0 } else { 0.5 }
        };
        let coverage: f64 = per_outlet.keys().map(|outlet_id| outlet_weight(*outlet_id)).sum();

        let mut outlets: Vec<OutletContribution> = Vec::with_capacity(per_outlet.len());
        for (outlet_id, mut contribution) in per_outlet {
            contribution.weight = outlet_weight(outlet_id);
            outlets.push(contribution);
        }

        let mut engagement = 0.0;
        let mut view_count = 0i64;
        for rate in &rates {
            if group_of.get(&rate.item_id) != Some(&group_index) {
                continue;
            }
            let baseline = medians.get(&rate.source_id).copied().unwrap_or(1.0).max(1.0);
            engagement += rate.rate / baseline;
            view_count += rate.views;
        }

        let age_hours = ((now - group.newest).max(0) as f64) / 3600.0;
        let freshness = (-age_hours / tau_hours).exp();

        stories.push(ScoredStory {
            key: group.key.clone(),
            title: group.title.clone(),
            score: 0.0,
            coverage,
            coverage_norm: 0.0,
            engagement,
            engagement_norm: 0.0,
            freshness,
            view_count,
            newest: group.newest,
            outlets,
        });
    }

    let max_coverage = stories.iter().map(|s| s.coverage).fold(0.0, f64::max);
    let max_engagement = stories.iter().map(|s| s.engagement).fold(0.0, f64::max);
    for story in &mut stories {
        story.coverage_norm = if max_coverage > 0.0 { story.coverage / max_coverage } else { 0.0 };
        story.engagement_norm = if max_engagement > 0.0 { story.engagement / max_engagement } else { 0.0 };
        story.score = weights.coverage * story.coverage_norm
            + weights.engagement * story.engagement_norm
            + weights.freshness * story.freshness;
    }
    stories.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(b.newest.cmp(&a.newest))
    });
    stories
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Clusterer, Group};
    use crate::source::SourceKind;
    use crate::store::{ItemRow, Sample, Window};
    use std::sync::atomic::{AtomicI64, Ordering};

    const NOW: i64 = 1_700_000_000;

    /// Row ids are unique across every item in a real window, so two hand-built stories never
    /// share one. Numbering from 1 per story would let the second story's samples land on the
    /// first story's item and normalise against the wrong channel.
    static NEXT_ITEM_ID: AtomicI64 = AtomicI64::new(1);

    /// Build one story from a list of (outlet_id, source_id, title, age_hours, views, cited).
    fn story(rows: &[(i64, i64, &str, i64, Option<i64>, bool)]) -> Group {
        let items: Vec<ItemRow> = rows
            .iter()
            .enumerate()
            .map(|(index, (outlet_id, source_id, title, age_hours, views, cited))| ItemRow {
                item_id: NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
                source_id: *source_id,
                outlet_id: *outlet_id,
                outlet: format!("Outlet{outlet_id}"),
                kind: if views.is_some() { SourceKind::Telegram } else { SourceKind::Rss },
                title: title.to_string(),
                description: None,
                url: format!("https://example.az/{index}"),
                published_at: NOW - age_hours * 3600,
                views: *views,
                cited: *cited,
                is_backfill: false,
            })
            .collect();
        Clusterer::new(0.45).group_items(&items).remove(0)
    }

    fn views_for(group: &Group, views: i64, samples: &mut Vec<Sample>) {
        for item in &group.items {
            if item.views.is_some() {
                samples.push(Sample { item_id: item.item_id, ts: NOW - 3600, views: 0 });
                samples.push(Sample { item_id: item.item_id, ts: NOW, views });
            }
        }
    }

    #[test]
    fn coverage_counts_distinct_outlets_not_distinct_sources() {
        // One outlet publishing both an RSS feed and a Telegram channel casts one vote.
        let group = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, false),
            (1, 11, "Bakıda yollar bağlıdır", 1, Some(100), false),
        ]);
        let ranked = rank(&[group], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 1.0);
        assert_eq!(ranked[0].outlets.len(), 1);
    }

    #[test]
    fn a_cited_repost_is_worth_half_and_a_real_report_is_worth_one() {
        let cited_only = story(&[(1, 10, "Bakıda yollar bağlıdır", 1, None, true)]);
        let ranked = rank(&[cited_only], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 0.5);

        let mixed = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, true),
            (1, 11, "Bakıda yollar bağlıdır - Yenilik", 1, None, false),
        ]);
        let ranked = rank(&[mixed], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 1.0, "one uncited item makes the outlet count fully");
    }

    #[test]
    fn engagement_is_normalized_per_channel() {
        // Channel 10 habitually gets 1000 views/hour; channel 20 gets 50.
        let mut samples = Vec::new();
        let big = story(&[(1, 10, "Böyük kanal xəbəri budur", 1, Some(1000), false)]);
        let small = story(&[(2, 20, "Kiçik kanal xəbəri budur", 1, Some(50), false)]);
        views_for(&big, 1000, &mut samples);
        views_for(&small, 50, &mut samples);

        let ranked = rank(&[big, small], &samples, Window::Hour, &Weights::default(), NOW);
        let big_score = ranked.iter().find(|s| s.title.contains("Böyük")).unwrap();
        let small_score = ranked.iter().find(|s| s.title.contains("Kiçik")).unwrap();
        assert!(
            (big_score.engagement_norm - small_score.engagement_norm).abs() < 0.05,
            "each channel's median makes its own story the baseline: {} vs {}",
            big_score.engagement_norm,
            small_score.engagement_norm
        );
    }

    #[test]
    fn freshness_decays_within_the_window() {
        let fresh = story(&[(1, 10, "Təzə xəbər budur", 1, None, false)]);
        let stale = story(&[(2, 20, "Köhnə xəbər budur", 20, None, false)]);
        let ranked = rank(&[fresh, stale], &[], Window::Day, &Weights::default(), NOW);
        assert!(
            ranked[0].freshness > ranked[1].freshness + 0.3,
            "20 hours old must decay well above 1 hour old in a 24h window"
        );
    }

    #[test]
    fn broad_coverage_outranks_a_narrow_story() {
        let broad = story(&[
            (1, 10, "Geniş yayılmış xəbər budur", 1, None, false),
            (2, 20, "Geniş yayılmış xəbər budur", 1, None, false),
            (3, 30, "Geniş yayılmış xəbər budur", 1, None, false),
        ]);
        let narrow = story(&[(4, 40, "Yalnız bir yerdə olan xəbər", 1, None, false)]);
        let ranked = rank(&[broad, narrow], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 3.0);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn views_per_hour_uses_the_sample_slope_then_falls_back_to_age() {
        let samples = vec![
            Sample { item_id: 1, ts: NOW - 3600, views: 100 },
            Sample { item_id: 1, ts: NOW, views: 700 },
        ];
        let rate = views_per_hour(&samples, 1, NOW - 7200, NOW).unwrap();
        assert!((rate - 600.0).abs() < 1e-6);

        // A single sample two hours old: 400 views spread over two hours.
        let single = vec![Sample { item_id: 2, ts: NOW, views: 400 }];
        let rate = views_per_hour(&single, 2, NOW - 7200, NOW).unwrap();
        assert!((rate - 200.0).abs() < 1e-6);

        // A brand-new post is floored at a quarter hour so it cannot divide by zero.
        let fresh = vec![Sample { item_id: 3, ts: NOW, views: 10 }];
        let rate = views_per_hour(&fresh, 3, NOW, NOW).unwrap();
        assert!((rate - 40.0).abs() < 1e-6);
    }

    #[test]
    fn stories_are_returned_best_first() {
        let broad = story(&[
            (1, 10, "Geniş yayılmış xəbər budur", 1, None, false),
            (2, 20, "Geniş yayılmış xəbər budur", 1, None, false),
        ]);
        let narrow = story(&[(4, 40, "Yalnız bir yerdə olan xəbər", 1, None, false)]);
        let ranked = rank(&[narrow, broad], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 2.0);
    }
}
