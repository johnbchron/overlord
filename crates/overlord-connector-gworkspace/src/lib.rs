//! The Google Workspace connector: read-only, Admin SDK Directory API.
//!
//! PLAN.md M4 puts Workspace first so the overlay vocabulary (SPEC.md
//! section 11) is shaped by workspace semantics and the identity
//! provider normalizes onto it rather than the other way round.
//!
//! # What it reads
//!
//! Per sweep, for one configured tenant:
//!
//! - **users** — every account in the customer, `projection=full`, which is
//!   what carries 2-step verification, external ids, organizations and aliases;
//! - **domains** — the customer's verified domains, which is how an external
//!   group member is told from an internal one;
//! - **groups and their members** — inverted into per-account membership, so
//!   `count(groups where external) > 0` is a question the overlay can answer;
//! - **licence assignments** — only for the SKUs an operator names, because the
//!   Licensing API is enumerated per SKU and there is no "all of them" call.
//!
//! # Minimum scopes
//!
//! Granted to the service account's client id in the Admin console,
//! under Security → Access and data control → API controls → Domain-wide
//! delegation. These are the whole of what overlord can do to a tenant:
//!
//! | scope | why | read-only? |
//! | --- | --- | --- |
//! | `.../auth/admin.directory.user.readonly` | enumerate accounts | yes |
//! | `.../auth/admin.directory.group.readonly` | groups and members | yes |
//! | `.../auth/admin.directory.domain.readonly` | verified domains | yes |
//! | `.../auth/apps.licensing` | licence assignments | **no** |
//!
//! `apps.licensing` is the exception SPEC.md section 11 asks every
//! connector to declare: Google publishes no read-only variant of it, so
//! reading which accounts hold which licence requires a scope that can
//! also assign and revoke them. overlord never does — [`ReadMethod`]
//! cannot name a mutating method and the allowlist below permits exactly
//! one licensing path — but the grant is wider than the use, and an
//! operator should know that before making it. It is requested **only**
//! when `licenses` is configured; leave that empty and the scope is
//! neither needed nor asked for.
//!
//! # What it does not read
//!
//! **Drive sharing settings.** SPEC.md section 11 lists sharing settings
//! among a workspace connector's subjects, and the Directory API does
//! not expose them: domain-level sharing lives in the Admin console,
//! whose API was retired, and per-account sharing behaviour is a Drive
//! API question needing a Drive scope over every user's content. That is
//! a much larger grant than anything above, and it belongs to a separate
//! connector an operator can decline. Nothing here approximates it: a
//! check written against a guessed `external_sharing` field would be
//! confidently wrong, which is worse than absent.
//!
//! **Nested groups.** A group that is a member of another group is
//! reported as a member of type `GROUP` and not expanded, because
//! attributing the outer group's membership to the inner group's people
//! is a claim the directory did not make.
//!
//! # The shape of an observation
//!
//! A user's facts come from up to three APIs, so the raw payload is an
//! envelope of verbatim vendor objects rather than one response body:
//!
//! ```json
//! {
//!   "user":          { "...": "the Directory user resource, verbatim" },
//!   "external_ids":  { "organization": "E-4471" },
//!   "organization":  { "department": "Platform", "primary": true },
//!   "groups":        [ { "email": "eng@…", "external": false } ],
//!   "licenses":      [ { "skuId": "1010020020" } ]
//! }
//! ```
//!
//! `raw.user.<anything>` therefore reaches the vendor payload exactly as
//! Google sent it. `external_ids` and `organization` are indexed views
//! of two repeated Directory fields, and they exist because
//! `Value::get_path` refuses to index a list by number — vendor array
//! order is not stable, so `externalIds.0.value` would be a check that
//! quietly changes its mind. Keying by Google's own `type`, and picking
//! the organization Google marked `primary`, says what was meant.
//!
//! `groups` and `licenses` are **absent** when they were not collected,
//! and an empty list when they were collected and there were none. The
//! difference matters: null is not "no groups".
//!
//! [`ReadMethod`]: overlord_connect::ReadMethod

pub mod auth;
pub mod directory;

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use overlord_connect::{
  Allow, Connector, ConnectorError, Observation, ObserveCtx, RestrictedHttp,
  Ruleset, Snapshot,
};
use overlord_core::{Completeness, SystemKind};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::{
  auth::{Credential, TOKEN_BASE, TOKEN_PATH},
  directory::{DIRECTORY_BASE, LICENSING_BASE, Paged},
};

const SCOPE_USERS: &str =
  "https://www.googleapis.com/auth/admin.directory.user.readonly";
const SCOPE_GROUPS: &str =
  "https://www.googleapis.com/auth/admin.directory.group.readonly";
const SCOPE_DOMAINS: &str =
  "https://www.googleapis.com/auth/admin.directory.domain.readonly";
/// No read-only variant exists. Requested only when licences are
/// configured; see the crate docs.
const SCOPE_LICENSING: &str = "https://www.googleapis.com/auth/apps.licensing";

/// One licence SKU to enumerate assignments for.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sku {
  /// Google's product id, e.g. `Google-Apps`.
  pub product: String,
  /// Google's SKU id, e.g. `1010020020` (Workspace Business Standard).
  pub sku:     String,
}

/// Per-system configuration, from the `[systems.config]` table.
///
/// Credentials are conspicuously absent: they come from the environment
/// named by `credentials_env` and nowhere else (SPEC.md section 14).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  /// `my_customer` resolves to the customer the impersonated
  /// administrator belongs to, which is what a single-tenant deployment
  /// wants. A reseller managing several names the customer id.
  pub customer:        String,
  /// The administrator a service account borrows authority from.
  /// Required for a service account: domain-wide delegation has no
  /// meaning without a subject.
  pub impersonate:     Option<String>,
  /// The environment variable holding the Google credential file, or a
  /// path to it.
  pub credentials_env: String,
  /// Collect group membership. On by default — it is one API call per
  /// group, and the alternative is an overlay that cannot answer a
  /// sharing question at all.
  pub groups:          bool,
  /// SKUs to enumerate licence assignments for. Empty by default, which
  /// is also what keeps the one non-read-only scope unrequested.
  pub licenses:        Vec<Sku>,
  /// The customer id the Licensing API wants. Taken from the accounts
  /// themselves when absent, which is where Google also puts it.
  pub customer_id:     Option<String>,
  /// Send Directory requests somewhere other than Google — an egress
  /// proxy, or a test double. Changes where, never what: the allowlist
  /// is unaffected.
  pub base_url:        Option<String>,
  /// As `base_url`, for the token endpoint.
  pub token_url:       Option<String>,
  /// As `base_url`, for the Licensing API.
  pub licensing_url:   Option<String>,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      customer:        "my_customer".to_owned(),
      impersonate:     None,
      credentials_env: "OVERLORD_GWS_CREDENTIALS".to_owned(),
      groups:          true,
      licenses:        Vec::new(),
      customer_id:     None,
      base_url:        None,
      token_url:       None,
      licensing_url:   None,
    }
  }
}

impl Config {
  fn read(ctx: &ObserveCtx) -> Result<Self, ConnectorError> {
    if ctx.config.is_null() {
      return Ok(Self::default());
    }
    serde_json::from_value(ctx.config.clone())
      .map_err(|e| ConnectorError::Config(format!("google-workspace: {e}")))
  }

  fn scopes(&self) -> Vec<&'static str> {
    let mut scopes = vec![SCOPE_USERS, SCOPE_DOMAINS];
    if self.groups {
      scopes.push(SCOPE_GROUPS);
    }
    if !self.licenses.is_empty() {
      scopes.push(SCOPE_LICENSING);
    }
    scopes
  }
}

#[derive(Debug, Default)]
pub struct GoogleWorkspaceConnector {
  /// Set only by [`Self::with_credential`]; see that constructor.
  credential: Option<Credential>,
}

impl GoogleWorkspaceConnector {
  /// The connector as the binary registers it: credentials come from
  /// the environment variable the system's configuration names, and
  /// from nowhere else (SPEC.md section 14).
  #[must_use]
  pub fn new() -> Self { Self { credential: None } }

  /// The connector with a credential supplied directly, for tests.
  ///
  /// It exists because a test cannot set an environment variable at
  /// all: `std::env::set_var` is unsafe in this edition and
  /// `unsafe_code` is forbidden across the workspace. No configuration
  /// path reaches this — a `[[systems]]` entry names a connector, and
  /// the binary's registry calls [`Self::new`] — so the environment
  /// remains the only way a credential gets into a real deployment.
  #[must_use]
  pub fn with_credential(credential: Credential) -> Self {
    Self {
      credential: Some(credential),
    }
  }

  #[must_use]
  pub fn boxed() -> Box<dyn Connector> { Box::new(Self::new()) }

  /// The tenant's credential, from the environment unless a test put
  /// one here.
  fn credential(&self, cfg: &Config) -> Result<Credential, ConnectorError> {
    match &self.credential {
      Some(c) => Ok(c.clone()),
      None => Credential::from_env(&cfg.credentials_env),
    }
  }
}

#[async_trait]
impl Connector for GoogleWorkspaceConnector {
  fn name(&self) -> &'static str { "google-workspace" }

  fn system_kind(&self) -> SystemKind { SystemKind::Workspace }

  /// Six endpoints, on three origins, each with the reason it is
  /// needed. This is the whole of what the connector can reach.
  fn allowlist(&self) -> Vec<Allow> {
    vec![
      Allow::get(directory::USERS_PATH, "enumerate accounts"),
      Allow::get(directory::GROUPS_PATH, "enumerate groups"),
      Allow::get(
        &format!("{}/*/members", directory::GROUPS_PATH),
        "group membership, for sharing and role checks",
      ),
      Allow::get(
        "/admin/directory/v1/customer/*/domains",
        "verified domains, to tell an external member from an internal one",
      ),
      Allow::get(
        "/apps/licensing/v1/product/*/sku/*/users",
        "licence assignments for the configured SKUs",
      )
      .at(LICENSING_BASE),
      // A token grant is a POST that reads: it exchanges a credential
      // for a token and changes nothing in the tenant.
      Allow::post(TOKEN_PATH, "exchange a credential for a read token")
        .at(TOKEN_BASE),
    ]
  }

  fn base_url(&self, ctx: &ObserveCtx) -> String {
    Config::read(ctx)
      .ok()
      .and_then(|c| c.base_url)
      .unwrap_or_else(|| DIRECTORY_BASE.to_owned())
  }

  fn default_ruleset(&self, _: &ObserveCtx) -> Ruleset {
    serde_json::from_str(include_str!("ruleset.json"))
      .expect("the shipped Google Workspace ruleset must parse")
  }

  async fn observe(
    &self,
    http: &RestrictedHttp,
    ctx: &ObserveCtx,
  ) -> Result<Snapshot, ConnectorError> {
    let cfg = Config::read(ctx)?;
    let credential = self.credential(&cfg)?;

    if credential.needs_impersonation() && cfg.impersonate.is_none() {
      return Err(ConnectorError::Config(
        "a service account reads a Workspace tenant by domain-wide \
         delegation, which has no meaning without a subject: set \
         `impersonate` to an administrator's address"
          .to_owned(),
      ));
    }

    let token = auth::access_token(
      &self.http_for(TOKEN_BASE, cfg.token_url.as_deref())?,
      &credential,
      cfg.impersonate.as_deref(),
      &cfg.scopes(),
      ctx.started_at,
    )
    .await?;
    http.set_bearer(token.clone());

    let mut warnings = Vec::new();
    let mut incomplete: Vec<String> = Vec::new();

    // Accounts. An enumeration that produced nothing at all is a failed
    // system rather than a tenant that lost everybody.
    let users = directory::users(http, &cfg.customer).await;
    directory::require_something(&users, "accounts")?;
    if let Some(reason) = &users.incomplete {
      incomplete.push(reason.clone());
    }
    info!(
      system = %ctx.system,
      accounts = users.items.len(),
      "directory read"
    );

    // Verified domains. Without them nothing can be called external, so
    // a failure here degrades the snapshot rather than silently
    // reporting every group as internal.
    let domains = directory::domains(http, &cfg.customer).await;
    if let Some(reason) = &domains.incomplete {
      incomplete.push(format!(
        "{reason}; group membership cannot be classified as external without \
         the customer's verified domains"
      ));
    }
    let domains = domain_names(&domains);

    let (memberships, group_problems) = if cfg.groups {
      collect_groups(http, &cfg, &domains).await
    } else {
      (None, Vec::new())
    };
    incomplete.extend(group_problems);

    let customer_id = cfg
      .customer_id
      .clone()
      .or_else(|| first_customer_id(&users.items));
    let (licences, licence_problems) =
      collect_licences(self, &cfg, customer_id.as_deref(), &token).await?;
    incomplete.extend(licence_problems);

    let observations = users
      .items
      .iter()
      .map(|user| {
        Observation::new(envelope(
          user,
          memberships.as_ref(),
          licences.as_ref(),
        ))
      })
      .collect();

    warnings.extend(incomplete.iter().cloned());
    Ok(Snapshot {
      completeness: if incomplete.is_empty() {
        Completeness::Complete
      } else {
        // SPEC.md section 10: a partial snapshot never tombstones, and
        // the violations it touches are marked stale. That is the right
        // answer for a half-read overlay as well as a half-read
        // enumeration — a group read that failed would otherwise close
        // every sharing violation in the tenant.
        Completeness::Partial {
          reason: incomplete.join("; "),
        }
      },
      observations,
      warnings,
    })
  }
}

/// Build the raw payload for one account.
fn envelope(
  user: &Value,
  memberships: Option<&BTreeMap<String, Vec<Value>>>,
  licences: Option<&BTreeMap<String, Vec<Value>>>,
) -> Value {
  let mut out = serde_json::Map::new();
  out.insert("user".to_owned(), user.clone());

  let ids = external_ids(user);
  if !ids.is_empty() {
    out.insert("external_ids".to_owned(), Value::Object(ids));
  }
  if let Some(org) = primary_organization(user) {
    out.insert("organization".to_owned(), org);
  }

  // Absent when not collected, empty when collected and empty: null is
  // not "no groups".
  if let Some(by_user) = memberships {
    let id = user.get("id").and_then(Value::as_str).unwrap_or_default();
    out.insert(
      "groups".to_owned(),
      Value::Array(by_user.get(id).cloned().unwrap_or_default()),
    );
  }
  if let Some(by_email) = licences {
    let email = user
      .get("primaryEmail")
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_lowercase();
    out.insert(
      "licenses".to_owned(),
      Value::Array(by_email.get(&email).cloned().unwrap_or_default()),
    );
  }

  Value::Object(out)
}

/// Google's repeated `externalIds`, keyed by the type it gave each one.
///
/// A custom type is keyed by its `customType`, which is the name the
/// operator chose in the Admin console, so a ruleset can name it.
fn external_ids(user: &Value) -> serde_json::Map<String, Value> {
  let mut out = serde_json::Map::new();
  let Some(Value::Array(ids)) = user.get("externalIds") else {
    return out;
  };
  for id in ids {
    let Some(value) = id.get("value").and_then(Value::as_str) else {
      continue;
    };
    let kind = match id.get("type").and_then(Value::as_str) {
      Some("custom") => id.get("customType").and_then(Value::as_str),
      other => other,
    };
    if let Some(kind) = kind.filter(|k| !k.is_empty()) {
      // First writer wins: Google lists at most one per type, and a
      // duplicate is a tenant's data problem, not something to guess at.
      out
        .entry(kind.to_owned())
        .or_insert_with(|| Value::String(value.to_owned()));
    }
  }
  out
}

/// The organization Google marked primary, or the only one, or none.
fn primary_organization(user: &Value) -> Option<Value> {
  let Some(Value::Array(orgs)) = user.get("organizations") else {
    return None;
  };
  orgs
    .iter()
    .find(|o| o.get("primary").and_then(Value::as_bool) == Some(true))
    .or_else(|| orgs.first())
    .cloned()
}

fn domain_names(domains: &Paged) -> BTreeSet<String> {
  domains
    .items
    .iter()
    .filter_map(|d| d.get("domainName").and_then(Value::as_str))
    .map(str::to_lowercase)
    .collect()
}

/// The customer id Google stamps on every account, for the Licensing
/// API's benefit.
fn first_customer_id(users: &[Value]) -> Option<String> {
  users
    .iter()
    .find_map(|u| u.get("customerId").and_then(Value::as_str))
    .map(ToOwned::to_owned)
}

/// Enumerate groups and invert their membership onto accounts.
///
/// Returns `None` for the membership map when groups were not
/// collected at all, so [`envelope`] can tell "not collected" from
/// "collected and empty".
async fn collect_groups(
  http: &RestrictedHttp,
  cfg: &Config,
  domains: &BTreeSet<String>,
) -> (Option<BTreeMap<String, Vec<Value>>>, Vec<String>) {
  let mut problems = Vec::new();
  let groups = directory::groups(http, &cfg.customer).await;
  if let Some(reason) = &groups.incomplete {
    problems.push(reason.clone());
    if groups.is_empty() {
      // Nothing was read, so there is no membership to report. Saying
      // "no groups" here would resolve every sharing violation at once.
      return (None, problems);
    }
  }

  let mut by_user: BTreeMap<String, Vec<Value>> = BTreeMap::new();
  for group in &groups.items {
    let Some(key) = group.get("id").and_then(Value::as_str) else {
      continue;
    };
    let members = directory::members(http, key).await;
    if let Some(reason) = &members.incomplete {
      problems.push(reason.clone());
    }

    let external = members.items.iter().any(|m| is_external(m, domains));
    for member in &members.items {
      if member.get("type").and_then(Value::as_str) != Some("USER") {
        continue;
      }
      let Some(id) = member.get("id").and_then(Value::as_str) else {
        continue;
      };
      by_user.entry(id.to_owned()).or_default().push(json!({
        "id":          group.get("id"),
        "email":       group.get("email"),
        "name":        group.get("name"),
        "description": group.get("description"),
        "external":    external,
        "member_role": member.get("role"),
      }));
    }
  }

  (Some(by_user), problems)
}

/// Whether a group member is outside the customer's verified domains.
fn is_external(member: &Value, domains: &BTreeSet<String>) -> bool {
  // The whole customer as a member is the customer, by definition.
  if member.get("type").and_then(Value::as_str) == Some("CUSTOMER") {
    return false;
  }
  let Some(email) = member.get("email").and_then(Value::as_str) else {
    return false;
  };
  match email.rsplit_once('@') {
    Some((_, domain)) => !domains.contains(&domain.to_lowercase()),
    None => false,
  }
}

/// Licence assignments for each configured SKU, keyed by the account's
/// address — which is what the Licensing API returns as `userId`.
async fn collect_licences(
  connector: &GoogleWorkspaceConnector,
  cfg: &Config,
  customer_id: Option<&str>,
  token: &str,
) -> Result<(Option<BTreeMap<String, Vec<Value>>>, Vec<String>), ConnectorError>
{
  if cfg.licenses.is_empty() {
    return Ok((None, Vec::new()));
  }
  let Some(customer_id) = customer_id else {
    return Ok((None, vec![
      "licences were requested but no customer id could be found; set \
       `customer_id`"
        .to_owned(),
    ]));
  };

  // A second origin needs its own client, and therefore its own copy of
  // the token the sweep already obtained. The scopes it was minted with
  // include licensing whenever this function runs at all.
  let http =
    connector.http_for(LICENSING_BASE, cfg.licensing_url.as_deref())?;
  http.set_bearer(token);

  let mut problems = Vec::new();
  let mut by_email: BTreeMap<String, Vec<Value>> = BTreeMap::new();
  for sku in &cfg.licenses {
    let page = directory::licence_assignments(
      &http,
      &sku.product,
      &sku.sku,
      customer_id,
    )
    .await;
    if let Some(reason) = &page.incomplete {
      problems.push(reason.clone());
    }
    for item in &page.items {
      let Some(user) = item.get("userId").and_then(Value::as_str) else {
        continue;
      };
      by_email
        .entry(user.to_lowercase())
        .or_default()
        .push(item.clone());
    }
  }

  Ok((Some(by_email), problems))
}

#[cfg(test)]
mod tests {
  use overlord_connect::ReadMethod;
  use overlord_core::{SystemId, Timestamp, Value as CoreValue};

  use super::*;

  fn ctx(config: Value) -> ObserveCtx {
    ObserveCtx {
      system: SystemId::new("gws-prod"),
      started_at: "2026-01-15T00:00:00Z".parse::<Timestamp>().unwrap(),
      config,
    }
  }

  #[test]
  fn the_shipped_ruleset_parses_and_names_every_identity_signal() {
    // PLAN.md M4's handoff: identity runs off the overlay, so a
    // connector that maps none of these can only be matched by its key
    // — and this connector's key is an opaque Google id.
    let rs = GoogleWorkspaceConnector::new().default_ruleset(&ctx(Value::Null));
    assert!(rs.fields.contains_key("email"));
    assert!(rs.fields.contains_key("username"));
    assert!(rs.fields.contains_key("employee_id"));
  }

  #[test]
  fn the_stable_key_is_googles_immutable_id_not_the_address() {
    // A rename would otherwise tombstone the account and open a new
    // one, losing every episode filed against it.
    let rs = GoogleWorkspaceConnector::new().default_ruleset(&ctx(Value::Null));
    assert_eq!(rs.entity_key.path, "user.id");
  }

  #[test]
  fn the_ruleset_normalizes_a_directory_payload() {
    let rs = GoogleWorkspaceConnector::new().default_ruleset(&ctx(Value::Null));
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &envelope(
          &json!({
            "id": "1001",
            "primaryEmail": "Ada.Lovelace@example.com",
            "name": { "fullName": "Ada Lovelace" },
            "suspended": false,
            "archived": false,
            "isAdmin": true,
            "isEnrolledIn2Sv": false,
            "lastLoginTime": "2026-01-02T03:04:05.000Z",
            "aliases": ["ada@example.com"],
            "externalIds": [{ "value": "E-4471", "type": "organization" }],
            "organizations": [
              { "department": "Sales", "primary": false },
              { "department": "Platform", "primary": true }
            ]
          }),
          None,
          None,
        ),
      )
      .unwrap();
    assert!(n.warnings.is_empty(), "{:?}", n.warnings);
    let r = &n.record;
    assert_eq!(r.entity_key.as_str(), "1001");
    assert_eq!(r.display_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(r.status, overlord_core::EntityStatus::Active);
    assert_eq!(r.get("mfa_enrolled"), CoreValue::Bool(false));
    assert_eq!(r.get("is_admin"), CoreValue::Bool(true));
    assert_eq!(r.get("last_login_at").type_name(), "timestamp");
    assert_eq!(
      r.get("email"),
      CoreValue::String("Ada.Lovelace@example.com".to_owned())
    );
    assert_eq!(
      r.get("username"),
      CoreValue::String("Ada.Lovelace".to_owned())
    );
    assert_eq!(r.get("employee_id"), CoreValue::String("E-4471".to_owned()));
    // The organization Google marked primary, not the first one.
    assert_eq!(
      r.get("department"),
      CoreValue::String("Platform".to_owned())
    );
    // Not collected is null, and null is not "no groups".
    assert_eq!(r.get("groups"), CoreValue::Null);
  }

  #[test]
  fn an_archived_account_is_deprovisioned_not_active() {
    let rs = GoogleWorkspaceConnector::new().default_ruleset(&ctx(Value::Null));
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &envelope(
          &json!({
            "id": "1002",
            "primaryEmail": "grace@example.com",
            "archived": true,
            "suspended": false
          }),
          None,
          None,
        ),
      )
      .unwrap();
    assert_eq!(n.record.status, overlord_core::EntityStatus::Deprovisioned);
  }

  #[test]
  fn a_user_who_has_never_signed_in_has_no_last_login_rather_than_1970() {
    let rs = GoogleWorkspaceConnector::new().default_ruleset(&ctx(Value::Null));
    let n = rs
      .apply(
        &SystemId::new("gws-prod"),
        &envelope(
          &json!({
            "id": "1003",
            "primaryEmail": "new@example.com",
            "suspended": false,
            "archived": false,
            "lastLoginTime": "1970-01-01T00:00:00.000Z"
          }),
          None,
          None,
        ),
      )
      .unwrap();
    assert_eq!(n.record.get("last_login_at"), CoreValue::Null);
  }

  #[test]
  fn a_custom_external_id_is_keyed_by_the_name_the_operator_chose() {
    let ids = external_ids(&json!({
      "externalIds": [
        { "value": "E-1", "type": "organization" },
        { "value": "badge-9", "type": "custom", "customType": "badge" }
      ]
    }));
    assert_eq!(ids["organization"], json!("E-1"));
    assert_eq!(ids["badge"], json!("badge-9"));
  }

  #[test]
  fn a_member_outside_the_verified_domains_is_external() {
    let domains: BTreeSet<String> =
      ["example.com".to_owned(), "example.net".to_owned()]
        .into_iter()
        .collect();
    assert!(is_external(
      &json!({ "type": "USER", "email": "auditor@partner.example" }),
      &domains
    ));
    assert!(!is_external(
      &json!({ "type": "USER", "email": "Ada@Example.COM" }),
      &domains
    ));
    // The customer as a whole is not an outsider to itself.
    assert!(!is_external(
      &json!({ "type": "CUSTOMER", "email": "anything@elsewhere.test" }),
      &domains
    ));
  }

  #[test]
  fn the_allowlist_is_the_whole_of_what_the_connector_can_reach() {
    let c = GoogleWorkspaceConnector::new();
    let http = c.http(&ctx(Value::Null)).unwrap();

    assert!(http.permits(ReadMethod::Get, directory::USERS_PATH));
    assert!(http.permits(
      ReadMethod::Get,
      "/admin/directory/v1/groups/eng@example.com/members"
    ));

    // Everything the Directory API offers that overlord did not ask for.
    assert!(!http.permits(ReadMethod::Get, "/admin/directory/v1/users/1001"));
    assert!(
      !http.permits(ReadMethod::Get, "/admin/directory/v1/users/1001/aliases")
    );
    assert!(!http.permits(ReadMethod::Post, directory::USERS_PATH));
    assert!(!http.permits(ReadMethod::Get, "/admin/reports/v1/activity"));
    // Another origin's entries are not carried here.
    assert!(!http.permits(ReadMethod::Post, TOKEN_PATH));
  }

  #[test]
  fn the_token_client_can_reach_the_token_endpoint_and_nothing_else() {
    let c = GoogleWorkspaceConnector::new();
    let http = c.http_for(TOKEN_BASE, None).unwrap();
    assert!(http.permits(ReadMethod::Post, TOKEN_PATH));
    assert!(!http.permits(ReadMethod::Post, "/revoke"));
    assert!(!http.permits(ReadMethod::Get, directory::USERS_PATH));
  }

  #[test]
  fn the_licensing_scope_is_asked_for_only_when_licences_are() {
    let plain = Config::default();
    assert!(!plain.scopes().contains(&SCOPE_LICENSING));

    let with_licences = Config {
      licenses: vec![Sku {
        product: "Google-Apps".to_owned(),
        sku:     "1010020020".to_owned(),
      }],
      ..Config::default()
    };
    assert!(with_licences.scopes().contains(&SCOPE_LICENSING));
  }

  #[test]
  fn a_misspelled_setting_is_refused_rather_than_ignored() {
    // `deny_unknown_fields`, because a typo that silently disables
    // group collection would look exactly like a tenant with no groups.
    let err = Config::read(&ctx(json!({ "imperson8": "a@b.test" })))
      .unwrap_err()
      .to_string();
    assert!(err.contains("imperson8"), "{err}");
  }

  #[test]
  fn the_default_configuration_reads_one_tenant_with_groups() {
    let cfg = Config::read(&ctx(Value::Null)).unwrap();
    assert_eq!(cfg.customer, "my_customer");
    assert!(cfg.groups);
    assert!(cfg.licenses.is_empty());
    assert_eq!(cfg.credentials_env, "OVERLORD_GWS_CREDENTIALS");
  }
}
