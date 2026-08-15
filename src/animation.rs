use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use serde::Deserialize;

use crate::config::AnimationConfig;

/// Easing functions supported by the versioned animation contract.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Easing {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    Step,
}

impl std::str::FromStr for Easing {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "linear" => Ok(Self::Linear),
            "easein" | "ease-in" => Ok(Self::EaseIn),
            "easeout" | "ease-out" => Ok(Self::EaseOut),
            "easeinout" | "ease-in-out" => Ok(Self::EaseInOut),
            "step" => Ok(Self::Step),
            _ => Err(()),
        }
    }
}

impl Easing {
    fn apply(self, progress: u16) -> u16 {
        let p = u32::from(progress);
        let eased = match self {
            Self::Linear => p,
            Self::EaseIn => p * p / 1000,
            Self::EaseOut => 1000 - (1000 - p) * (1000 - p) / 1000,
            Self::EaseInOut => {
                if p < 500 {
                    2 * p * p / 1000
                } else {
                    1000 - 2 * (1000 - p) * (1000 - p) / 1000
                }
            }
            Self::Step => u32::from(progress >= 1000) * 1000,
        };
        eased.min(1000) as u16
    }
}

/// Controls whether repeated animation cycles play forwards or alternate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AnimationDirection {
    #[default]
    Normal,
    Alternate,
}

impl std::str::FromStr for AnimationDirection {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "normal" => Ok(Self::Normal),
            "alternate" => Ok(Self::Alternate),
            _ => Err(()),
        }
    }
}

/// Controls the value retained after a transition completes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FillMode {
    #[default]
    None,
    Forwards,
}

impl std::str::FromStr for FillMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "forwards" => Ok(Self::Forwards),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationSettings {
    pub enabled: bool,
    pub duration: Option<Duration>,
    pub delay: Option<Duration>,
    pub easing: Option<Easing>,
    pub repeat: Option<u16>,
    pub direction: Option<AnimationDirection>,
    pub fill: Option<FillMode>,
}

impl Default for AnimationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            duration: None,
            delay: None,
            easing: None,
            repeat: None,
            direction: None,
            fill: None,
        }
    }
}

impl AnimationSettings {
    pub fn from_settings(settings: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        if let Some(value) = settings.get("animation") {
            parsed.enabled = value
                .parse::<bool>()
                .map_err(|_| format!("widget animation must be true or false, got {value:?}"))?;
        }
        parsed.duration = parse_duration(settings, "animation_duration_ms")?;
        parsed.delay = parse_duration(settings, "animation_delay_ms")?;
        parsed.easing = parse_enum(settings, "animation_easing")?;
        parsed.repeat = parse_integer(settings, "animation_repeat")?;
        parsed.direction = parse_enum(settings, "animation_direction")?;
        parsed.fill = parse_enum(settings, "animation_fill")?;
        if parsed.repeat.is_some_and(|repeat| repeat > 32) {
            return Err("widget animation_repeat must be between 0 and 32".to_owned());
        }
        Ok(parsed)
    }

    pub fn apply(self, mut spec: AnimationSpec) -> AnimationSpec {
        if let Some(duration) = self.duration {
            spec.duration = duration;
        }
        if let Some(delay) = self.delay {
            spec.delay = delay;
        }
        if let Some(easing) = self.easing {
            spec.easing = easing;
        }
        if let Some(repeat) = self.repeat {
            spec.repeat = repeat;
        }
        if let Some(direction) = self.direction {
            spec.direction = direction;
        }
        if let Some(fill) = self.fill {
            spec.fill = fill;
        }
        spec
    }
}

fn parse_duration(
    settings: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<Duration>, String> {
    let Some(value) = settings.get(key) else {
        return Ok(None);
    };
    let milliseconds = value
        .parse::<u64>()
        .map_err(|_| format!("widget {key} must be an integer, got {value:?}"))?;
    if milliseconds == 0 || milliseconds > 60_000 {
        return Err(format!("widget {key} must be between 1 and 60000"));
    }
    Ok(Some(Duration::from_millis(milliseconds)))
}

fn parse_integer<T>(settings: &BTreeMap<String, String>, key: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
{
    settings
        .get(key)
        .map(|value| {
            value
                .parse::<T>()
                .map_err(|_| format!("widget {key} is invalid: {value:?}"))
        })
        .transpose()
}

fn parse_enum<T>(settings: &BTreeMap<String, String>, key: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
{
    parse_integer(settings, key)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationSpec {
    pub delay: Duration,
    pub duration: Duration,
    pub easing: Easing,
    pub repeat: u16,
    pub direction: AnimationDirection,
    pub fill: FillMode,
}

impl AnimationSpec {
    pub const fn from_config(config: &AnimationConfig) -> Self {
        Self {
            delay: Duration::from_millis(config.delay_ms),
            duration: Duration::from_millis(config.duration_ms),
            easing: config.easing,
            repeat: config.repeat,
            direction: config.direction,
            fill: config.fill,
        }
    }

    pub const fn instant() -> Self {
        Self {
            delay: Duration::ZERO,
            duration: Duration::ZERO,
            easing: Easing::Linear,
            repeat: 0,
            direction: AnimationDirection::Normal,
            fill: FillMode::Forwards,
        }
    }

    fn total_duration(self) -> Option<Duration> {
        self.duration
            .checked_mul(u32::from(self.repeat).saturating_add(1))?
            .checked_add(self.delay)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum AnimationKey {
    Focus,
    Surface(u64),
    Widget(u64),
    Overlay(u64),
    Tabs,
    Panes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationSample {
    pub progress: u16,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationFrame {
    pub focus_progress: u16,
    pub transition_progress: u16,
}

impl AnimationFrame {
    pub const fn complete() -> Self {
        Self {
            focus_progress: 1000,
            transition_progress: 1000,
        }
    }
}

impl AnimationSample {
    pub const COMPLETE: Self = Self {
        progress: 1000,
        active: false,
    };
}

#[derive(Clone, Copy, Debug)]
struct RunningAnimation {
    spec: AnimationSpec,
    started: SystemTime,
    paused_at: Option<SystemTime>,
    paused_elapsed: Duration,
}

/// A bounded retained animation manager.
///
/// The manager has no worker thread and never sleeps. The UI coordinator calls
/// `advance` with its clock, renders the resulting ordinary scene, and asks
/// `next_wakeup` when another frame is needed.
#[derive(Clone, Debug)]
pub struct AnimationManager {
    config: AnimationConfig,
    running: std::collections::BTreeMap<AnimationKey, RunningAnimation>,
    samples: std::collections::BTreeMap<AnimationKey, AnimationSample>,
    paused: bool,
}

impl AnimationManager {
    pub fn new(config: AnimationConfig) -> Self {
        Self {
            config,
            running: std::collections::BTreeMap::new(),
            samples: std::collections::BTreeMap::new(),
            paused: false,
        }
    }

    pub fn config(&self) -> &AnimationConfig {
        &self.config
    }

    pub fn is_active(&self) -> bool {
        !self.running.is_empty()
    }

    pub fn sample(&self, key: AnimationKey) -> AnimationSample {
        self.samples
            .get(&key)
            .copied()
            .unwrap_or(AnimationSample::COMPLETE)
    }

    pub fn start(&mut self, key: AnimationKey, spec: AnimationSpec, now: SystemTime) -> bool {
        if !self.config.enabled || self.config.reduced_motion || self.paused {
            self.running.remove(&key);
            self.samples.insert(key, AnimationSample::COMPLETE);
            return false;
        }
        if !self.running.contains_key(&key) && self.running.len() >= self.config.max_concurrent {
            return false;
        }
        let spec = AnimationSpec {
            duration: spec.duration.max(Duration::from_millis(1)),
            ..spec
        };
        self.running.insert(
            key,
            RunningAnimation {
                spec,
                started: now,
                paused_at: None,
                paused_elapsed: Duration::ZERO,
            },
        );
        self.samples.insert(
            key,
            AnimationSample {
                progress: 0,
                active: true,
            },
        );
        true
    }

    pub fn cancel(&mut self, key: AnimationKey) {
        self.running.remove(&key);
        self.samples.remove(&key);
    }

    pub fn pause(&mut self, now: SystemTime) {
        if self.paused {
            return;
        }
        self.paused = true;
        for animation in self.running.values_mut() {
            animation.paused_at = Some(now);
        }
    }

    pub fn resume(&mut self, now: SystemTime) {
        if !self.paused {
            return;
        }
        self.paused = false;
        for animation in self.running.values_mut() {
            if let Some(paused_at) = animation.paused_at.take() {
                animation.paused_elapsed = animation
                    .paused_elapsed
                    .saturating_add(now.duration_since(paused_at).unwrap_or(Duration::ZERO));
            }
        }
    }

    pub fn advance(&mut self, now: SystemTime) -> bool {
        if self.paused {
            return false;
        }
        let mut changed = false;
        let mut completed = Vec::new();
        for (&key, animation) in &self.running {
            let elapsed = now
                .duration_since(animation.started)
                .unwrap_or(Duration::ZERO)
                .saturating_sub(animation.paused_elapsed);
            let sample = sample_at(animation.spec, elapsed);
            if self.samples.get(&key).copied() != Some(sample) {
                self.samples.insert(key, sample);
                changed = true;
            }
            if !sample.active {
                completed.push((key, animation.spec.fill == FillMode::None));
            }
        }
        for (key, remove_sample) in completed {
            self.running.remove(&key);
            if remove_sample {
                self.samples.remove(&key);
            }
        }
        changed
    }

    pub fn next_wakeup(&self, now: SystemTime) -> Option<Duration> {
        if self.paused {
            return None;
        }
        self.running
            .values()
            .filter_map(|animation| {
                let elapsed = now
                    .duration_since(animation.started)
                    .unwrap_or(Duration::ZERO)
                    .saturating_sub(animation.paused_elapsed);
                let total = animation.spec.total_duration()?;
                Some(total.saturating_sub(elapsed).min(Duration::from_millis(16)))
            })
            .min()
    }
}

fn sample_at(spec: AnimationSpec, elapsed: Duration) -> AnimationSample {
    if elapsed < spec.delay {
        return AnimationSample {
            progress: 0,
            active: true,
        };
    }
    let elapsed = elapsed.saturating_sub(spec.delay);
    let duration = spec.duration.max(Duration::from_millis(1));
    let cycle = elapsed.as_millis() / duration.as_millis();
    let cycles = u128::from(spec.repeat) + 1;
    if cycle >= cycles {
        return AnimationSample {
            progress: if spec.fill == FillMode::Forwards {
                1000
            } else {
                0
            },
            active: false,
        };
    }
    let remainder = elapsed.as_millis() % duration.as_millis();
    let mut progress = ((remainder * 1000) / duration.as_millis()) as u16;
    if spec.direction == AnimationDirection::Alternate && cycle % 2 == 1 {
        progress = 1000 - progress;
    }
    AnimationSample {
        progress: spec.easing.apply(progress),
        active: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AnimationConfig {
        AnimationConfig {
            enabled: true,
            reduced_motion: false,
            duration_ms: 100,
            delay_ms: 10,
            easing: Easing::Linear,
            repeat: 1,
            direction: AnimationDirection::Alternate,
            fill: FillMode::Forwards,
            max_concurrent: 2,
        }
    }

    #[test]
    fn parses_per_widget_overrides_and_rejects_invalid_values() {
        let settings = BTreeMap::from([
            ("animation".to_owned(), "true".to_owned()),
            ("animation_duration_ms".to_owned(), "240".to_owned()),
            ("animation_easing".to_owned(), "ease-out".to_owned()),
            ("animation_repeat".to_owned(), "2".to_owned()),
        ]);
        let parsed = AnimationSettings::from_settings(&settings).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.duration, Some(Duration::from_millis(240)));
        assert_eq!(parsed.easing, Some(Easing::EaseOut));
        assert_eq!(parsed.repeat, Some(2));

        let invalid = BTreeMap::from([("animation_easing".to_owned(), "unknown".to_owned())]);
        assert!(AnimationSettings::from_settings(&invalid).is_err());
    }

    #[test]
    fn samples_delay_progress_repeat_and_completion_deterministically() {
        let start = SystemTime::UNIX_EPOCH;
        let mut manager = AnimationManager::new(config());
        assert!(manager.start(
            AnimationKey::Focus,
            AnimationSpec::from_config(&config()),
            start
        ));
        assert_eq!(manager.sample(AnimationKey::Focus).progress, 0);
        manager.advance(start + Duration::from_millis(60));
        assert_eq!(manager.sample(AnimationKey::Focus).progress, 500);
        manager.advance(start + Duration::from_millis(160));
        assert_eq!(manager.sample(AnimationKey::Focus).progress, 500);
        manager.advance(start + Duration::from_millis(220));
        assert!(!manager.is_active());
        assert_eq!(manager.sample(AnimationKey::Focus).progress, 1000);
    }

    #[test]
    fn reduced_motion_and_budget_are_enforced() {
        let mut reduced = config();
        reduced.reduced_motion = true;
        let mut manager = AnimationManager::new(reduced);
        assert!(!manager.start(
            AnimationKey::Focus,
            AnimationSpec::instant(),
            SystemTime::UNIX_EPOCH
        ));
        assert_eq!(
            manager.sample(AnimationKey::Focus),
            AnimationSample::COMPLETE
        );

        let mut manager = AnimationManager::new(config());
        assert!(manager.start(
            AnimationKey::Focus,
            AnimationSpec::from_config(&config()),
            SystemTime::UNIX_EPOCH
        ));
        assert!(manager.start(
            AnimationKey::Tabs,
            AnimationSpec::from_config(&config()),
            SystemTime::UNIX_EPOCH
        ));
        assert!(!manager.start(
            AnimationKey::Panes,
            AnimationSpec::from_config(&config()),
            SystemTime::UNIX_EPOCH
        ));
    }

    #[test]
    fn pause_and_resume_preserve_elapsed_time() {
        let start = SystemTime::UNIX_EPOCH;
        let mut manager = AnimationManager::new(config());
        manager.start(
            AnimationKey::Focus,
            AnimationSpec::from_config(&config()),
            start,
        );
        manager.advance(start + Duration::from_millis(60));
        manager.pause(start + Duration::from_millis(60));
        manager.resume(start + Duration::from_millis(200));
        manager.advance(start + Duration::from_millis(210));
        assert_eq!(manager.sample(AnimationKey::Focus).progress, 600);
    }
}
