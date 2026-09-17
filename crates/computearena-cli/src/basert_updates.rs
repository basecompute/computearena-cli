//! What ComputeArena says about the BaseRT it found.
//!
//! Two things are worth a sentence before a benchmark is spent on them: a
//! newer BaseRT release exists, or the installed harness predates the
//! benchmark protocol current reports use. The second needs no network, because
//! the harness says what it supports. Neither ever stops a run: an older BaseRT
//! keeps working exactly as before, and its report records what was used.

use crate::adapters::basert::{supports_headline_capacity, supports_isolated_workloads};
use crate::reports::Paths;
use crate::runtimes::{Source, BASERT_INSTALL_SCRIPT, BASERT_RELEASES};
use crate::ui::TerminalUi;
use crate::updates::{LatestRelease, ReleaseCheck};
use semver::Version;
use serde_json::Value;

/// The first BaseRT release whose harness runs the headline-first protocol.
const HEADLINE_FIRST_SINCE: Version = Version::new(0, 2, 5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// PP512 and TG128 first, in a freshly loaded model with a 4K reservation.
    HeadlineFirst,
    /// One context per workload, without the headline order.
    IsolatedWorkloads,
    /// One context shared by the whole sweep; signed as not comparable.
    SharedContext,
}

fn protocol(descriptor: &Value) -> Protocol {
    // A harness advertising a capacity protocol this client cannot read is
    // newer than the client, not older: the run itself reports that.
    if supports_headline_capacity(descriptor).unwrap_or(true) {
        Protocol::HeadlineFirst
    } else if supports_isolated_workloads(descriptor) {
        Protocol::IsolatedWorkloads
    } else {
        Protocol::SharedContext
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Advice {
    /// Stands on its own: a status line, a plan row.
    pub(crate) summary: String,
    /// What an older protocol means for the report; empty for a plain update.
    pub(crate) consequence: Option<String>,
    /// What to do about it.
    pub(crate) action: String,
    /// Whether `computearena basert install` is that action.
    pub(crate) installable: bool,
    /// The older protocol in a few words, for a benchmark plan.
    pub(crate) plan_note: Option<&'static str>,
}

/// What to say about this BaseRT, if anything. `latest` is the newest release
/// when that is known; an older protocol is reported either way.
pub(crate) fn advice(
    descriptor: &Value,
    latest: Option<&Version>,
    source: Source,
    prebuilt_for_platform: bool,
) -> Option<Advice> {
    let installed_text = descriptor
        .pointer("/runtime/version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty());
    let installed = installed_text.and_then(|version| Version::parse(version).ok());
    let newer = match (&installed, latest) {
        (Some(installed), Some(latest)) if latest > installed => Some(latest),
        _ => None,
    };
    let protocol = protocol(descriptor);
    if protocol == Protocol::HeadlineFirst && newer.is_none() {
        return None;
    }

    let name = match installed_text {
        Some(version) => format!("BaseRT {version}"),
        None => "This BaseRT".to_string(),
    };
    // The release worth moving to: the newest when it is known to carry the
    // protocol, otherwise the first one that does.
    let target = match latest {
        Some(latest) if *latest >= HEADLINE_FIRST_SINCE => format!("BaseRT {latest}"),
        _ if protocol != Protocol::HeadlineFirst => {
            format!("BaseRT {HEADLINE_FIRST_SINCE} or newer")
        }
        _ => "the newer release".to_string(),
    };
    let (summary, consequence, plan_note) = match protocol {
        Protocol::HeadlineFirst => (
            format!(
                "{target} is available (installed: {}).",
                installed_text.unwrap_or("unknown")
            ),
            None,
            None,
        ),
        Protocol::IsolatedWorkloads => (
            format!("{name} predates the current benchmark protocol."),
            Some(format!(
                "Its runs are signed without the headline-first order. {target} measures PP512 and TG128 first, in a freshly loaded model with a 4K reservation, and reports telemetry from the timed repetitions."
            )),
            Some("Older BaseRT protocol: no headline-first order"),
        ),
        Protocol::SharedContext => (
            format!("{name} predates the current benchmark protocol."),
            Some(format!(
                "Its runs share one context across the sweep and are signed as computearena-throughput-legacy/1, marked not comparable. {target} measures PP512 and TG128 first, in a freshly loaded model with a 4K reservation, and reports telemetry from the timed repetitions."
            )),
            Some("Older BaseRT protocol: signed as not comparable"),
        ),
    };

    let chosen_by_hand = match source {
        Source::Override => Some("--runtime-path".to_string()),
        Source::Environment(variable) => Some(variable.to_string()),
        Source::Managed | Source::Path | Source::KnownLocation => None,
    };
    let (action, installable) = match (chosen_by_hand, prebuilt_for_platform) {
        (Some(how), true) => (
            format!(
                "This harness was chosen with {how}: point that at a newer build, or leave it out and run `computearena basert install`."
            ),
            false,
        ),
        (Some(how), false) => (
            format!(
                "This harness was chosen with {how}: point that at a build of the newer release ({BASERT_RELEASES})."
            ),
            false,
        ),
        (None, true) => (
            format!(
                "Update with `computearena basert install`, or the official installer: {BASERT_INSTALL_SCRIPT}"
            ),
            true,
        ),
        (None, false) => (
            format!(
                "No prebuilt BaseRT is published for this platform; build the harness from the newer release: {BASERT_RELEASES}"
            ),
            false,
        ),
    };
    Some(Advice {
        summary,
        consequence,
        action,
        installable,
        plan_note,
    })
}

/// Whether `computearena basert install` can fetch a bundle on this machine.
pub(crate) fn prebuilt_for_this_platform() -> bool {
    crate::runtimes::asset_rule(
        crate::adapters::Runtime::Basert,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
    .is_ok()
}

pub(crate) fn print_advice(ui: TerminalUi, advice: &Advice) {
    println!("{} {}", ui.warning("!"), ui.strong(&advice.summary));
    if let Some(consequence) = &advice.consequence {
        println!("  {}", ui.neutral(consequence));
    }
    println!("  {}", ui.neutral(&advice.action));
}

/// The release lookup for one command: what the last lookup found, and the
/// one in progress. Nothing here waits for the network.
pub(crate) struct Watch {
    latest: Option<LatestRelease>,
    check: Option<ReleaseCheck>,
}

impl Watch {
    pub(crate) fn start(paths: &Paths) -> Self {
        let (latest, check) = crate::updates::BASERT.start(paths);
        Self { latest, check }
    }

    #[cfg(test)]
    pub(crate) fn known(latest: Option<LatestRelease>) -> Self {
        Self {
            latest,
            check: None,
        }
    }

    /// Takes in the lookup's answer if it has arrived; true when it changed
    /// what is known.
    pub(crate) fn refresh(&mut self) -> bool {
        let Some(answer) = self.check.as_ref().and_then(ReleaseCheck::poll) else {
            return false;
        };
        self.check = None;
        match answer {
            Some(release) if self.latest.as_ref() != Some(&release) => {
                self.latest = Some(release);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn latest(&self) -> Option<&Version> {
        self.latest.as_ref().map(|release| &release.version)
    }

    pub(crate) fn advice(&self, descriptor: &Value, source: Source) -> Option<Advice> {
        advice(
            descriptor,
            self.latest(),
            source,
            prebuilt_for_this_platform(),
        )
    }
}

/// One `run`: says what is known before the benchmark starts, and afterwards
/// only what the lookup learned in the meantime, so nothing is said twice.
pub(crate) struct RunNotice {
    watch: Watch,
    descriptor: Option<Value>,
    source: Source,
    said: bool,
}

impl RunNotice {
    /// A harness that cannot be probed says nothing here; the run itself
    /// explains what is wrong with it.
    pub(crate) fn start(paths: &Paths, harness: &std::path::Path, source: Source) -> Self {
        use crate::adapters::Runtime;
        Self {
            watch: Watch::start(paths),
            descriptor: Runtime::Basert.adapter().probe(harness).ok(),
            source,
            said: false,
        }
    }

    fn say(&mut self, ui: TerminalUi) {
        self.watch.refresh();
        let Some(descriptor) = &self.descriptor else {
            return;
        };
        if let Some(advice) = self.watch.advice(descriptor, self.source) {
            print_advice(ui, &advice);
            self.said = true;
        }
    }

    pub(crate) fn before_run(&mut self, ui: TerminalUi) {
        self.say(ui);
    }

    /// A benchmark takes minutes, so a lookup that was still running when it
    /// started has answered by now.
    pub(crate) fn after_run(&mut self, ui: TerminalUi) {
        if !self.said {
            self.say(ui);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn harness(version: &str, headline: bool, isolated: bool) -> Value {
        json!({
            "runtime": {"name": "basert", "version": version},
            "capacity_protocol_schema": "basert-throughput-protocol/2",
            "features": {
                "headline_context_capacity": headline,
                "isolated_workload_contexts": isolated
            }
        })
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn a_current_basert_with_nothing_newer_says_nothing() {
        let current = harness("0.2.5", true, true);
        assert_eq!(advice(&current, None, Source::Managed, true), None);
        assert_eq!(
            advice(&current, Some(&version("0.2.5")), Source::Managed, true),
            None
        );
        // An older release on the feed is not an update.
        assert_eq!(
            advice(&current, Some(&version("0.2.4")), Source::Managed, true),
            None
        );
    }

    #[test]
    fn a_newer_release_is_offered_with_the_command_that_installs_it() {
        let advice = advice(
            &harness("0.2.5", true, true),
            Some(&version("0.2.6")),
            Source::KnownLocation,
            true,
        )
        .unwrap();
        assert_eq!(
            advice.summary,
            "BaseRT 0.2.6 is available (installed: 0.2.5)."
        );
        assert_eq!(advice.consequence, None);
        assert_eq!(advice.plan_note, None);
        assert!(advice.installable);
        assert!(advice.action.contains("`computearena basert install`"));
        assert!(advice.action.contains(BASERT_INSTALL_SCRIPT));
    }

    #[test]
    fn an_older_protocol_is_reported_offline_and_names_what_the_report_will_say() {
        // 0.2.4: neither isolated contexts nor the headline order.
        let offline = advice(&harness("0.2.4", false, false), None, Source::Path, true).unwrap();
        assert_eq!(
            offline.summary,
            "BaseRT 0.2.4 predates the current benchmark protocol."
        );
        let consequence = offline.consequence.as_deref().unwrap();
        assert!(consequence.contains("computearena-throughput-legacy/1"));
        assert!(consequence.contains("not comparable"));
        assert!(consequence.contains("BaseRT 0.2.5 or newer measures PP512 and TG128 first"));
        assert_eq!(
            offline.plan_note,
            Some("Older BaseRT protocol: signed as not comparable")
        );
        assert!(offline.installable);

        // With the feed's answer the advice names the release to move to.
        let online = advice(
            &harness("0.2.4", false, false),
            Some(&version("0.2.6")),
            Source::Path,
            true,
        )
        .unwrap();
        assert!(online
            .consequence
            .unwrap()
            .contains("BaseRT 0.2.6 measures PP512 and TG128 first"));

        // Isolated contexts without the headline order are still comparable.
        let isolated = advice(&harness("0.2.4", false, true), None, Source::Path, true).unwrap();
        assert_eq!(
            isolated.plan_note,
            Some("Older BaseRT protocol: no headline-first order")
        );
        let consequence = isolated.consequence.unwrap();
        assert!(consequence.contains("without the headline-first order"));
        assert!(!consequence.contains("legacy"));
    }

    #[test]
    fn the_action_fits_how_the_harness_was_found_and_what_the_platform_offers() {
        let old = harness("0.2.4", false, false);
        let by_flag = advice(&old, None, Source::Override, true).unwrap();
        assert!(by_flag.action.contains("chosen with --runtime-path"));
        assert!(!by_flag.installable);

        let by_variable = advice(
            &old,
            None,
            Source::Environment("COMPUTEARENA_BASERT_HARNESS"),
            true,
        )
        .unwrap();
        assert!(by_variable
            .action
            .contains("chosen with COMPUTEARENA_BASERT_HARNESS"));

        let no_bundle = advice(&old, None, Source::Path, false).unwrap();
        assert!(no_bundle.action.contains("No prebuilt BaseRT is published"));
        assert!(no_bundle.action.contains(BASERT_RELEASES));
        assert!(!no_bundle.installable);
        assert!(!no_bundle.action.contains("basert install"));
    }

    #[test]
    fn versions_that_cannot_be_compared_never_invent_an_update() {
        let unversioned = json!({
            "runtime": {"name": "basert"},
            "capacity_protocol_schema": "basert-throughput-protocol/2",
            "features": {"headline_context_capacity": true}
        });
        assert_eq!(
            advice(&unversioned, Some(&version("9.9.9")), Source::Managed, true),
            None
        );
        let development = harness("main-abc123", true, true);
        assert_eq!(
            advice(&development, Some(&version("9.9.9")), Source::Managed, true),
            None
        );
        // An unversioned harness that lacks the protocol is still told so.
        let old = json!({"runtime": {"name": "basert"}, "features": {}});
        let advice = advice(&old, None, Source::Managed, true).unwrap();
        assert_eq!(
            advice.summary,
            "This BaseRT predates the current benchmark protocol."
        );

        // A harness newer than this client understands is not "older".
        let future = json!({
            "runtime": {"name": "basert", "version": "0.9.0"},
            "capacity_protocol_schema": "basert-throughput-protocol/9",
            "features": {"headline_context_capacity": true}
        });
        assert_eq!(super::advice(&future, None, Source::Managed, true), None);
    }

    #[test]
    fn a_lookup_that_finishes_later_updates_what_is_known_once() {
        let mut watch = Watch::known(Some(crate::updates::release_for_tests("0.2.6")));
        assert_eq!(watch.latest(), Some(&version("0.2.6")));
        // Nothing in progress: nothing changes.
        assert!(!watch.refresh());
        let advice = watch
            .advice(&harness("0.2.5", true, true), Source::Managed)
            .unwrap();
        assert!(advice.summary.contains("BaseRT 0.2.6 is available"));
    }
}
