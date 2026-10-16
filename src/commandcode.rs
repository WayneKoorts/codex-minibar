//! Command Code (`cmdc`) rolling and monthly credit limits from the official
//! CLI login.
//!
//! `cmd login` stores a `user_...` API key in `~/.commandcode/auth.json`. The
//! CLI's own `/alpha/billing/*` routes accept it as a bearer token: `credits`
//! reports the 5-hour and weekly USD windows plus the remaining monthly grant,
//! and `subscriptions` names the plan that sizes that grant.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use directories::BaseDirs;
use serde_json::Value;

use crate::{
    limits::{AdditionalLimit, Credits, LimitWindow, RateLimits, UsageAmount},
    provider_cli,
    usage::UsageStatistics,
    worker::{Activator, LimitProvider, UsageProvider},
};

const API_BASE: &str = "https://api.commandcode.ai";
const CREDITS_PATH: &str = "/alpha/billing/credits";
const SUBSCRIPTIONS_PATH: &str = "/alpha/billing/subscriptions";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const FIVE_HOUR_MINUTES: u32 = 5 * 60;
const WEEKLY_MINUTES: u32 = 7 * 24 * 60;
const MONTHLY_MINUTES: u32 = 30 * 24 * 60;
/// `cmd` itself is deliberately absent: on Windows it would resolve to
/// `cmd.exe`. The npm package installs these aliases next to it.
const CLI_NAMES: &[&str] = &[
    "cmdc.cmd",
    "cmdc.ps1",
    "cmdc.exe",
    "command-code.cmd",
    "command-code.ps1",
    "commandcode.cmd",
    "commandcode.ps1",
];
pub(crate) const MONTHLY_LIMIT_ID: &str = "monthly";

/// Monthly USD allowance per subscription plan, from
/// https://commandcode.ai/pricing. The credits response reports only the
/// remaining balance unless it includes `monthlyCreditsGranted`.
const PLANS: &[(&str, &str, f64)] = &[
    ("individual-go", "Go", 10.0),
    ("individual-go-v1", "Go", 10.0),
    ("individual-pro", "Pro", 30.0),
    ("individual-pro-v1", "Pro", 80.0),
    ("individual-goat", "GOAT", 70.0),
    ("individual-max", "Max", 150.0),
    ("individual-ultra", "Ultra", 300.0),
];

pub struct CommandCodeClient {
    agent: ureq::Agent,
}

pub struct CommandCodeActivator;

impl CommandCodeClient {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build(),
        }
    }

    fn get(&self, path: &str, api_key: &str) -> Result<String> {
        let response = match self
            .agent
            .get(&format!("{API_BASE}{path}"))
            .set("Authorization", &format!("Bearer {api_key}"))
            .set("Accept", "application/json")
            .set("User-Agent", "Codex Minibar")
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(401 | 403, _)) => {
                bail!("Command Code API key was rejected; run `cmdc login`")
            }
            Err(ureq::Error::Status(429, _)) => {
                return Err(crate::worker::rate_limit_error(
                    "Command Code billing request was rate limited (HTTP 429).",
                ));
            }
            Err(ureq::Error::Status(status, _)) => {
                bail!("Command Code billing request failed with HTTP {status}")
            }
            Err(error) => return Err(error).context("request Command Code billing"),
        };
        response
            .into_string()
            .context("read Command Code billing response")
    }
}

impl Default for CommandCodeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LimitProvider for CommandCodeClient {
    fn read_limits(&mut self) -> Result<RateLimits> {
        let auth = load_auth()?;
        // The plan only sizes the monthly row, so its lookup runs alongside
        // the credits request and a failure never hides the rolling windows.
        let (credits, subscription) = thread::scope(|scope| {
            let subscription = scope.spawn(|| {
                self.get(SUBSCRIPTIONS_PATH, &auth.api_key)
                    .and_then(|body| parse_subscription(&body))
            });
            let credits = self
                .get(CREDITS_PATH, &auth.api_key)
                .and_then(|body| parse_credits(&body));
            let subscription = subscription
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Command Code plan lookup panicked")));
            (credits, subscription)
        });
        let credits = credits?;
        let subscription = subscription.unwrap_or_else(|error| {
            crate::logger::info(format!("Command Code plan lookup failed: {error:#}"));
            SubscriptionLookup::Unavailable
        });
        Ok(rate_limits(
            credits,
            subscription,
            auth.user_name,
            Utc::now(),
        ))
    }
}

impl UsageProvider for CommandCodeClient {
    fn load_cached_usage_statistics(&mut self, _history_days: u16) -> Result<UsageStatistics> {
        Ok(UsageStatistics::default())
    }

    fn refresh_usage_statistics(&mut self, _history_days: u16) -> Result<UsageStatistics> {
        // Subscription credits are quota, not API billing or local token spend.
        Ok(UsageStatistics::default())
    }
}

impl Activator for CommandCodeActivator {
    fn activate(&mut self) -> Result<()> {
        bail!("Command Code does not support session-window activation")
    }
}

pub fn is_installed(explicit: Option<&Path>) -> bool {
    signed_in() || cli_available(explicit).is_some()
}

/// Resolves a Command Code CLI launcher. An explicit folder is searched first;
/// a file path remains supported so a copied launcher still counts.
pub fn cli_available(explicit: Option<&Path>) -> Option<PathBuf> {
    provider_cli::candidates_from(explicit, npm_launchers(), CLI_NAMES)
        .into_iter()
        .next()
}

fn npm_launchers() -> Vec<PathBuf> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let npm = PathBuf::from(appdata).join("npm");
    CLI_NAMES.iter().map(|name| npm.join(name)).collect()
}

fn auth_path() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.home_dir().join(".commandcode/auth.json"))
}

fn signed_in() -> bool {
    auth_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|raw| parse_auth(&raw).is_ok())
}

struct CommandCodeAuth {
    api_key: String,
    user_name: Option<String>,
}

fn load_auth() -> Result<CommandCodeAuth> {
    let path = auth_path().context("Command Code home directory is unavailable")?;
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "Command Code auth not found at {}; run `cmdc login`",
            path.display()
        )
    })?;
    parse_auth(&raw)
}

fn parse_auth(raw: &str) -> Result<CommandCodeAuth> {
    let root: Value = serde_json::from_str(raw).context("parse Command Code auth.json")?;
    let api_key = root
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .context("Command Code auth.json has no API key; run `cmdc login`")?
        .to_owned();
    Ok(CommandCodeAuth {
        api_key,
        user_name: root
            .get("userName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    })
}

#[derive(Debug, PartialEq)]
struct RollingWindow {
    used: f64,
    cap: f64,
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq)]
struct CreditsPayload {
    monthly_remaining: f64,
    monthly_granted: Option<f64>,
    purchased: f64,
    five_hour: Option<RollingWindow>,
    weekly: Option<RollingWindow>,
}

#[derive(Debug, PartialEq)]
struct Subscription {
    plan_id: String,
    period_end: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq)]
enum SubscriptionLookup {
    /// A successful response with `data: null`: no paid subscription.
    Free,
    Paid(Subscription),
    /// The lookup failed; the monthly row may still be sized from the grant.
    Unavailable,
}

fn parse_credits(body: &str) -> Result<CreditsPayload> {
    let root: Value = serde_json::from_str(body).context("parse Command Code credits response")?;
    let credits = root
        .get("credits")
        .filter(|credits| credits.is_object())
        .context("Command Code credits response has no credits object")?;
    let monthly_remaining = number(credits.get("monthlyCredits"))
        .context("Command Code credits response has no monthly credits")?;
    let windows = root
        .get("windowLimits")
        .or_else(|| credits.get("windowLimits"));
    Ok(CreditsPayload {
        monthly_remaining,
        monthly_granted: number(credits.get("monthlyCreditsGranted")).filter(|value| *value > 0.0),
        purchased: number(credits.get("purchasedCredits")).unwrap_or(0.0),
        five_hour: windows.and_then(|windows| rolling_window(windows.get("fiveHour"))),
        weekly: windows.and_then(|windows| rolling_window(windows.get("weekly"))),
    })
}

fn rolling_window(value: Option<&Value>) -> Option<RollingWindow> {
    let value = value?;
    let cap = number(value.get("cap")).filter(|cap| *cap > 0.0)?;
    Some(RollingWindow {
        used: number(value.get("used")).unwrap_or(0.0).max(0.0),
        cap,
        resets_at: timestamp(value.get("resetAt")),
    })
}

fn parse_subscription(body: &str) -> Result<SubscriptionLookup> {
    let root: Value =
        serde_json::from_str(body).context("parse Command Code subscription response")?;
    // Only an explicit successful null identifies the free tier; failure
    // envelopes are transient.
    if root.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("Command Code subscription response was unsuccessful")
    }
    let data = match root.get("data") {
        Some(Value::Null) => return Ok(SubscriptionLookup::Free),
        Some(data) if data.is_object() => data,
        _ => bail!("Command Code subscription response has no data"),
    };
    let plan_id = data
        .get("planId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .context("Command Code subscription has no plan")?
        .to_owned();
    Ok(SubscriptionLookup::Paid(Subscription {
        plan_id,
        period_end: timestamp(data.get("currentPeriodEnd")),
    }))
}

fn plan(plan_id: &str) -> Option<(&'static str, f64)> {
    PLANS
        .iter()
        .find(|(id, _, _)| id.eq_ignore_ascii_case(plan_id))
        .map(|(_, name, credits)| (*name, *credits))
}

fn rate_limits(
    credits: CreditsPayload,
    subscription: SubscriptionLookup,
    account_name: Option<String>,
    sampled_at: DateTime<Utc>,
) -> RateLimits {
    let (plan_type, plan_credits, period_end) = match &subscription {
        SubscriptionLookup::Paid(subscription) => match plan(&subscription.plan_id) {
            Some((name, credits)) => (
                Some(name.to_owned()),
                Some(credits),
                subscription.period_end,
            ),
            // Unknown plans keep their raw id and only an API-reported grant.
            None => (
                Some(subscription.plan_id.clone()),
                None,
                subscription.period_end,
            ),
        },
        SubscriptionLookup::Free | SubscriptionLookup::Unavailable => (None, None, None),
    };
    let monthly_total = credits.monthly_granted.or(plan_credits);
    let additional_limits = monthly_total
        .map(|total| {
            let used = (total - credits.monthly_remaining).clamp(0.0, total);
            AdditionalLimit {
                id: MONTHLY_LIMIT_ID.into(),
                title: "Monthly".into(),
                window: LimitWindow {
                    used_percent: Some(percent(used, total)),
                    resets_at: period_end,
                    duration_minutes: Some(MONTHLY_MINUTES),
                },
            }
        })
        .into_iter()
        .collect();
    let window = |window: Option<&RollingWindow>, minutes| {
        window.map_or_else(LimitWindow::default, |window| LimitWindow {
            used_percent: Some(percent(window.used, window.cap)),
            resets_at: window.resets_at,
            duration_minutes: Some(minutes),
        })
    };
    let amount = |window: Option<&RollingWindow>| {
        window.map(|window| UsageAmount {
            used: window.used,
            limit: window.cap,
        })
    };
    let purchased = (credits.purchased > 0.0).then(|| format!("${:.2}", credits.purchased));
    RateLimits {
        primary: window(credits.five_hour.as_ref(), FIVE_HOUR_MINUTES),
        secondary: window(credits.weekly.as_ref(), WEEKLY_MINUTES),
        primary_usage_amount: amount(credits.five_hour.as_ref()),
        secondary_usage_amount: amount(credits.weekly.as_ref()),
        sampled_at,
        account_name,
        plan_type,
        limit_name: Some("Command Code".into()),
        credits: Credits {
            has_credits: purchased.is_some(),
            unlimited: false,
            balance: purchased,
        },
        additional_limits,
        ..RateLimits::default()
    }
}

fn percent(used: f64, limit: f64) -> u8 {
    (used / limit * 100.0).clamp(0.0, 100.0).round() as u8
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite())
}

/// Epoch seconds, epoch milliseconds, or RFC 3339. The API reports `0` for a
/// window that has not started yet, which has no reset time.
fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    if let Some(Value::String(text)) = value
        && let Ok(parsed) = DateTime::parse_from_rfc3339(text.trim())
    {
        return Some(parsed.with_timezone(&Utc));
    }
    let raw = number(value).filter(|value| *value > 0.0)?;
    let millis = if raw > 10_000_000_000.0 {
        raw
    } else {
        raw * 1000.0
    };
    Utc.timestamp_millis_opt(millis as i64).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDITS: &str = r#"{
      "credits":{"monthlyCredits":7.5,"purchasedCredits":2,"freeCredits":0},
      "windowLimits":{
        "fiveHour":{"used":1.5,"cap":3,"resetAt":1792141200000},
        "weekly":{"used":0,"cap":6,"resetAt":0}
      }
    }"#;

    #[test]
    fn reads_the_api_key_and_user_name() {
        let auth = parse_auth(r#"{"apiKey":" user_secret ","userName":"octocat","keyName":"cli"}"#)
            .unwrap();
        assert_eq!(auth.api_key, "user_secret");
        assert_eq!(auth.user_name.as_deref(), Some("octocat"));
        assert!(parse_auth(r#"{"apiKey":""}"#).is_err());
    }

    #[test]
    fn maps_rolling_windows_and_sizes_monthly_from_the_plan() {
        let credits = parse_credits(CREDITS).unwrap();
        let subscription = parse_subscription(
            r#"{"success":true,"data":{"planId":"individual-go-v1","status":"active","currentPeriodEnd":"2026-11-16T16:09:56.000Z"}}"#,
        )
        .unwrap();
        let limits = rate_limits(credits, subscription, None, Utc::now());

        assert_eq!(limits.primary.used_percent, Some(50));
        assert_eq!(limits.primary.duration_minutes, Some(FIVE_HOUR_MINUTES));
        assert!(limits.primary.resets_at.is_some());
        assert_eq!(
            limits.primary_usage_amount,
            Some(UsageAmount {
                used: 1.5,
                limit: 3.0
            })
        );
        // An untouched window reports `resetAt: 0`, which is not a reset time.
        assert_eq!(limits.secondary.used_percent, Some(0));
        assert_eq!(limits.secondary.resets_at, None);
        assert_eq!(limits.plan_type.as_deref(), Some("Go"));

        let monthly = &limits.additional_limits[0];
        assert_eq!(monthly.id, MONTHLY_LIMIT_ID);
        assert_eq!(monthly.window.used_percent, Some(25));
        assert_eq!(
            monthly.window.resets_at,
            timestamp(Some(&Value::from("2026-11-16T16:09:56.000Z")))
        );
        assert_eq!(limits.credits.balance.as_deref(), Some("$2.00"));
    }

    #[test]
    fn reported_grant_wins_over_the_catalog_and_survives_a_failed_lookup() {
        let credits =
            parse_credits(r#"{"credits":{"monthlyCredits":"15","monthlyCreditsGranted":20}}"#)
                .unwrap();
        let limits = rate_limits(credits, SubscriptionLookup::Unavailable, None, Utc::now());
        assert_eq!(limits.additional_limits[0].window.used_percent, Some(25));
        assert_eq!(limits.additional_limits[0].window.resets_at, None);
        assert!(limits.primary.is_empty());
        assert!(!limits.credits.has_credits);
    }

    #[test]
    fn unknown_or_free_plans_have_no_monthly_percentage() {
        let free = parse_subscription(r#"{"success":true,"data":null}"#).unwrap();
        assert_eq!(free, SubscriptionLookup::Free);
        let limits = rate_limits(parse_credits(CREDITS).unwrap(), free, None, Utc::now());
        assert!(limits.additional_limits.is_empty());
        // "Free" would switch the popup to its monthly-only layout.
        assert_eq!(limits.plan_type, None);

        let unknown = parse_subscription(
            r#"{"success":true,"data":{"planId":"team-enterprise","status":"active"}}"#,
        )
        .unwrap();
        let limits = rate_limits(parse_credits(CREDITS).unwrap(), unknown, None, Utc::now());
        assert!(limits.additional_limits.is_empty());
        assert_eq!(limits.plan_type.as_deref(), Some("team-enterprise"));
    }

    #[test]
    fn failure_envelopes_are_not_the_free_tier() {
        assert!(parse_subscription(r#"{"success":false,"data":null}"#).is_err());
        assert!(parse_subscription(r#"{"data":null}"#).is_err());
        assert!(parse_credits(r#"{"credits":{}}"#).is_err());
    }

    #[test]
    fn parses_second_millisecond_and_iso_timestamps() {
        let expected = Utc.timestamp_opt(1_792_141_200, 0).single();
        assert_eq!(timestamp(Some(&Value::from(1_792_141_200_u64))), expected);
        assert_eq!(
            timestamp(Some(&Value::from(1_792_141_200_000_u64))),
            expected
        );
        assert_eq!(timestamp(Some(&Value::from(0))), None);
        assert!(timestamp(Some(&Value::from("2026-11-16T16:09:56Z"))).is_some());
    }

    #[test]
    fn never_resolves_the_windows_cmd_shell() {
        assert!(!CLI_NAMES.iter().any(|name| name.starts_with("cmd.")));
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("cmdc.cmd");
        fs::write(&launcher, b"fixture").unwrap();
        assert_eq!(
            cli_available(Some(directory.path())),
            Some(fs::canonicalize(launcher).unwrap())
        );
    }
}
