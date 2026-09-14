//! Popularity scoring: cross-outlet coverage, reader engagement, and freshness.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::cluster::Group;
use crate::source::SourceKind;
use crate::store::{ItemRow, Sample, Window};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub coverage: f64,
    pub engagement: f64,
    pub freshness: f64,
    pub spread_velocity: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            coverage: 0.35,
            engagement: 0.35,
            freshness: 0.20,
            spread_velocity: 0.10,
        }
    }
}

/// A channel's own median velocity is only a baseline once there is enough of it. Below this
/// many rated posts the median describes the sample, not the channel, and one viral post
/// would set the bar for everything that follows it.
const MIN_BASELINE_POSTS: usize = 20;

/// Ratios below this are measurement noise: a channel whose typical post gains under one view
/// per hour is not a denominator anything should be divided by.
const MIN_BASELINE: f64 = 1.0;

/// Ceiling on one post's ratio to its channel baseline. Without it a single viral post on a
/// quiet channel decides the whole ranking on its own, and the ranking stops being about how
/// widely a story is being read.
const MAX_RELATIVE_VELOCITY: f64 = 10.0;

/// Identifies the scoring arithmetic behind a stored ranking. Two rankings computed by
/// different versions are not comparable, and a movement claim between them would be invented
/// rather than measured. Bump this whenever a change here alters any story's score or order.
pub const ALGORITHM_VERSION: i64 = 2;

/// The recent interval an item's current pace is measured over.
///
/// A rate averaged over the item's whole life answers "how did this do overall", not "what is
/// happening now", and the two diverge exactly when the news is over: a post that gained ten
/// thousand views in its first hour and nothing since still shows five thousand views an hour
/// against a two-hour average. Only samples inside this interval describe the current pace.
const VELOCITY_RECENT_SECS: i64 = 3600;

/// How old the newest sample may be before the number stops describing current pace.
///
/// The poller samples each item at most once per ten minutes, so half an hour without a sample
/// means the source stopped answering for this item — a count that has not moved since is
/// history, not a measurement, and showing it as current would present a stale number as news.
const VELOCITY_STALE_AFTER_SECS: i64 = 1800;

/// Two samples closer than this cannot show a slope. Telegram counts move in steps and the
/// poller samples an item at most once per ten minutes, so a shorter span is step noise.
const MIN_MEASURED_SPAN_SECS: i64 = 600;

/// Floor on the age a single-sample estimate is divided by: a post published a minute ago has
/// had no time to accumulate, and dividing by a fraction of an hour would report a spike.
const MIN_AGE_HOURS: f64 = 0.25;

/// Below this many stories, a 95th percentile is just the maximum wearing a hat, so the
/// maximum is used directly and the intent is at least honest.
///
/// The number is not a taste: nearest-rank `P95` is `ceil(0.95n)`-th of `n`, which for `n < 20`
/// is the largest value itself. Twenty stories is where the percentile first drops one rank
/// below the maximum, so below twenty there is nothing to choose between them.
const MIN_STORIES_FOR_P95: usize = 20;

/// What an item is, relative to the outlet that did the reporting.
///
/// The three variants describe what the text says, and nothing more. None of them is a verdict
/// on the journalism: this program reads a small marker table, and "no citation found" means
/// exactly that — the item is credited with the full coverage weight because nothing in it
/// contradicts that, not because it was proved to be original reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Provenance {
    /// No citation marker was found, or the marker names nobody knowable. The item carries the
    /// full coverage weight, for want of anything that says otherwise.
    Independent,
    /// A repeat that names the outlet it took the story from. It says someone else did the
    /// reporting, which is the one direction the markers do support.
    Citation,
    /// A repeat that credits nobody. It is not the outlet's own work — someone had to report it
    /// first — but nobody can prove which side of that line it is on, so it keeps the half
    /// weight this project used before provenance was split.
    Repost,
}

impl Provenance {
    pub fn of(item: &ItemRow) -> Self {
        match (item.cited, item.cited_outlet.is_some()) {
            (false, _) => Self::Independent,
            (true, true) => Self::Citation,
            (true, false) => Self::Repost,
        }
    }

    /// How much this item contributes to the story's coverage.
    pub fn coverage_weight(self) -> f64 {
        match self {
            Self::Independent => 1.0,
            Self::Citation => 0.0,
            Self::Repost => 0.5,
        }
    }

    /// Rank used to pick one label for an outlet that published several kinds of item: a
    /// single piece of original reporting makes the outlet a source, whatever else it ran.
    fn rank(self) -> u8 {
        match self {
            Self::Independent => 2,
            Self::Repost => 1,
            Self::Citation => 0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::Citation => "citation",
            Self::Repost => "repost",
        }
    }
}

/// Per-channel typical velocity, plus the corpus-wide median to fall back on.
///
/// The fallback is not a nicety. A channel that the app has only just started following has
/// no history, and dividing its posts by nothing would either panic or invent a baseline.
#[derive(Debug, Clone)]
pub struct Baselines {
    per_channel: HashMap<i64, f64>,
    global: f64,
}

impl Baselines {
    /// No channel knowledge at all: every post is measured against the floor. Callers that
    /// cannot load history still rank, and every ratio stays bounded.
    pub fn empty() -> Self {
        Self {
            per_channel: HashMap::new(),
            global: MIN_BASELINE,
        }
    }

    /// Baselines the caller already knows, for a channel pace measured from something other
    /// than the sample table — a test with a hand-built fixture, or a future cache warmer.
    /// Values below the floor are raised to it, so the same guarantee holds as [`from_items`].
    pub fn from_medians(per_channel: HashMap<i64, f64>, global: f64) -> Self {
        Self {
            per_channel: per_channel
                .into_iter()
                .map(|(source_id, rate)| (source_id, rate.max(MIN_BASELINE)))
                .collect(),
            global: global.max(MIN_BASELINE),
        }
    }

    /// Median measured velocity per channel, over every rated Telegram item in `items`.
    ///
    /// `items` should be wider than one scoring window: a channel's typical pace is a property
    /// of the channel, not of the hour being ranked, and a one-hour sample would fall back to
    /// the global median almost always.
    ///
    /// Only observed slopes take part. A baseline is the divisor of a measured pace, and a
    /// divisor built from single-sample estimates would mix an average since publication into a
    /// comparison of current paces; an item that was never measured therefore contributes
    /// nothing rather than an estimate.
    pub fn from_items(items: &[ItemRow], samples: &[Sample], now: i64) -> Self {
        let by_item = sample_index(samples);
        let mut per_channel: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
        for item in items {
            if item.kind != SourceKind::Telegram {
                continue;
            }
            if item.views.is_none() {
                continue;
            }
            let Some(velocity) = by_item
                .get(&item.item_id)
                .and_then(|owned| measured_velocity(owned, item.published_at, now))
            else {
                continue;
            };
            per_channel
                .entry(item.source_id)
                .or_default()
                .push(velocity.per_hour);
        }

        let mut all: Vec<f64> = per_channel.values().flatten().copied().collect();
        let global = if all.is_empty() {
            MIN_BASELINE
        } else {
            median(&mut all).max(MIN_BASELINE)
        };

        let per_channel = per_channel
            .into_iter()
            .filter(|(_, rates)| rates.len() >= MIN_BASELINE_POSTS)
            .map(|(source_id, mut rates)| (source_id, median(&mut rates).max(MIN_BASELINE)))
            .collect();

        Self {
            per_channel,
            global,
        }
    }

    /// The baseline for one channel: its own median when it has earned one, the corpus median
    /// otherwise, and never below the floor.
    pub fn of(&self, source_id: i64) -> f64 {
        self.per_channel
            .get(&source_id)
            .copied()
            .unwrap_or(self.global)
            .max(MIN_BASELINE)
    }

    /// True when this channel is measured against the corpus median rather than its own.
    pub fn is_fallback(&self, source_id: i64) -> bool {
        !self.per_channel.contains_key(&source_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OutletContribution {
    pub outlet: String,
    /// Coverage weight of this outlet: the best provenance any of its items reached.
    pub weight: f64,
    pub provenance: Provenance,
    pub newest: i64,
    pub views: Option<i64>,
    /// Best post of this outlet by relative pace, and how that pace was obtained. `None` when
    /// none of its posts carries a view sample.
    pub velocity: Option<Velocity>,
    /// `velocity` as a multiple of its channel's measured pace, capped. Present only for a
    /// measured post: dividing an estimate by a measured median would compare two different
    /// kinds of number.
    pub relative_velocity: Option<f64>,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoredStory {
    pub key: String,
    pub title: String,
    /// A short body for the story, when any of its items carried one: the lede a reader can
    /// use to decide whether the headline is worth opening.
    pub description: Option<String>,
    pub score: f64,
    /// Outlets with no citation found, and their repeat weight. Repeats count toward
    /// [`Self::spread`], not here.
    pub coverage: f64,
    pub coverage_norm: f64,
    pub engagement: f64,
    pub engagement_norm: f64,
    pub freshness: f64,
    /// Best measurement state behind [`Self::engagement`]: measured when any outlet's
    /// contribution was an observed slope, otherwise estimated, stale, or absent.
    pub engagement_basis: Option<VelocityBasis>,
    /// Timestamp of the newest sample behind that contribution, so the screen can show how old
    /// the measurement is instead of implying it is from this second.
    pub engagement_observed_at: Option<i64>,
    /// Every outlet carrying the story, cited or not.
    pub spread: usize,
    /// Outlets that picked the story up inside the recent sub-window.
    pub spread_velocity: f64,
    pub spread_velocity_norm: f64,
    /// First and last moment the story actually developed.
    pub started_at: i64,
    pub updated_at: i64,
    pub view_count: i64,
    pub newest: i64,
    pub outlets: Vec<OutletContribution>,
}

/// How a views-per-hour number was obtained, so nothing on screen can present an estimate as a
/// measurement, stale history as current pace, or absent data as a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum VelocityBasis {
    /// Two or more samples inside the recent interval, at least ten minutes apart: an observed
    /// slope, and the only kind of number the score is built from.
    Measured,
    /// One fresh sample, or several too close together: total views spread over the item's age.
    /// An estimate of the average pace since publication, not an observation of it.
    Estimated,
    /// The newest sample is older than the staleness bound. What was last seen, and no claim
    /// about now.
    Stale,
}

/// A views-per-hour number with the evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Velocity {
    pub per_hour: f64,
    pub basis: VelocityBasis,
    /// Span between the samples behind a measured slope, in seconds. Zero when nothing was
    /// measured.
    pub observed_seconds: i64,
    /// Timestamp of the newest sample the number rests on: its age is the age of the
    /// measurement.
    pub observed_at: i64,
}

impl Velocity {
    /// True for the one basis the score may use: an observed slope.
    pub fn is_measured(self) -> bool {
        self.basis == VelocityBasis::Measured
    }
}

/// Views per hour from one item's samples, which must already be sorted by `ts`.
///
/// `recent` bounds which samples may take part: only those at or after `now - recent`. Within
/// that interval two samples at least [`MIN_MEASURED_SPAN_SECS`] apart give the observed slope;
/// anything else gives the item's lifetime average as an estimate. `None` when the interval
/// holds no sample at all, which is not a zero rate — it is no observation.
fn velocity_of(owned: &[&Sample], published_at: i64, now: i64, recent: i64) -> Option<Velocity> {
    let cutoff = now.saturating_sub(recent);
    // `owned` is sorted by `ts`, so the samples inside the interval are a contiguous tail and
    // the first and last of them are the ends of the slope.
    let start = owned.partition_point(|sample| sample.ts < cutoff);
    let window = &owned[start..];
    let first = window.first()?;
    let newest = window.last()?;

    if window.len() >= 2 && newest.ts - first.ts >= MIN_MEASURED_SPAN_SECS {
        let hours = (newest.ts - first.ts) as f64 / 3600.0;
        let gained = newest.views.saturating_sub(first.views).max(0) as f64;
        let per_hour = gained / hours;
        if per_hour.is_finite() {
            return Some(Velocity {
                per_hour,
                basis: VelocityBasis::Measured,
                observed_seconds: newest.ts - first.ts,
                observed_at: newest.ts,
            });
        }
    }

    let age_hours = ((now - published_at).max(0) as f64 / 3600.0).max(MIN_AGE_HOURS);
    let per_hour = newest.views as f64 / age_hours;
    if !per_hour.is_finite() {
        return None;
    }
    Some(Velocity {
        per_hour,
        basis: VelocityBasis::Estimated,
        observed_seconds: 0,
        observed_at: newest.ts,
    })
}

/// The item's current pace, or the last one observed when the samples stopped.
///
/// An item whose newest sample is older than [`VELOCITY_STALE_AFTER_SECS`], or which has none
/// inside the recent interval, returns a [`VelocityBasis::Stale`] number: the caller can show
/// it as history but must not score it as today's engagement.
pub fn current_velocity(
    samples: &[Sample],
    item_id: i64,
    published_at: i64,
    now: i64,
) -> Option<Velocity> {
    let mut owned: Vec<&Sample> = samples.iter().filter(|s| s.item_id == item_id).collect();
    owned.sort_by_key(|s| s.ts);
    current_velocity_of(&owned, published_at, now)
}

/// [`current_velocity`] over samples the caller has already indexed and sorted.
fn current_velocity_of(owned: &[&Sample], published_at: i64, now: i64) -> Option<Velocity> {
    let last = owned.last()?;
    let recent = velocity_of(owned, published_at, now, VELOCITY_RECENT_SECS);
    match recent {
        Some(velocity) if now - velocity.observed_at <= VELOCITY_STALE_AFTER_SECS => Some(velocity),
        // Samples exist but none is fresh enough to describe now. The number is still worth
        // showing as a last measurement, and is explicitly not a current pace.
        _ => {
            let mut stale = velocity_of(owned, published_at, now, i64::MAX)?;
            stale.basis = VelocityBasis::Stale;
            stale.observed_at = last.ts;
            Some(stale)
        }
    }
}

/// An item's whole-history pace, ignoring staleness. Used for channel baselines, which describe
/// how a channel behaved while it could be observed rather than how it behaves this minute.
fn measured_velocity(owned: &[&Sample], published_at: i64, now: i64) -> Option<Velocity> {
    let velocity = velocity_of(owned, published_at, now, i64::MAX)?;
    velocity.is_measured().then_some(velocity)
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

/// Nearest-rank percentile: the value at or below which `percentile` per cent of the sample
/// falls. Sorts a copy, so the caller's order survives.
pub fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let fraction = (percentile / 100.0).clamp(0.0, 1.0);
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// The divisor that maps a raw signal into 0..1: its 95th percentile when the window holds
/// enough stories, its maximum otherwise.
///
/// Dividing by the maximum looks equivalent and is not. One outlier — a story carried by
/// twenty outlets, or a post that went viral on a small channel — pushes every other story
/// toward zero, and the ranking below the leader stops carrying information. The 95th
/// percentile is what the top of a distribution actually looks like once one tail value is
/// discounted. With too few stories a percentile is just the maximum under another name, so
/// the maximum is used and the intent stays honest. Zero means "no scale", and every caller
/// must then score zero rather than divide.
pub fn robust_scale(values: &[f64]) -> f64 {
    let max = values.iter().copied().fold(0.0, f64::max);
    if values.len() < MIN_STORIES_FOR_P95 {
        return max;
    }
    let p95 = percentile(values, 95.0);
    if p95 > 0.0 { p95 } else { max }
}

/// Map a value into 0..1 against a scale, never dividing by zero and never returning a
/// non-finite number. A NaN that reached the sort would compare equal to everything and
/// silently reorder the ranking.
pub fn normalize(value: f64, scale: f64) -> f64 {
    if !value.is_finite() || scale <= 0.0 {
        return 0.0;
    }
    (value / scale).clamp(0.0, 1.0)
}

/// The last moment the story actually developed: the newest first arrival of an outlet that had
/// not carried it yet.
///
/// Freshness is measured from here, not from the newest item. A channel that reposts the same
/// headline every five minutes would otherwise hold its story at the top forever, which is
/// exactly backwards — nothing new happened. A story that is genuinely developing gains
/// outlets, and those do move this forward.
///
/// Taken from [`Group::outlet_first`], which covers the group's whole history. A scoring window
/// trims `items` to its own range, and reading freshness off the trimmed list would date the
/// story to the oldest report *inside* the window: a story that has been running for hours
/// would look brand new the moment the hour rolled over.
fn updated_at(group: &Group) -> i64 {
    group
        .outlet_first
        .values()
        .copied()
        .max()
        .unwrap_or(group.oldest)
}

/// How many outlets first picked the story up inside the recent sub-window.
///
/// First arrivals, not posts: an outlet that has been carrying the story for hours is not the
/// story spreading, it is the story sitting still. Each outlet counts once however often it
/// repeats, for the same reason coverage does.
///
/// Also read from the whole history, so a repost by an outlet that reported before the window
/// is not mistaken for a fresh pickup.
fn spread_velocity(group: &Group, now: i64, span: i64) -> f64 {
    let cutoff = now.saturating_sub(span);
    group
        .outlet_first
        .values()
        .filter(|first| **first >= cutoff)
        .count() as f64
}

/// Order of measurement kinds, for choosing which post speaks for an outlet. A measured slope
/// outranks an estimate, and an estimate outranks history: the score counts measured paces, so
/// the post that represents the outlet must be one of them whenever one exists.
fn pace_kind(basis: VelocityBasis) -> u8 {
    match basis {
        VelocityBasis::Measured => 2,
        VelocityBasis::Estimated => 1,
        VelocityBasis::Stale => 0,
    }
}

/// Whether a candidate post should replace the one currently representing an outlet.
fn pace_better(
    candidate: Velocity,
    candidate_relative: Option<f64>,
    previous: Velocity,
    previous_relative: Option<f64>,
) -> bool {
    let key = |velocity: Velocity, relative: Option<f64>| {
        (
            pace_kind(velocity.basis),
            relative.unwrap_or(velocity.per_hour),
        )
    };
    key(candidate, candidate_relative) > key(previous, previous_relative)
}

/// How much of a story's body travels into the ranked list. Long enough for a lede, short
/// enough that a Telegram post's full text is not copied into every story on every refresh.
const DESCRIPTION_CHARS: usize = 280;

/// The freshest body any item of the story carried. A feed that ships no summary must not blank
/// out the description a channel's post of the same story does carry, so the newest item that
/// has one wins.
fn short_description(group: &Group) -> Option<String> {
    group
        .items
        .iter()
        .filter(|item| {
            item.description
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        })
        .max_by_key(|item| item.published_at)
        .and_then(|item| item.description.as_deref())
        .map(clip_chars)
}

/// Truncate on a character boundary, so a headline that runs into the cap cannot split a
/// multi-byte character and panic.
fn clip_chars(text: &str) -> String {
    match text.char_indices().nth(DESCRIPTION_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// Score stories and return them best first.
///
/// `groups` are the window's stories — what happened inside the period being ranked. Their
/// items carry the window's own coverage and view counts. Freshness and spread are read from
/// each group's whole history instead (`Group::outlet_first`), so trimming an item out of the
/// window cannot turn a repeat into a fresh pickup. Callers that hold the same grouping over a
/// wider range must therefore pass the *same* group objects, trimmed by [`Group::items`] only.
///
/// The result is an index of prominence by the sources this program can see: how many outlets
/// reported the story, how fast its Telegram posts moved relative to their channels, how
/// recently it developed, and how many outlets picked it up inside the recent sub-window. It is
/// not a count of readers and not a probability that anyone read anything.
pub fn rank(
    groups: &[Group],
    samples: &[Sample],
    baselines: &Baselines,
    window: Window,
    weights: &Weights,
    now: i64,
) -> Vec<ScoredStory> {
    // Every Telegram item's samples are sorted once here and reused by its outlet's
    // contribution, rather than re-scanned and re-sorted per item.
    let by_item = sample_index(samples);
    let tau_hours = (window.seconds() as f64 / 3600.0) / 3.0;
    let spread_span = window.spread_seconds();

    let mut stories: Vec<ScoredStory> = Vec::with_capacity(groups.len());
    for group in groups {
        let mut per_outlet: BTreeMap<i64, OutletContribution> = BTreeMap::new();

        for item in &group.items {
            let provenance = Provenance::of(item);
            let entry = per_outlet
                .entry(item.outlet_id)
                .or_insert_with(|| OutletContribution {
                    outlet: item.outlet.clone(),
                    weight: 0.0,
                    provenance,
                    newest: item.published_at,
                    views: None,
                    velocity: None,
                    relative_velocity: None,
                    title: item.title.clone(),
                    url: item.url.clone(),
                });
            // The outlet's best provenance wins: one piece of original reporting makes the
            // outlet a source, whatever else it also ran.
            if provenance.rank() > entry.provenance.rank() {
                entry.provenance = provenance;
            }
            if item.published_at > entry.newest {
                entry.newest = item.published_at;
                entry.title = item.title.clone();
                entry.url = item.url.clone();
            }
            if item.kind == SourceKind::Telegram
                && let Some(views) = item.views
            {
                // The outlet's view count is its best post, not the sum of its posts: five
                // reposts of one story reach the same readers five times over.
                entry.views = Some(entry.views.unwrap_or(0).max(views));
                if let Some(velocity) = by_item
                    .get(&item.item_id)
                    .and_then(|owned| current_velocity_of(owned, item.published_at, now))
                {
                    // One number per outlet, chosen by relative pace rather than by raw views
                    // per hour. An outlet may own several channels and they are not the same
                    // size: a post at 1x on a big channel and a post at 10x on a small one are
                    // the same raw number to nobody, and the baseline is the only thing that
                    // makes the two comparable. The count is still the outlet's best post, so
                    // the outlet enters the score once.
                    let relative = velocity.is_measured().then(|| {
                        (velocity.per_hour / baselines.of(item.source_id))
                            .min(MAX_RELATIVE_VELOCITY)
                    });
                    if entry.velocity.is_none_or(|previous| {
                        pace_better(velocity, relative, previous, entry.relative_velocity)
                    }) {
                        entry.velocity = Some(velocity);
                        entry.relative_velocity = relative;
                    }
                }
            }
        }

        let mut coverage = 0.0;
        let mut engagement = 0.0;
        let mut view_count = 0i64;
        let mut basis: Option<VelocityBasis> = None;
        let mut observed_at: Option<i64> = None;
        let mut outlets: Vec<OutletContribution> = Vec::with_capacity(per_outlet.len());
        for (_, mut contribution) in per_outlet {
            contribution.weight = contribution.provenance.coverage_weight();
            coverage += contribution.weight;
            // Only observed slopes enter the sum, and only through their ratio to the channel's
            // measured pace: an estimate divided by a measured median is not a comparable
            // number, and a stale count describes a story that has stopped moving.
            engagement += contribution.relative_velocity.unwrap_or(0.0);
            view_count = view_count.saturating_add(contribution.views.unwrap_or(0));
            if let Some(velocity) = contribution.velocity
                && basis.is_none_or(|current| pace_kind(velocity.basis) > pace_kind(current))
            {
                basis = Some(velocity.basis);
                observed_at = Some(velocity.observed_at);
            }
            outlets.push(contribution);
        }

        let age_hours = ((now - updated_at(group)).max(0) as f64) / 3600.0;
        let freshness = (-age_hours / tau_hours).exp();

        stories.push(ScoredStory {
            key: group.key.clone(),
            title: group.title.clone(),
            description: short_description(group),
            score: 0.0,
            coverage,
            coverage_norm: 0.0,
            engagement,
            engagement_norm: 0.0,
            freshness,
            engagement_basis: basis,
            engagement_observed_at: observed_at,
            spread: outlets.len(),
            spread_velocity: spread_velocity(group, now, spread_span),
            spread_velocity_norm: 0.0,
            started_at: group.oldest,
            updated_at: updated_at(group),
            view_count,
            newest: group.newest,
            outlets,
        });
    }

    let coverage_scale = robust_scale(&stories.iter().map(|s| s.coverage).collect::<Vec<_>>());
    let engagement_scale = robust_scale(&stories.iter().map(|s| s.engagement).collect::<Vec<_>>());
    let spread_scale = robust_scale(
        &stories
            .iter()
            .map(|s| s.spread_velocity)
            .collect::<Vec<_>>(),
    );
    // Dividing by the sum of the weights keeps the result a weighted average, so the score
    // stays inside 0..1 even for weights a config chose to sum above one.
    let weight_sum =
        (weights.coverage + weights.engagement + weights.freshness + weights.spread_velocity)
            .max(f64::MIN_POSITIVE);

    for story in &mut stories {
        story.coverage_norm = normalize(story.coverage, coverage_scale);
        story.engagement_norm = normalize(story.engagement, engagement_scale);
        story.spread_velocity_norm = normalize(story.spread_velocity, spread_scale);
        story.score = (weights.coverage * story.coverage_norm
            + weights.engagement * story.engagement_norm
            + weights.freshness * story.freshness
            + weights.spread_velocity * story.spread_velocity_norm)
            / weight_sum;
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
                    // A cited item with no named origin is a `Repost`, which keeps the half
                    // weight these tests were written against. Naming the origin is what makes
                    // it a `Citation`, and the provenance tests build that case explicitly.
                    cited_outlet: None,
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

    /// A story's description is the freshest body any of its items carried: a feed that ships
    /// no summary must not blank out the one a channel's post of the same story does carry.
    #[test]
    fn the_description_comes_from_the_newest_item_that_has_one() {
        let mut group = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 3, None, false),
            (2, 20, "Bakıda yollar bağlıdır", 1, None, false),
        ]);
        group.items[0].description = Some("Köhnə təsvir".into());
        group.items[1].description = None;
        let ranked = rank(
            &[group.clone()],
            &[],
            &Baselines::empty(),
            Window::Day,
            &Weights::default(),
            NOW,
        );
        assert_eq!(
            ranked[0].description.as_deref(),
            Some("Köhnə təsvir"),
            "the only body in the story is used, however old its item is"
        );

        group.items[1].description = Some("Yeni təsvir".into());
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Day,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].description.as_deref(), Some("Yeni təsvir"));
    }

    /// A Telegram post can be thousands of characters. The ranked story carries a lede, not the
    /// whole post, and the cut lands on a character boundary so a multi-byte glyph cannot split.
    #[test]
    fn a_very_long_body_is_cut_short_on_a_character_boundary() {
        let mut group = story(&[(1, 10, "Uzun xəbər budur", 1, None, false)]);
        group.items[0].description = Some("ə".repeat(600));
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Day,
            &Weights::default(),
            NOW,
        );
        let description = ranked[0].description.as_deref().unwrap();
        assert_eq!(
            description.chars().count(),
            281,
            "280 characters and the ellipsis"
        );
        assert!(description.ends_with('…'));
    }

    #[test]
    fn coverage_counts_distinct_outlets_not_distinct_sources() {
        // One outlet publishing both an RSS feed and a Telegram channel casts one vote.
        let group = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, false),
            (1, 11, "Bakıda yollar bağlıdır", 1, Some(100), false),
        ]);
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 1.0);
        assert_eq!(ranked[0].outlets.len(), 1);
    }

    #[test]
    fn a_cited_repost_is_worth_half_and_a_real_report_is_worth_one() {
        let cited_only = story(&[(1, 10, "Bakıda yollar bağlıdır", 1, None, true)]);
        let ranked = rank(
            &[cited_only],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 0.5);

        let mixed = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, true),
            (1, 11, "Bakıda yollar bağlıdır - Yenilik", 1, None, false),
        ]);
        let ranked = rank(
            &[mixed],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(
            ranked[0].coverage, 1.0,
            "one uncited item makes the outlet count fully"
        );
    }

    #[test]
    fn engagement_is_normalized_per_channel() {
        // Channel 10 carries a breakout post (1000 views/hour) and a routine one (100); channel
        // 20 carries one post at 50. The baselines are supplied rather than derived, because a
        // real one needs twenty rated posts and this test is about the arithmetic: the point is
        // that channel 10's pace is 550 and channel 20 has none of its own, so it falls back to
        // the corpus median of 50 rather than being compared against a channel nobody measured.
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
            &Baselines::from_medians(HashMap::from([(10, 550.0)]), 50.0),
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
        let ranked = rank(
            &[fresh, stale],
            &[],
            &Baselines::empty(),
            Window::Day,
            &Weights::default(),
            NOW,
        );
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
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 3.0);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn a_measured_slope_is_told_apart_from_a_lifetime_estimate() {
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
        let velocity = current_velocity(&samples, 1, NOW - 7200, NOW).unwrap();
        assert!((velocity.per_hour - 600.0).abs() < 1e-6);
        assert_eq!(velocity.basis, VelocityBasis::Measured);
        assert_eq!(velocity.observed_seconds, 3600);
        assert_eq!(velocity.observed_at, NOW);

        // A single sample two hours old: 400 views spread over two hours. The number is an
        // estimate of the lifetime average, and it says so.
        let single = vec![Sample {
            item_id: 2,
            ts: NOW,
            views: 400,
        }];
        let velocity = current_velocity(&single, 2, NOW - 7200, NOW).unwrap();
        assert!((velocity.per_hour - 200.0).abs() < 1e-6);
        assert_eq!(velocity.basis, VelocityBasis::Estimated);

        // A brand-new post is floored at a quarter hour so it cannot divide by zero.
        let fresh = vec![Sample {
            item_id: 3,
            ts: NOW,
            views: 10,
        }];
        let velocity = current_velocity(&fresh, 3, NOW, NOW).unwrap();
        assert!((velocity.per_hour - 40.0).abs() < 1e-6);
        assert_eq!(velocity.basis, VelocityBasis::Estimated);

        // No samples at all is not a rate of zero. It is no observation, and the caller must
        // show nothing rather than a number nothing was measured for.
        assert_eq!(current_velocity(&[], 4, NOW - 7200, NOW), None);
    }

    /// The report this was written for: 0 → 10000 → 10000 views over two hours. The post gained
    /// nothing in the last hour, and the old arithmetic — total gained over the whole history —
    /// called that 5000 views an hour.
    #[test]
    fn no_growth_in_the_recent_interval_is_not_reported_as_the_older_average() {
        let samples = vec![
            Sample {
                item_id: 1,
                ts: NOW - 7200,
                views: 0,
            },
            Sample {
                item_id: 1,
                ts: NOW - 3600,
                views: 10_000,
            },
            Sample {
                item_id: 1,
                ts: NOW,
                views: 10_000,
            },
        ];
        let velocity = current_velocity(&samples, 1, NOW - 7200, NOW).unwrap();
        assert_eq!(velocity.basis, VelocityBasis::Measured);
        assert!(
            velocity.per_hour < 1.0,
            "the last hour gained nothing, but the rate came out {}",
            velocity.per_hour
        );
    }

    /// A source that stopped answering must not keep showing its last number as today's pace.
    #[test]
    fn samples_that_stopped_are_stale_rather_than_current() {
        let samples = vec![
            Sample {
                item_id: 1,
                ts: NOW - 7200,
                views: 100,
            },
            Sample {
                item_id: 1,
                ts: NOW - 3600,
                views: 700,
            },
        ];
        let velocity = current_velocity(&samples, 1, NOW - 7200, NOW).unwrap();
        assert_eq!(
            velocity.basis,
            VelocityBasis::Stale,
            "the newest sample is an hour old; that is history, not pace"
        );
        // The last measurement is still reported, so the screen can show what was seen and when.
        assert!((velocity.per_hour - 600.0).abs() < 1e-6);
        assert_eq!(velocity.observed_at, NOW - 3600);
    }

    /// An estimate is not a measurement: it says what the post averaged since publication, and
    /// the score may not treat it as an observed pace.
    #[test]
    fn an_estimate_is_never_scored_as_engagement() {
        let (group, samples) = posts(&[(1, 10, "Təzə xəbər budur", 1, 5000)]);
        // The fresh sample alone: one observation, so nothing was measured.
        let single: Vec<Sample> = samples.iter().copied().skip(1).take(1).collect();
        let baselines = Baselines::from_medians(HashMap::from([(10, 1000.0)]), 1.0);
        let ranked = rank(
            &[group],
            &single,
            &baselines,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].outlets[0].relative_velocity, None);
        assert_eq!(ranked[0].engagement, 0.0);
        assert_eq!(ranked[0].view_count, 5000);
        assert_eq!(
            ranked[0].outlets[0].velocity.unwrap().basis,
            VelocityBasis::Estimated
        );
    }

    /// A view count that no channel has cannot wrap a story's total into a negative number.
    /// The parse refuses absurd counts, and the sum saturates even if one reaches the database.
    #[test]
    fn a_wild_view_count_saturates_the_story_total_instead_of_wrapping_it() {
        let (group, samples) = posts(&[
            (1, 10, "Çox böyük rəqəm xəbəri budur", 1, i64::MAX),
            (2, 20, "Çox böyük rəqəm xəbəri budur", 1, i64::MAX),
        ]);
        let ranked = rank(
            &[group],
            &samples,
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert!(
            ranked[0].view_count > 0,
            "the total saturates at the maximum instead of wrapping"
        );
        assert!(ranked[0].score.is_finite() && ranked[0].score <= 1.0);
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
            &Baselines::empty(),
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
        // views to another. Each item gained its own channel's median pace over the hour, so a
        // correctly attributed rate is exactly 1x that channel and every story's engagement is
        // exactly 1.0.
        let slipped = story(&[(1, 10, "Böyük yol qəzası budur", 2, Some(600), false)]);
        let single = story(&[(2, 20, "Kiçik kanal xəbəri budur", 2, Some(60), false)]);
        let risen = story(&[(3, 30, "Orta xəbər belə gəldi", 2, Some(150), false)]);
        let (slipped_id, single_id, risen_id) = (
            slipped.items[0].item_id,
            single.items[0].item_id,
            risen.items[0].item_id,
        );

        let samples = vec![
            // One item's rows newest first, and the others' scattered between them.
            Sample {
                item_id: slipped_id,
                ts: NOW,
                views: 600,
            },
            Sample {
                item_id: single_id,
                ts: NOW - 3600,
                views: 0,
            },
            Sample {
                item_id: slipped_id,
                ts: NOW - 3600,
                views: 0,
            },
            Sample {
                item_id: risen_id,
                ts: NOW,
                views: 150,
            },
            Sample {
                item_id: single_id,
                ts: NOW,
                views: 60,
            },
            Sample {
                item_id: risen_id,
                ts: NOW - 3600,
                views: 0,
            },
        ];

        let ranked = rank(
            &[slipped, single, risen],
            &samples,
            &Baselines::from_medians(HashMap::from([(10, 600.0), (20, 60.0), (30, 150.0)]), 1.0),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let story = |title: &str| ranked.iter().find(|s| s.title == title).unwrap();

        let slipped = story("Böyük yol qəzası budur");
        assert_eq!(
            slipped.outlets[0]
                .velocity
                .map(|velocity| velocity.per_hour),
            Some(600.0),
            "the slope of its own two samples, not another story's"
        );
        assert_eq!(slipped.view_count, 600);
        assert_eq!(slipped.engagement, 1.0);

        let single = story("Kiçik kanal xəbəri budur");
        assert_eq!(
            single.outlets[0].velocity.map(|velocity| velocity.per_hour),
            Some(60.0),
            "its own samples again, and its own channel's median is the divisor"
        );
        assert_eq!(
            single.outlets[0].velocity.map(|velocity| velocity.basis),
            Some(VelocityBasis::Measured)
        );
        assert_eq!(single.view_count, 60);
        assert_eq!(single.engagement, 1.0);

        let risen = story("Orta xəbər belə gəldi");
        assert_eq!(
            risen.outlets[0].velocity.map(|velocity| velocity.per_hour),
            Some(150.0),
            "and the third story's own samples"
        );
        assert_eq!(risen.view_count, 150);
        assert_eq!(risen.engagement, 1.0);
    }

    /// One hand-built Telegram post: (outlet_id, source_id, title, age_hours, views).
    type PostRow<'a> = (i64, i64, &'a str, i64, i64);

    /// A group built from Telegram posts through the real clusterer, with the samples each post
    /// needs to have a rate. The samples span one hour from zero, so a post's `views` value in
    /// these tests *is* its views per hour, which keeps the arithmetic in the assertions readable.
    fn posts(rows: &[PostRow<'_>]) -> (Group, Vec<Sample>) {
        let items: Vec<ItemRow> = rows
            .iter()
            .enumerate()
            .map(
                |(index, (outlet_id, source_id, title, age_hours, views))| ItemRow {
                    item_id: NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
                    source_id: *source_id,
                    outlet_id: *outlet_id,
                    outlet: format!("Outlet{outlet_id}"),
                    kind: SourceKind::Telegram,
                    title: title.to_string(),
                    description: None,
                    url: format!("https://example.az/p{index}"),
                    published_at: NOW - age_hours * 3600,
                    views: Some(*views),
                    cited: false,
                    cited_outlet: None,
                    is_backfill: false,
                },
            )
            .collect();
        let samples: Vec<Sample> = items
            .iter()
            .flat_map(|item| {
                [
                    Sample {
                        item_id: item.item_id,
                        ts: NOW - 3600,
                        views: 0,
                    },
                    Sample {
                        item_id: item.item_id,
                        ts: NOW,
                        views: item.views.unwrap_or(0),
                    },
                ]
            })
            .collect();
        (Clusterer::new(0.45).group_items(&items).remove(0), samples)
    }

    /// A group whose rows carry explicit provenance, so `cited_outlet` can be set.
    /// (outlet_id, source_id, title, age_hours, cited, cited_outlet).
    type ProvenanceRow<'a> = (i64, i64, &'a str, i64, bool, Option<&'a str>);

    fn story_with(rows: &[ProvenanceRow<'_>]) -> Group {
        let items: Vec<ItemRow> = rows
            .iter()
            .enumerate()
            .map(
                |(index, (outlet_id, source_id, title, age_hours, cited, cited_outlet))| ItemRow {
                    item_id: NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
                    source_id: *source_id,
                    outlet_id: *outlet_id,
                    outlet: format!("Outlet{outlet_id}"),
                    kind: SourceKind::Rss,
                    title: title.to_string(),
                    description: None,
                    url: format!("https://example.az/c{index}"),
                    published_at: NOW - age_hours * 3600,
                    views: None,
                    cited: *cited,
                    cited_outlet: cited_outlet.map(str::to_string),
                    is_backfill: false,
                },
            )
            .collect();
        Clusterer::new(0.45).group_items(&items).remove(0)
    }

    #[test]
    fn one_outlet_posting_five_times_does_not_multiply_its_engagement() {
        // Five posts by one outlet about one story, each gaining 1000 views an hour — the
        // channel's own pace. The outlet is still one outlet, so it contributes its best post.
        let (group, samples) = posts(&[
            (1, 10, "Bakıda yollar bağlıdır", 5, 1000),
            (1, 10, "Bakıda yollar bağlıdır", 4, 1000),
            (1, 10, "Bakıda yollar bağlıdır", 3, 1000),
            (1, 10, "Bakıda yollar bağlıdır", 2, 1000),
            (1, 10, "Bakıda yollar bağlıdır", 1, 1000),
        ]);
        assert_eq!(group.items.len(), 5, "all five posts are one story");
        let ranked = rank(
            &[group],
            &samples,
            &Baselines::from_medians(HashMap::new(), 1000.0),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].outlets.len(), 1, "one outlet, not five");
        assert_eq!(
            ranked[0].engagement, 1.0,
            "five routine posts are one outlet at its own pace, not five times it"
        );
    }

    #[test]
    fn a_quiet_channels_breakout_outranks_a_busy_channels_routine() {
        // The same absolute velocity means opposite things on two channels: 800 views an hour is
        // four times channel 20's normal pace and a fifth of channel 10's.
        let (big, mut samples) = posts(&[(1, 10, "Böyük kanal xəbəri budur", 1, 5000)]);
        let (small, small_samples) = posts(&[(2, 20, "Kiçik kanal xəbəri budur", 1, 800)]);
        samples.extend(small_samples);
        let baselines = Baselines::from_medians(HashMap::from([(10, 5000.0), (20, 200.0)]), 1.0);
        let ranked = rank(
            &[big, small],
            &samples,
            &baselines,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let of = |title: &str| ranked.iter().find(|s| s.title.contains(title)).unwrap();
        assert_eq!(of("Böyük").engagement, 1.0, "5000 against a 5000 pace");
        assert_eq!(of("Kiçik").engagement, 4.0, "800 against a 200 pace");
        assert!(
            of("Kiçik").score > of("Böyük").score,
            "the quiet channel's breakout outranks the busy channel's routine post"
        );
    }

    #[test]
    fn a_viral_post_is_capped_at_ten_times_its_channel() {
        // 28.8 times the channel's pace is a measurement artefact as often as a real sensation,
        // and an uncapped ratio would let one post decide the whole ranking.
        let (group, samples) = posts(&[(1, 10, "Viral xəbər budur", 1, 28_800)]);
        let ranked = rank(
            &[group],
            &samples,
            &Baselines::from_medians(HashMap::new(), 1000.0),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(
            ranked[0].outlets[0].relative_velocity,
            Some(MAX_RELATIVE_VELOCITY)
        );
        assert_eq!(ranked[0].engagement, MAX_RELATIVE_VELOCITY);
        assert_eq!(
            ranked[0].outlets[0]
                .velocity
                .map(|velocity| velocity.per_hour),
            Some(28_800.0),
            "the raw measurement is kept beside the capped ratio"
        );
    }

    #[test]
    fn one_outlier_does_not_flatten_the_rest_of_the_window() {
        // Twenty ordinary stories carried by two outlets each, and one carried by a hundred.
        // Against the window maximum every ordinary story would score 2/100 = 0.02; against the
        // 95th percentile — which the outlier cannot move, being one value of twenty-one — they
        // keep their full weight.
        let mut groups = Vec::new();
        for index in 0..20 {
            groups.push(story(&[
                (1, 10 + index, &format!("Adi xəbər {index}"), 1, None, false),
                (
                    2,
                    100 + index,
                    &format!("Adi xəbər {index}"),
                    1,
                    None,
                    false,
                ),
            ]));
        }
        let mut outlier = Vec::new();
        for index in 0..100 {
            outlier.push((index + 1, 1000 + index, "Böyük xəbər", 1, None, false));
        }
        groups.push(story(&outlier));

        let ranked = rank(
            &groups,
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let ordinary = ranked.iter().find(|s| s.coverage == 2.0).unwrap();
        assert!(
            ordinary.coverage_norm > 0.99,
            "an outlier must not flatten the ordinary stories: {}",
            ordinary.coverage_norm
        );
        let leader = ranked.iter().find(|s| s.coverage == 100.0).unwrap();
        assert_eq!(
            leader.coverage_norm, 1.0,
            "the outlier still leads, it just cannot distort"
        );
    }

    #[test]
    fn four_outlets_republishing_one_agency_are_one_independent_origin() {
        // APA reports it; three outlets repeat it and say whose work it is. Four outlets carry
        // the story and exactly one of them did the reporting.
        let group = story_with(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, false, None),
            (2, 20, "Bakıda yollar bağlıdır", 1, true, Some("apa")),
            (3, 30, "Bakıda yollar bağlıdır", 1, true, Some("apa")),
            (4, 40, "Bakıda yollar bağlıdır", 1, true, Some("apa")),
        ]);
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].spread, 4, "four outlets carry it");
        assert_eq!(
            ranked[0].coverage, 1.0,
            "one of them reported it; the other three confirm nothing"
        );

        // A repeat that names nobody keeps half weight: nobody can prove whose work it is.
        let unattributed = story_with(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, false, None),
            (2, 20, "Bakıda yollar bağlıdır", 1, true, None),
        ]);
        let ranked = rank(
            &[unattributed],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].coverage, 1.5);
    }

    #[test]
    fn a_duplicate_repost_does_not_refresh_the_story() {
        // One outlet reported six hours ago and has repeated the headline every hour since.
        // Nothing has developed, so the story keeps decaying from the original report.
        let (repeated, _) = posts(&[
            (1, 10, "Bakıda yollar bağlıdır", 6, 100),
            (1, 10, "Bakıda yollar bağlıdır", 1, 100),
        ]);
        let ranked = rank(
            &[repeated],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].started_at, NOW - 6 * 3600);
        assert_eq!(ranked[0].newest, NOW - 3600, "it did repost an hour ago");
        assert_eq!(
            ranked[0].updated_at,
            NOW - 6 * 3600,
            "a repeat by an outlet that already carried it is not a development"
        );

        // A second outlet picking the story up is a development, and moves freshness forward.
        let (developing, _) = posts(&[
            (1, 10, "Bakıda yollar bağlıdır", 6, 100),
            (1, 10, "Bakıda yollar bağlıdır", 1, 100),
            (2, 20, "Bakıda yollar bağlıdır", 2, 100),
        ]);
        let ranked = rank(
            &[developing],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].updated_at, NOW - 2 * 3600);
    }

    #[test]
    fn spread_velocity_counts_outlets_arriving_in_the_recent_sub_window() {
        // The hour window looks back twenty minutes for arrivals: an outlet already carrying the
        // story for two hours is the story sitting still, not the story spreading.
        let (group, _) = posts(&[
            (1, 10, "Bakıda yollar bağlıdır", 2, 100),
            (2, 20, "Bakıda yollar bağlıdır", 1, 100),
            (3, 30, "Bakıda yollar bağlıdır", 0, 100),
        ]);
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(ranked[0].spread, 3);
        assert_eq!(
            ranked[0].spread_velocity, 1.0,
            "only the outlet that arrived just now counts"
        );
    }

    #[test]
    fn the_score_is_the_weighted_sum_of_its_four_parts() {
        let (big, mut samples) = posts(&[(1, 10, "Birinci xəbər budur", 1, 4000)]);
        let (small, small_samples) = posts(&[(2, 20, "İkinci xəbər budur", 1, 1000)]);
        samples.extend(small_samples);
        let baselines = Baselines::from_medians(HashMap::from([(10, 1000.0), (20, 1000.0)]), 1.0);
        let ranked = rank(
            &[big, small],
            &samples,
            &baselines,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let weights = Weights::default();
        for story in &ranked {
            let expected = weights.coverage * story.coverage_norm
                + weights.engagement * story.engagement_norm
                + weights.freshness * story.freshness
                + weights.spread_velocity * story.spread_velocity_norm;
            assert!(
                (story.score - expected).abs() < 1e-12,
                "score {} is not 0.35/0.35/0.20/0.10 over {} {} {} {}",
                story.score,
                story.coverage_norm,
                story.engagement_norm,
                story.freshness,
                story.spread_velocity_norm
            );
            assert!(
                (0.0..=1.0).contains(&story.score),
                "every component is 0..1, so the score is too: {}",
                story.score
            );
            // The timeline a story claims must be one: it starts at or before the last
            // development, and the last development is at or before its newest item.
            assert!(
                story.started_at <= story.updated_at,
                "a story cannot be updated before it started: {} > {}",
                story.started_at,
                story.updated_at
            );
            assert!(
                story.updated_at <= story.newest,
                "a development cannot postdate the newest item: {} > {}",
                story.updated_at,
                story.newest
            );
        }
    }

    #[test]
    fn a_channel_needs_twenty_rated_posts_before_its_own_median_is_trusted() {
        // Nineteen posts describe a sample, not a channel: one lucky post would set the bar for
        // everything after it, so the corpus median is used until the twentieth arrives.
        let build = |count: usize| {
            let items: Vec<ItemRow> = (0..count)
                .map(|index| ItemRow {
                    item_id: NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
                    source_id: 10,
                    outlet_id: 1,
                    outlet: "Outlet1".into(),
                    kind: SourceKind::Telegram,
                    title: format!("Xəbər {index}"),
                    description: None,
                    url: format!("https://example.az/b{index}"),
                    published_at: NOW - 3600,
                    views: Some(100),
                    cited: false,
                    cited_outlet: None,
                    is_backfill: false,
                })
                .collect();
            let samples: Vec<Sample> = items
                .iter()
                .flat_map(|item| {
                    [
                        Sample {
                            item_id: item.item_id,
                            ts: NOW - 3600,
                            views: 0,
                        },
                        Sample {
                            item_id: item.item_id,
                            ts: NOW,
                            views: 100,
                        },
                    ]
                })
                .collect();
            (items, samples)
        };

        let (items, samples) = build(19);
        let thin = Baselines::from_items(&items, &samples, NOW);
        assert!(thin.is_fallback(10), "nineteen posts is not a baseline");
        assert_eq!(thin.of(10), 100.0, "the corpus median covers it");

        let (items, samples) = build(20);
        let earned = Baselines::from_items(&items, &samples, NOW);
        assert!(!earned.is_fallback(10), "twenty posts is a baseline");
        assert_eq!(earned.of(10), 100.0);
    }

    #[test]
    fn missing_samples_and_baselines_never_panic_and_never_exceed_one() {
        // The state of a fresh database: items with no samples, no baselines, one story. Every
        // component must still be a number in 0..1.
        let (group, _) = posts(&[(1, 10, "Təzə xəbər budur", 1, 100)]);
        let ranked = rank(
            &[group],
            &[],
            &Baselines::empty(),
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        let story = &ranked[0];
        assert_eq!(story.engagement, 0.0, "no samples, no velocity");
        assert_eq!(story.coverage_norm, 1.0);
        for value in [
            story.score,
            story.coverage_norm,
            story.engagement_norm,
            story.freshness,
            story.spread_velocity_norm,
        ] {
            assert!((0.0..=1.0).contains(&value), "out of range: {value}");
        }
    }

    /// One outlet with two channels of different size: the contribution is the post that ran
    /// furthest above its own channel's pace, not the one with the biggest raw number.
    #[test]
    fn an_outlet_contributes_its_fastest_relative_post_not_its_absolute_one() {
        // Both posts gained 1000 views in the hour these fixtures measure. Channel 10 normally
        // runs at 1000/h, channel 20 at 100/h, so the same raw number means 1x on one and 10x
        // on the other.
        let (group, samples) = posts(&[
            (1, 10, "İki kanalı olan xəbər budur", 1, 1000),
            (1, 20, "İki kanalı olan xəbər budur", 1, 1000),
        ]);
        let baselines = Baselines::from_medians(HashMap::from([(10, 1000.0), (20, 100.0)]), 1.0);
        let ranked = rank(
            &[group],
            &samples,
            &baselines,
            Window::Hour,
            &Weights::default(),
            NOW,
        );
        assert_eq!(
            ranked[0].outlets.len(),
            1,
            "one outlet, however many channels"
        );
        assert_eq!(ranked[0].outlets[0].relative_velocity, Some(10.0));
        assert_eq!(ranked[0].engagement, 10.0);
    }

    /// The percentile only becomes a percentile at twenty stories: below that every nearest
    /// rank above the 95th is the largest value itself.
    #[test]
    fn the_scale_is_the_maximum_below_twenty_stories_and_a_percentile_above() {
        let values = |count: usize| -> Vec<f64> {
            let mut values: Vec<f64> = (0..count).map(|n| n as f64 + 1.0).collect();
            // One outlier at the top, which is what the percentile exists to discount.
            values[count - 1] = 1000.0;
            values
        };
        for count in [1, 5, 19] {
            let values = values(count);
            assert_eq!(
                robust_scale(&values),
                values.iter().copied().fold(0.0, f64::max),
                "{count} stories: the maximum is the only honest scale"
            );
        }
        for count in [20, 21, 40, 100] {
            let values = values(count);
            let scale = robust_scale(&values);
            assert!(
                scale < 1000.0,
                "{count} stories: the outlier must not set it"
            );
            assert_eq!(scale, percentile(&values, 95.0), "{count} stories");
        }
        assert_eq!(robust_scale(&[]), 0.0);
    }

    /// The normalizer decides who shares the top, and that is the whole of its effect on the
    /// order: dividing by one positive number cannot reorder anything else. Measured here,
    /// because the choice was made on this evidence rather than on the shape of the formula.
    ///
    /// Under the maximum only the leader normalises to 1.00. Under the 95th percentile everything
    /// at or above the percentile clamps to 1.00, so the top one or two stories of a window tie
    /// and the ranking falls through to its recency tie-break. That tie is bounded — no more
    /// stories can exceed the percentile than the percentile allows — while the maximum would let
    /// one outlier push every other story toward zero. Both properties are true, and this test
    /// keeps both visible instead of hiding the trade-off behind the division.
    #[test]
    fn the_normalizer_decides_only_who_shares_the_top() {
        let values: Vec<f64> = (1..=25).map(|n| n as f64).collect();
        let max = percentile(&values, 100.0);
        let p95 = robust_scale(&values);
        assert_eq!((p95, max), (24.0, 25.0));

        assert_eq!(normalize(values[24], max), 1.0);
        assert_eq!(normalize(values[23], max), 0.96);
        assert_eq!(normalize(values[23], p95), 1.0);
        assert_eq!(normalize(values[24], p95), 1.0);
        assert_eq!(normalize(values[22], p95), 23.0 / 24.0);

        // Below the percentile the two scales order the stories identically.
        let order = |scale: f64| {
            let mut scored: Vec<(usize, f64)> = values[..23]
                .iter()
                .enumerate()
                .map(|(index, value)| (index, normalize(*value, scale)))
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            scored
                .into_iter()
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(max), order(p95));
    }

    #[test]
    fn percentile_takes_the_nearest_rank_and_robust_scale_guards_its_edges() {
        let values: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(&values, 95.0), 95.0);
        assert_eq!(percentile(&[3.0], 95.0), 3.0);
        assert_eq!(percentile(&[], 95.0), 0.0);

        // Fewer than five stories: the maximum, because a percentile of three values is the
        // maximum wearing a hat.
        assert_eq!(robust_scale(&[1.0, 2.0, 3.0]), 3.0);
        // Enough stories: the 95th percentile, which the single outlier cannot move.
        let mut many = vec![1.0; 40];
        many.push(1_000.0);
        assert_eq!(robust_scale(&many), 1.0);
        // No scale at all, and a caller that must not divide by it.
        assert_eq!(robust_scale(&[]), 0.0);
        assert_eq!(robust_scale(&[0.0, 0.0]), 0.0);
        assert_eq!(normalize(5.0, 0.0), 0.0);
        assert_eq!(normalize(5.0, 10.0), 0.5);
        assert_eq!(normalize(50.0, 10.0), 1.0, "clamped, never above one");
        assert_eq!(
            normalize(f64::NAN, 10.0),
            0.0,
            "a NaN must not reach the sort"
        );
    }
}
