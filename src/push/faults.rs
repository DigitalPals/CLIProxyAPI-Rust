//! The dashboard's faults, worked out on the server so they can be pushed. The rules
//! mirror `alertsList()` and `acctState()` in ui/app.js, so every notification is
//! something the faults menu shows too, plus whole providers that have run out.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::quota::Window;

/// Which `notifications` setting sends a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SignIn,
    ProviderExhausted,
    AccountUsedUp,
    AccountErrors,
    /// Counted for the app badge, never sent (rate limits come and go too often).
    Quiet,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fault {
    /// Stable identity: `signin:<id>`, `quota:<id>`, `error:<id>`, `failures:<id>`,
    /// `rate:<id>` or `provider:<provider>`. Also the notification's tag.
    pub key: String,
    pub event: Event,
    /// The provider group, so one provider running out can stand in for its accounts.
    pub provider: String,
    pub title: String,
    pub body: String,
    /// Where a click lands in the dashboard.
    pub path: String,
}

/// What the rules need to know about one account.
#[derive(Debug, Clone, Default)]
pub struct AccountView {
    pub id: String,
    /// Provider group: the provider id, or the compatibility group's name.
    pub provider: String,
    /// The account's provider as the dashboard names it ("Claude", "OpenAI" for a key).
    pub provider_name: String,
    /// The provider group's name ("Claude", "Codex").
    pub group_name: String,
    pub label: String,
    pub api_key: bool,
    pub disabled: bool,
    /// From `AccountState::pauses`: model -> (until, kind).
    pub pauses: BTreeMap<String, (DateTime<Utc>, &'static str)>,
    pub last_error: Option<String>,
    pub windows: Vec<Window>,
    /// Failed requests (not cancellations) in the last hour.
    pub failures: u64,
}

impl AccountView {
    fn signin_error(&self) -> bool {
        !self.api_key && self.last_error.as_deref().is_some_and(signin_error)
    }

    /// Can't take any request now: paused whole for a limit, or its sign-in expired.
    fn unavailable(&self) -> bool {
        matches!(self.pauses.get("*"), Some((_, "quota" | "rate_limit"))) || self.signin_error()
    }

    fn path(&self) -> String {
        format!("#/accounts/{}", url::form_urlencoded::byte_serialize(self.id.as_bytes()).collect::<String>())
    }
}

/// The text the dashboard reads as an expired sign-in (`SIGNIN_ERR` in ui/app.js).
pub fn signin_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "invalid_grant",
        "refresh token",
        "sign in again",
        "reauthenticat",
        "re-authenticat",
        "unauthorized",
        "unauthorised",
        "token expired",
        "token has expired",
        "expired token",
        "revoked",
    ]
    .iter()
    .any(|p| t.contains(p))
        || t.match_indices("401").any(|(i, _)| {
            let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            !word(t[..i].chars().next_back()) && !word(t[i + 3..].chars().next())
        })
}

pub fn faults(accounts: &[AccountView], now: DateTime<Utc>) -> Vec<Fault> {
    let mut out = Vec::new();
    for a in accounts.iter().filter(|a| !a.disabled) {
        let fault = |key: &str, event, title: String, body: String| Fault {
            key: format!("{key}:{}", a.id),
            event,
            provider: a.provider.clone(),
            title,
            body,
            path: a.path(),
        };
        // The latest pause decides the state, as in the dashboard.
        let pause = a.pauses.iter().max_by_key(|(_, (until, _))| *until);
        let error = pause.is_none() && a.last_error.is_some();
        match pause {
            Some((_, (until, "quota"))) => {
                let spent = spent_window(&a.windows, now);
                let back = spent.and_then(|w| w.resets_at).unwrap_or(*until);
                let limit = spent.map_or("usage", |w| window_title(&w.name));
                out.push(fault(
                    "quota",
                    Event::AccountUsedUp,
                    format!("{}: {limit} limit used up", a.label),
                    format!("{} · back in {}.", a.provider_name, span(back - now)),
                ));
            }
            Some((model, (until, "rate_limit"))) => {
                let what = if model == "*" { "Every model".to_string() } else { model.clone() };
                out.push(fault(
                    "rate",
                    Event::Quiet,
                    format!("{}: rate limited", a.label),
                    format!("{what} is paused for {}.", span(*until - now)),
                ));
            }
            _ => {}
        }
        if error {
            if a.signin_error() {
                out.push(fault(
                    "signin",
                    Event::SignIn,
                    format!("{} sign-in expired", a.provider_name),
                    format!("{}: sign in again to put it back in rotation.", a.label),
                ));
            } else {
                let message: String = a.last_error.as_deref().unwrap_or_default().chars().take(120).collect();
                out.push(fault(
                    "error",
                    Event::AccountErrors,
                    format!("{} account error", a.provider_name),
                    format!("{}: {message}", a.label),
                ));
            }
        }
        if a.failures >= 3 && !error {
            let mut f = fault(
                "failures",
                Event::AccountErrors,
                format!("{} failed requests", a.failures),
                format!("{} · {} in the last hour.", a.provider_name, a.label),
            );
            f.path =
                format!("#/requests?acc={}", url::form_urlencoded::byte_serialize(a.id.as_bytes()).collect::<String>());
            out.push(f);
        }
    }

    // Providers whose every enabled account is out.
    let mut groups: BTreeMap<&str, Vec<&AccountView>> = BTreeMap::new();
    for a in accounts.iter().filter(|a| !a.disabled) {
        groups.entry(a.provider.as_str()).or_default().push(a);
    }
    for (provider, members) in groups {
        if !members.iter().all(|a| a.unavailable()) {
            continue;
        }
        let name = &members[0].group_name;
        let back = members
            .iter()
            .filter_map(|a| a.pauses.get("*").filter(|(_, kind)| matches!(*kind, "quota" | "rate_limit")))
            .map(|(until, _)| *until)
            .min();
        let used_up = members.iter().all(|a| matches!(a.pauses.get("*"), Some((_, "quota"))));
        let title = if used_up {
            format!("Every {name} account is used up")
        } else {
            format!("No {name} account can take requests")
        };
        let body = match back {
            Some(t) => format!("Requests for {name} models will fail. The first is back in {}.", span(t - now)),
            None => format!("Requests for {name} models will fail until an account is signed in again."),
        };
        out.push(Fault {
            key: format!("provider:{provider}"),
            event: Event::ProviderExhausted,
            provider: provider.to_string(),
            title,
            body,
            path: "#/accounts".into(),
        });
    }
    out
}

/// A provider that was out has an account that can take requests again.
pub fn recovered(accounts: &[AccountView], provider: &str) -> Option<Fault> {
    let back = accounts.iter().find(|a| !a.disabled && a.provider == provider && !a.unavailable())?;
    Some(Fault {
        key: format!("provider:{provider}"),
        event: Event::ProviderExhausted,
        provider: provider.to_string(),
        title: format!("{} is back", back.group_name),
        body: format!("{} can take requests again.", back.label),
        path: back.path(),
    })
}

/// The 5-hour or weekly window that is used up, as the dashboard picks it.
fn spent_window(windows: &[Window], now: DateTime<Utc>) -> Option<&Window> {
    let live = |short: bool| {
        windows
            .iter()
            .filter(|w| w.model.is_none() && w.resets_at.is_none_or(|r| r > now))
            .filter(move |w| is_hours(&w.name) == short)
            .max_by(|x, y| x.used.total_cmp(&y.used))
    };
    [live(true), live(false)].into_iter().flatten().find(|w| w.used >= 100.0)
}

fn is_hours(name: &str) -> bool {
    name.strip_suffix('h').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn window_title(name: &str) -> &'static str {
    match name {
        "5h" => "5-hour",
        "day" => "daily",
        n if is_hours(n) => "short",
        _ => "weekly",
    }
}

/// "2h 14m", "3d 7h", "45m", "<1m": the dashboard's countdowns, at most two units.
pub fn span(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..60 => "<1m".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn acct(id: &str, provider: &str) -> AccountView {
        AccountView {
            id: id.into(),
            provider: provider.into(),
            provider_name: "Claude".into(),
            group_name: "Claude".into(),
            label: format!("{id}@example.com"),
            ..Default::default()
        }
    }

    fn keys(f: &[Fault]) -> Vec<&str> {
        f.iter().map(|f| f.key.as_str()).collect()
    }

    #[test]
    fn reads_sign_in_errors_like_the_dashboard() {
        for text in [
            "token refresh failed: invalid_grant",
            "401 Unauthorized",
            "upstream said 401",
            "Your refresh token was revoked",
            "OAuth token has expired",
            "please sign in again",
        ] {
            assert!(signin_error(text), "{text}");
        }
        for text in ["429 rate limited", "token refresh failed: connection reset", "status 4010", "5401 bytes"] {
            assert!(!signin_error(text), "{text}");
        }
    }

    #[test]
    fn mirrors_the_faults_menu() {
        let now = Utc::now();
        let mut signin = acct("a", "claude");
        signin.last_error = Some("token refresh failed: invalid_grant".into());
        let mut key = acct("k", "claude");
        key.api_key = true;
        key.last_error = Some("401 Unauthorized".into());
        let mut spent = acct("q", "claude");
        spent.pauses.insert("*".into(), (now + Duration::hours(3), "quota"));
        spent.windows =
            vec![Window { name: "week".into(), used: 100.0, resets_at: Some(now + Duration::hours(50)), model: None }];
        // Cooling wins over an old error, as in the dashboard.
        let mut limited = acct("r", "claude");
        limited.pauses.insert("claude-opus".into(), (now + Duration::minutes(5), "rate_limit"));
        limited.last_error = Some("429".into());
        limited.failures = 4;
        let mut checking = acct("c", "claude");
        checking.pauses.insert("*".into(), (now + Duration::seconds(30), "checking"));
        let mut off = acct("d", "claude");
        off.disabled = true;
        off.last_error = Some("invalid_grant".into());

        let f = faults(&[signin, key, spent, limited, checking, off], now);
        assert_eq!(keys(&f), ["signin:a", "error:k", "quota:q", "rate:r", "failures:r"]);
        assert_eq!(f[0].title, "Claude sign-in expired");
        assert_eq!(f[0].path, "#/accounts/a");
        assert_eq!(f[2].title, "q@example.com: weekly limit used up");
        assert_eq!(f[2].body, "Claude · back in 2d 2h.");
        assert_eq!(f[3].event, Event::Quiet);
        assert_eq!(f[4].path, "#/requests?acc=r");
    }

    #[test]
    fn a_provider_runs_out_when_every_enabled_account_is_out() {
        let now = Utc::now();
        let mut a = acct("a", "claude");
        a.pauses.insert("*".into(), (now + Duration::hours(2), "quota"));
        let mut b = acct("b", "claude");
        b.pauses.insert("*".into(), (now + Duration::minutes(30), "quota"));
        let mut off = acct("c", "claude");
        off.disabled = true;
        let mut other = acct("x", "codex");
        other.group_name = "Codex".into();
        other.pauses.insert("gpt-6".into(), (now + Duration::hours(1), "rate_limit"));

        let f = faults(&[a.clone(), b.clone(), off, other.clone()], now);
        let p = f.iter().find(|f| f.key == "provider:claude").unwrap();
        assert_eq!(p.title, "Every Claude account is used up");
        assert_eq!(p.body, "Requests for Claude models will fail. The first is back in 30m.");
        assert!(!keys(&f).contains(&"provider:codex"));
        assert!(recovered(&[a.clone(), b.clone()], "claude").is_none());

        // A sign-in problem counts as out, but changes the wording.
        let mut expired = acct("e", "claude");
        expired.last_error = Some("invalid_grant".into());
        let f = faults(&[a.clone(), expired.clone()], now);
        assert_eq!(f.iter().find(|f| f.key == "provider:claude").unwrap().title, "No Claude account can take requests");

        b.pauses.clear();
        let back = recovered(&[a, b], "claude").unwrap();
        assert_eq!(back.title, "Claude is back");
        assert_eq!(back.body, "b@example.com can take requests again.");
    }

    #[test]
    fn counts_down_like_the_dashboard() {
        assert_eq!(span(Duration::seconds(20)), "<1m");
        assert_eq!(span(Duration::minutes(45)), "45m");
        assert_eq!(span(Duration::minutes(134)), "2h 14m");
        assert_eq!(span(Duration::hours(79)), "3d 7h");
        assert_eq!(span(Duration::seconds(-5)), "<1m");
    }
}
