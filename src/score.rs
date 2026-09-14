//! Popularity scoring: cross-outlet coverage, reader engagement, and freshness.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::cluster::Group;
use crate::source::SourceKind;
use crate::store::{Sample, Window};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub coverage: f64,
    pub engagement: f64,
    pub freshness: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            coverage: 0.40,
            engagement: 0.40,
            freshness: 0.20,
        }
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

/// Views gained per hour for one item's samples, which must already be sorted by `ts`. With two
/// samples at least ten minutes apart this is the observed slope; otherwise it spreads the total
/// over the item's age, floored at a quarter hour. Both paths return views per hour, so callers
/// can add rates from different items together.
fn rate_of(owned: &[&Sample], published_at: i64, now: i64) -> Option<f64> {
    let first = owned.first()?;
    let last = owned.last()?;
    if owned.len() >= 2 && last.ts - first.ts >= 600 {
        let hours = (last.ts - first.ts) as f64 / 3600.0;
        return Some(((last.views - first.views).max(0) as f64) / hours);
    }
    let age_hours = ((now - published_at).max(0) as f64 / 3600.0).max(0.25);
    Some(last.views as f64 / age_hours)
}

/// Views gained per hour for one item, scanned out of a raw sample slice. `rank` goes through
/// [`sample_index`] instead, which sorts each item's samples once rather than once per call;
/// both routes end in [`rate_of`], so they cannot disagree.
pub fn views_per_hour(
    samples: &[Sample],
    item_id: i64,
    published_at: i64,
    now: i64,
) -> Option<f64> {
    let mut owned: Vec<&Sample> = samples.iter().filter(|s| s.item_id == item_id).collect();
    owned.sort_by_key(|s| s.ts);
    rate_of(&owned, published_at, now)
}

/// Every item's samples, grouped by item and sorted by `ts`, built once per `rank` call. A
/// per-item filter over the whole slice would cost items x samples and re-sort the same rows
/// again and again; on a week of Telegram posts that is the difference between milliseconds and
/// seconds.
fn sample_index(samples: &[Sample]) -> HashMap<i64, Vec<&Sample>> {
    let mut index: HashMap<i64, Vec<&Sample>> = HashMap::new();
    for sample in samples {
        index.entry(sample.item_id).or_default().push(sample);
    }
    for owned in index.values_mut() {
        owned.sort_by_key(|s| s.ts);
    }
    index
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 1.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
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

    // Each Telegram item's rate is computed exactly once here and reused by its outlet's
    // contribution and by its story's engagement total.
    let by_item = sample_index(samples);
    let mut rates: Vec<ItemRate> = Vec::new();
    let mut rate_of_item: HashMap<i64, f64> = HashMap::new();
    for group in groups {
        for item in &group.items {
            if item.kind != SourceKind::Telegram {
                continue;
            }
            let Some(views) = item.views else { continue };
            let Some(rate) = by_item
                .get(&item.item_id)
                .and_then(|owned| rate_of(owned, item.published_at, now))
            else {
                continue;
            };
            rates.push(ItemRate {
                item_id: item.item_id,
                source_id: item.source_id,
                rate,
                views,
            });
            rate_of_item.insert(item.item_id, rate);
        }
    }

    // Per-channel medians: this is what stops a large channel's routine post from
    // outranking a small channel's breakout post.
    let mut per_channel: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for rate in &rates {
        per_channel
            .entry(rate.source_id)
            .or_default()
            .push(rate.rate);
    }
    let medians: BTreeMap<i64, f64> = per_channel
        .iter()
        .map(|(id, values)| (*id, median(&mut values.clone())))
        .collect();

    let tau_hours = (window.seconds() as f64 / 3600.0) / 3.0;

    let mut stories: Vec<ScoredStory> = Vec::with_capacity(groups.len());
    for (group_index, group) in groups.iter().enumerate() {
        let mut per_outlet: BTreeMap<i64, OutletContribution> = BTreeMap::new();
        let mut any_uncited: BTreeMap<i64, bool> = BTreeMap::new();

        for item in &group.items {
            let entry = per_outlet
                .entry(item.outlet_id)
                .or_insert_with(|| OutletContribution {
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
            if item.kind == SourceKind::Telegram
                && let Some(views) = item.views
            {
                entry.views = Some(entry.views.unwrap_or(0).max(views));
                entry.views_per_hour = rate_of_item.get(&item.item_id).copied();
            }
            let flag = any_uncited.entry(item.outlet_id).or_insert(false);
            *flag = *flag || !item.cited;
        }

        // Weight is decided per outlet, once every item of that outlet is known: a single
        // uncited item makes the whole outlet count fully.
        let outlet_weight = |outlet_id: i64| {
            if any_uncited.get(&outlet_id).copied().unwrap_or(false) {
                1.0
            } else {
                0.5
            }
        };
        let coverage: f64 = per_outlet
            .keys()
            .map(|outlet_id| outlet_weight(*outlet_id))
            .sum();

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
            let baseline = medians
                .get(&rate.source_id)
                .copied()
                .unwrap_or(1.0)
                .max(1.0);
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
        story.coverage_norm = if max_coverage > 0.0 {
            story.coverage / max_coverage
        } else {
            0.0
        };
        story.engagement_norm = if max_engagement > 0.0 {
            story.engagement / max_engagement
        } else {
            0.0
        };
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

    /// One hand-built story row: (outlet_id, source_id, title, age_hours, views, cited).
    type StoryRow<'a> = (i64, i64, &'a str, i64, Option<i64>, bool);

    /// Build one story from a list of rows.
    fn story(rows: &[StoryRow<'_>]) -> Group {
        let items: Vec<ItemRow> = rows
            .iter()
            .enumerate()
            .map(
                |(index, (outlet_id, source_id, title, age_hours, views, cited))| ItemRow {
                    item_id: NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
                    source_id: *source_id,
                    outlet_id: *outlet_id,
                    outlet: format!("Outlet{outlet_id}"),
                    kind: if views.is_some() {
                        SourceKind::Telegram
                    } else {
                        SourceKind::Rss
                    },
                    title: title.to_string(),
                    description: None,
                    url: format!("https://example.az/{index}"),
                    published_at: NOW - age_hours * 3600,
                    views: *views,
                    cited: *cited,
                    is_backfill: false,
                },
            )
            .collect();
        Clusterer::new(0.45).group_items(&items).remove(0)
    }

    fn views_for(group: &Group, views: i64, samples: &mut Vec<Sample>) {
        for item in &group.items {
            if item.views.is_some() {
                samples.push(Sample {
                    item_id: item.item_id,
                    ts: NOW - 3600,
                    views: 0,
                });
                samples.push(Sample {
                    item_id: item.item_id,
                    ts: NOW,
                    views,
                });
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
        assert_eq!(
            ranked[0].coverage, 1.0,
            "one uncited item makes the outlet count fully"
        );
    }

    #[test]
    fn engagement_is_normalized_per_channel() {
        // Channel 10 carries a breakout post (1000 views/hour) and a routine one (100); channel
        // 20 carries one post at 50. A per-story median would score all three at exactly 1.0, so
        // the numbers below only hold when the baseline is the channel's whole window.
        let mut samples = Vec::new();
        let big = story(&[(1, 10, "Böyük kanal xəbəri budur", 1, Some(1000), false)]);
        let routine = story(&[(1, 10, "Adi gün xəbəri budur", 1, Some(100), false)]);
        let small = story(&[(2, 20, "Kiçik kanal xəbəri budur", 1, Some(50), false)]);
        views_for(&big, 1000, &mut samples);
        views_for(&routine, 100, &mut samples);
        views_for(&small, 50, &mut samples);

        let ranked = rank(
            &[big, routine, small],
            &samples,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let score = |title: &str| ranked.iter().find(|s| s.title.contains(title)).unwrap();
        let big_score = score("Böyük");
        let routine_score = score("Adi");
        let small_score = score("Kiçik");

        // Channel 10's window median is (100 + 1000) / 2 = 550, not each story's own rate.
        assert!(
            (big_score.engagement - 1000.0 / 550.0).abs() < 1e-9,
            "breakout post against the channel median: {}",
            big_score.engagement
        );
        assert!(
            (routine_score.engagement - 100.0 / 550.0).abs() < 1e-9,
            "routine post against the same channel median: {}",
            routine_score.engagement
        );
        assert!(
            (small_score.engagement - 1.0).abs() < 1e-9,
            "the small channel's only post is its own median: {}",
            small_score.engagement
        );

        // The 50-view channel is not dwarfed by the 1000-view one: normalised against the
        // window's best, it keeps 550/1000 of the score a raw view count would have cut to 1/20.
        assert!((big_score.engagement_norm - 1.0).abs() < 1e-9);
        assert!((small_score.engagement_norm - 550.0 / 1000.0).abs() < 1e-9);
        assert!((routine_score.engagement_norm - 0.1).abs() < 1e-9);
        assert!(big_score.engagement_norm > small_score.engagement_norm);
        assert!(small_score.engagement_norm > routine_score.engagement_norm);
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
        let ranked = rank(
            &[broad, narrow],
            &[],
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 3.0);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn views_per_hour_uses_the_sample_slope_then_falls_back_to_age() {
        let samples = vec![
            Sample {
                item_id: 1,
                ts: NOW - 3600,
                views: 100,
            },
            Sample {
                item_id: 1,
                ts: NOW,
                views: 700,
            },
        ];
        let rate = views_per_hour(&samples, 1, NOW - 7200, NOW).unwrap();
        assert!((rate - 600.0).abs() < 1e-6);

        // A single sample two hours old: 400 views spread over two hours.
        let single = vec![Sample {
            item_id: 2,
            ts: NOW,
            views: 400,
        }];
        let rate = views_per_hour(&single, 2, NOW - 7200, NOW).unwrap();
        assert!((rate - 200.0).abs() < 1e-6);

        // A brand-new post is floored at a quarter hour so it cannot divide by zero.
        let fresh = vec![Sample {
            item_id: 3,
            ts: NOW,
            views: 10,
        }];
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
        let ranked = rank(
            &[narrow, broad],
            &[],
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 2.0);
    }

    #[test]
    fn a_story_scores_only_from_its_own_items_samples() {
        // One slice holds every story's samples, interleaved and with each item's own rows out
        // of order, so an index keyed on anything but the item id would attribute one story's
        // views to another. Each channel has one item, so a correctly attributed rate is its
        // own channel's median and every story's engagement is exactly 1.0.
        let slipped = story(&[(1, 10, "Böyük yol qəzası budur", 2, Some(600), false)]);
        let single = story(&[(2, 20, "Kiçik kanal xəbəri budur", 2, Some(120), false)]);
        let risen = story(&[(3, 30, "Orta xəbər belə gəldi", 1, Some(250), false)]);
        let (slipped_id, single_id, risen_id) = (
            slipped.items[0].item_id,
            single.items[0].item_id,
            risen.items[0].item_id,
        );

        let samples = vec![
            Sample {
                item_id: slipped_id,
                ts: NOW - 3600,
                views: 600,
            },
            Sample {
                item_id: single_id,
                ts: NOW,
                views: 120,
            },
            Sample {
                item_id: risen_id,
                ts: NOW,
                views: 250,
            },
            Sample {
                item_id: slipped_id,
                ts: NOW - 7200,
                views: 0,
            },
            Sample {
                item_id: risen_id,
                ts: NOW - 3600,
                views: 100,
            },
        ];

        let ranked = rank(
            &[slipped, single, risen],
            &samples,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let story = |title: &str| ranked.iter().find(|s| s.title == title).unwrap();

        let slipped = story("Böyük yol qəzası budur");
        assert_eq!(
            slipped.outlets[0].views_per_hour,
            Some(600.0),
            "slope branch, own samples only"
        );
        assert_eq!(slipped.view_count, 600);
        assert_eq!(slipped.engagement, 1.0);

        let single = story("Kiçik kanal xəbəri budur");
        assert_eq!(
            single.outlets[0].views_per_hour,
            Some(60.0),
            "age fallback, own sample only"
        );
        assert_eq!(single.view_count, 120);
        assert_eq!(single.engagement, 1.0);

        let risen = story("Orta xəbər belə gəldi");
        assert_eq!(
            risen.outlets[0].views_per_hour,
            Some(150.0),
            "slope branch, own samples only"
        );
        assert_eq!(risen.view_count, 250);
        assert_eq!(risen.engagement, 1.0);
    }
}
