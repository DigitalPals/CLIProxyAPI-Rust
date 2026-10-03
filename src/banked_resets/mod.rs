//! Manual banked reset redemption. Tokens and upstream identities never leave the server.
mod ledger;
mod provider;
#[cfg(test)]
mod tests;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::accounts::{Account, Credential, Provider};
use crate::state::App;
use ledger::{Ledger, Operation};
use provider::{Api, HttpProvider, Inventory, Outcome};

#[derive(Clone, Debug, Serialize)]
pub struct View {
    pub checked_at: DateTime<Utc>,
    pub inventory: Option<Inventory>,
    pub error: Option<String>,
    pub quote: Option<String>,
    pub operation: Option<OperationView>,
    pub retryable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperationView {
    pub request_id: String,
    pub grant_id: String,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub message: String,
    pub retry_until: DateTime<Utc>,
}
impl From<&Operation> for OperationView {
    fn from(o: &Operation) -> Self {
        Self {
            request_id: o.request_id.clone(),
            grant_id: o.grant_id.clone(),
            created_at: o.created_at,
            status: o.status.clone(),
            message: o.message.clone(),
            retry_until: o.created_at + chrono::Duration::minutes(10),
        }
    }
}

pub struct Quote {
    pub account: String,
    pub identity: String,
    pub version: usize,
    pub fingerprint: String,
    pub expires: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub action: String,
    pub request_id: String,
    #[serde(default)]
    pub grant_id: String,
    pub confirmed: bool,
}

fn fingerprint(inventory: &Inventory) -> String {
    hex::encode(Sha256::digest(serde_json::to_vec(inventory).unwrap()))
}

fn account_id(acct: &Account) -> Result<String> {
    ensure!(
        matches!(acct.provider, Provider::Codex | Provider::Claude),
        "Banked resets are supported for Codex and Claude subscriptions"
    );
    match &*acct.cred.read() {
        Credential::OAuth(o) if o.base_url.as_ref().is_none_or(|s| s.trim().is_empty()) => o
            .account_id
            .clone()
            .filter(|s| !s.is_empty() && s.len() <= 200)
            .ok_or_else(|| anyhow!("Account identity is missing; sign in again")),
        _ => Err(anyhow!("Banked resets require a native subscription OAuth account")),
    }
}
async fn api(app: &App, acct: &Arc<Account>) -> Result<HttpProvider> {
    account_id(acct)?;
    crate::oauth::ensure_fresh(app, acct, chrono::Duration::minutes(5), false)
        .await
        .map_err(|_| anyhow!("Could not refresh subscription credentials"))?;
    let (id, token) = {
        let cred = acct.cred.read();
        match &*cred {
            Credential::OAuth(o) if o.base_url.as_ref().is_none_or(|s| s.trim().is_empty()) => {
                (o.account_id.clone().ok_or_else(|| anyhow!("Account identity is missing"))?, o.access_token.clone())
            }
            _ => return Err(anyhow!("Subscription credentials changed; reload accounts")),
        }
    };
    Ok(HttpProvider {
        provider: acct.provider,
        client: app.http.for_reset(acct.proxy_url.as_deref()),
        token,
        account_id: id,
        #[cfg(test)]
        origin: app.reset_test_origin.lock().clone(),
    })
}
fn check_current(app: &App, acct: &Arc<Account>, id: &str, spending: bool) -> Result<()> {
    ensure!(
        app.pool.get(&acct.id).is_some_and(|current| Arc::ptr_eq(&current, acct)),
        "Account changed; reload accounts"
    );
    ensure!(account_id(acct)? == id, "Account identity changed; refresh before continuing");
    ensure!(!spending || !acct.state.lock().disabled, "Enable this account before applying a reset");
    Ok(())
}
fn scope(provider: Provider, organization: &str) -> String {
    format!("{}:{organization}", provider.as_str())
}
fn store(app: &App, acct: &Account, account: &str, view: &View) {
    for other in app.pool.all() {
        if other.provider == acct.provider && account_id(&other).ok().as_deref() == Some(account) {
            other.state.lock().banked_resets = Some(view.clone());
        }
    }
    app.broadcast("accounts", Value::Null);
}
fn view(ledger: &Ledger, inventory: Option<Inventory>, error: Option<String>, quote: Option<String>) -> View {
    let operation = ledger.journal.latest().cloned();
    let retryable = operation.as_ref().is_some_and(Operation::retryable);
    View {
        checked_at: Utc::now(),
        inventory,
        error,
        quote,
        operation: operation.as_ref().map(OperationView::from),
        retryable,
    }
}

pub async fn refresh(app: &Arc<App>, acct: &Arc<Account>) -> Result<View> {
    match refresh_inner(app, acct).await {
        Ok(view) => Ok(view),
        Err(e) => {
            let mut st = acct.state.lock();
            let mut view = st.banked_resets.clone().unwrap_or(View {
                checked_at: Utc::now(),
                inventory: None,
                error: None,
                quote: None,
                operation: None,
                retryable: false,
            });
            if view.operation.is_none()
                && let Ok(account) = account_id(acct)
                && let Ok(Some(operation)) =
                    ledger::last_operation(&app.startup_config.auth_dir(), acct.provider, &account)
            {
                view.operation = Some(OperationView::from(&operation));
            }
            view.checked_at = Utc::now();
            view.error = Some(e.to_string());
            view.quote = None;
            view.retryable = acct.provider == Provider::Claude
                && view
                    .operation
                    .as_ref()
                    .is_some_and(|o| matches!(o.status.as_str(), "pending" | "unknown") && Utc::now() < o.retry_until);
            st.banked_resets = Some(view.clone());
            drop(st);
            app.broadcast("accounts", Value::Null);
            Ok(view)
        }
    }
}
async fn refresh_inner(app: &Arc<App>, acct: &Arc<Account>) -> Result<View> {
    let api = api(app, acct).await?;
    let organization = api.identity().await?;
    let ledger = Ledger::open(&app.startup_config.auth_dir(), &scope(acct.provider, &organization))?;
    let epoch = acct.quota_epoch();
    let (inventory, usage) = match api.read().await {
        Ok(read) => read,
        Err(e) => {
            let mut view = view(&ledger, None, Some(e.to_string()), None);
            view.retryable &= acct.provider == Provider::Claude;
            store(app, acct, &api.account_id, &view);
            return Ok(view);
        }
    };
    check_current(app, acct, &api.account_id, false)?;
    let can_update = {
        let st = acct.state.lock();
        st.quota_epoch == epoch && !st.quota_refreshing
    };
    if can_update {
        reconcile(app, acct, &api.account_id, &usage);
    }
    let quote = if ledger.journal.unresolved().is_none() && inventory.eligible && !acct.state.lock().disabled {
        let request = uuid::Uuid::new_v4().to_string();
        let mut quotes = app.reset_quotes.lock();
        quotes.retain(|_, q| q.expires > Utc::now());
        ensure!(quotes.len() < 10000, "Too many reset confirmations; try again shortly");
        quotes.insert(
            request.clone(),
            Quote {
                account: api.account_id.clone(),
                identity: scope(acct.provider, &organization),
                version: ledger.journal.operations.len(),
                fingerprint: fingerprint(&inventory),
                expires: Utc::now() + chrono::Duration::seconds(120),
            },
        );
        Some(request)
    } else {
        None
    };
    let view = view(&ledger, Some(inventory), None, quote);
    let mut view = view;
    view.retryable &= acct.provider == Provider::Claude;
    store(app, acct, &api.account_id, &view);
    Ok(view)
}

/// The management handler spawns this future so closing the browser cannot cancel a spend.
pub async fn apply(app: Arc<App>, acct: Arc<Account>, action: Action) -> Result<View> {
    ensure!(action.confirmed, "Explicit confirmation is required");
    ensure!(uuid::Uuid::parse_str(&action.request_id).is_ok(), "Invalid reset request ID");
    ensure!(
        matches!(action.action.as_str(), "redeem" | "retry" | "resolve-used" | "resolve-unused"),
        "Unknown reset action"
    );
    let api = api(&app, &acct).await?;
    let organization = api.identity().await?;
    let mut ledger = Ledger::open(&app.startup_config.auth_dir(), &scope(acct.provider, &organization))?;
    execute(&app, &acct, &api, &organization, &api.account_id, &mut ledger, action).await
}

async fn execute<P: Api + Sync>(
    app: &App,
    acct: &Arc<Account>,
    api: &P,
    organization: &str,
    account: &str,
    ledger: &mut Ledger,
    action: Action,
) -> Result<View> {
    ensure!(action.confirmed, "Explicit confirmation is required");
    let existing = ledger.journal.operations.get(&action.request_id).cloned();
    if let Some(operation) = &existing {
        ensure!(operation.account == account, "This reset belongs to a different subscription account");
        if !operation.unsettled() || action.action == "redeem" {
            let mut view = view(ledger, None, None, None);
            if ledger.journal.unresolved().is_none() {
                view.operation = Some(OperationView::from(operation));
            }
            view.retryable &= acct.provider == Provider::Claude;
            store(app, acct, account, &view);
            return Ok(view);
        }
    }
    let retry = action.action == "retry";
    ensure!(
        !retry || acct.provider == Provider::Claude,
        "Codex retry guarantees are unknown; verify and reconcile the outcome instead"
    );
    let resolving = action.action.starts_with("resolve-");
    let mut operation = if retry || resolving {
        let operation = existing.ok_or_else(|| anyhow!("Unknown reset request; refresh status"))?;
        ensure!(operation.unsettled(), "Reset request is already settled");
        ensure!(
            resolving || operation.retryable(),
            "Retry window ended; verify the outcome with the provider and reconcile it"
        );
        operation
    } else {
        ensure!(
            ledger.journal.unresolved().is_none(),
            "An earlier reset has an unknown outcome; resolve it before spending another"
        );
        ensure!(ledger.journal.operations.len() < 10000, "Reset journal is full; operator review required");
        let (inventory, _) = api.read().await?;
        let quotes = app.reset_quotes.lock();
        let quote = quotes
            .get(&action.request_id)
            .ok_or_else(|| anyhow!("Confirmation expired; refresh resets and confirm again"))?;
        ensure!(
            quote.expires > Utc::now()
                && quote.account == account
                && quote.identity == scope(acct.provider, organization)
                && quote.version == ledger.journal.operations.len()
                && quote.fingerprint == fingerprint(&inventory),
            "Reset availability changed; refresh resets and confirm again"
        );
        ensure!(inventory.eligible, "No reset can be applied right now");
        let clears = if acct.provider == Provider::Claude {
            let grant = inventory
                .grants
                .iter()
                .find(|g| g.id == action.grant_id && g.usable)
                .ok_or_else(|| anyhow!("Selected grant is no longer usable"))?;
            grant.clears.clone()
        } else {
            ensure!(action.grant_id.is_empty(), "Codex selects the reset grant automatically");
            // Codex resets its subscription windows; preserve model-scoped quotas.
            vec!["*".into()]
        };
        Operation {
            request_id: action.request_id.clone(),
            account: account.into(),
            provider: acct.provider.as_str().into(),
            grant_id: action.grant_id,
            clears,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status: "pending".into(),
            message: "Reset request in progress".into(),
        }
    };
    check_current(app, acct, account, !resolving)?;
    if resolving {
        operation.status = if action.action == "resolve-used" { "reconciled_used" } else { "reconciled_unused" }.into();
        operation.message = if action.action == "resolve-used" {
            "Operator verified the reset was used"
        } else {
            "Operator verified no reset was used"
        }
        .into();
    } else {
        // This fsync precedes the only spending call. A crash leaves an unresolved entry.
        operation.status = "pending".into();
        operation.updated_at = Utc::now();
        ledger.journal.operations.insert(operation.request_id.clone(), operation.clone());
        ledger.save()?;
        let mut pending = view(ledger, None, None, None);
        pending.retryable = false;
        store(app, acct, account, &pending);
        let guard = QuotaGuard::new(app, acct, account);
        let outcome =
            api.redeem(organization, &operation.grant_id, &operation.request_id).await.unwrap_or(Outcome::Unknown);
        match outcome {
            Outcome::Applied => {
                operation.status = "applied".into();
                operation.message = "One banked reset applied".into();
                guard.invalidate(&operation.clears);
            }
            Outcome::AlreadyUsed => {
                operation.status = "already_used".into();
                operation.message = "Provider reports this reset was already used".into();
            }
            Outcome::Refused(reason) if !retry => {
                operation.status = "refused".into();
                operation.message = reason.into();
            }
            _ => {
                operation.status = "unknown".into();
                operation.message =
                    "Outcome unknown. A reset may have been spent; verify it with the provider before spending another"
                        .into();
            }
        }
        operation.updated_at = Utc::now();
        ledger.journal.operations.insert(operation.request_id.clone(), operation.clone());
        // If the terminal save fails, the on-disk pending entry still blocks a new spend.
        ledger.save()?;
        let read = api.read().await;
        drop(guard);
        let (inventory, error) = match read {
            Ok((inventory, usage)) => {
                reconcile(app, acct, account, &usage);
                (Some(inventory), None)
            }
            Err(e) => (None, Some(format!("Reset result saved; quota refresh failed: {e}"))),
        };
        let mut view = view(ledger, inventory, error, None);
        view.retryable &= acct.provider == Provider::Claude;
        store(app, acct, account, &view);
        return Ok(view);
    }
    operation.updated_at = Utc::now();
    ledger.journal.operations.insert(operation.request_id.clone(), operation);
    ledger.save()?;
    let guard = QuotaGuard::new(app, acct, account);
    let result = api.read().await;
    drop(guard);
    let (inventory, error) = match result {
        Ok((inventory, usage)) => {
            reconcile(app, acct, account, &usage);
            (Some(inventory), None)
        }
        Err(e) => (None, Some(format!("Reconciliation saved; quota refresh failed: {e}"))),
    };
    let mut view = view(ledger, inventory, error, None);
    view.retryable &= acct.provider == Provider::Claude;
    store(app, acct, account, &view);
    Ok(view)
}
fn reconcile(app: &App, acct: &Account, account: &str, usage: &Value) {
    for other in app.pool.all() {
        if other.provider == acct.provider && account_id(&other).ok().as_deref() == Some(account) {
            let mut st = other.state.lock();
            st.quota_epoch += 1;
            crate::quota::usage(&mut st, acct.provider, usage);
        }
    }
}

/// Increment epochs before and after the claim so older polls/responses cannot restore stale quota.
struct QuotaGuard {
    accounts: Vec<Arc<Account>>,
    account: String,
}
impl QuotaGuard {
    fn new(app: &App, acct: &Account, account: &str) -> Self {
        let accounts: Vec<_> = app
            .pool
            .all()
            .into_iter()
            .filter(|a| a.provider == acct.provider && account_id(a).ok().as_deref() == Some(account))
            .collect();
        for a in &accounts {
            let mut st = a.state.lock();
            st.quota_epoch += 1;
            st.quota_refreshing = true;
        }
        Self { accounts, account: account.into() }
    }
    fn invalidate(&self, clears: &[String]) {
        for a in &self.accounts {
            if account_id(a).ok().as_deref() != Some(self.account.as_str()) {
                continue;
            }
            let mut st = a.state.lock();
            let affected = |w: &crate::quota::Window| {
                clears.contains(&w.name) || (clears.iter().any(|s| s == "*") && w.model.is_none())
            };
            let quota = &st.quota;
            let removable: Vec<_> = st
                .quota_cooldowns
                .keys()
                .filter(|model| {
                    let exhausted: Vec<_> = quota
                        .windows
                        .iter()
                        .filter(|w| w.used >= 100.0 && w.model.as_ref().is_none_or(|m| model.contains(m)))
                        .collect();
                    !exhausted.is_empty() && exhausted.iter().all(|w| affected(w))
                })
                .cloned()
                .collect();
            for model in removable {
                st.quota_cooldowns.remove(&model);
            }
            st.quota.windows.retain(|w| !affected(w));
            st.quota.updated_at = None;
            // Keep cooldowns whose scope cannot be proven to match this grant.
        }
    }
}
impl Drop for QuotaGuard {
    fn drop(&mut self) {
        for a in &self.accounts {
            if account_id(a).ok().as_deref() != Some(self.account.as_str()) {
                continue;
            }
            let mut st = a.state.lock();
            st.quota_epoch += 1;
            st.quota_refreshing = false;
        }
    }
}
