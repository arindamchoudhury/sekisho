use std::collections::HashMap;

use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, check_label, is_foreign_key_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// Whose spend a budget limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetScope {
    /// One person, within one environment or (`None`) across all of them.
    Person {
        person: String,
        environment: Option<EnvName>,
    },
    /// Everyone using the environment, pooled.
    Environment(EnvName),
    Key(EnvKeyId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBudget {
    pub name: String,
    pub scope: BudgetScope,
    pub limit: MicroUsd,
    pub window: BudgetWindow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    pub id: Uuid,
    pub name: String,
    pub scope: BudgetScope,
    pub limit: MicroUsd,
    pub window: BudgetWindow,
    /// Rolling windows count from here.
    pub created_at: OffsetDateTime,
}

/// One budget's total before and after a charge, in the current window.
/// For any threshold `t`, exactly one charge has `before < t <= after`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Charged {
    pub budget: Uuid,
    pub before: MicroUsd,
    pub after: MicroUsd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetStatus {
    pub budget: Budget,
    /// Spend in the current window.
    pub spent: MicroUsd,
    /// The end of the current window.
    pub resets_at: OffsetDateTime,
}

impl BudgetStatus {
    pub fn exhausted(&self) -> bool {
        self.spent >= self.budget.limit
    }
}

/// Expands to the budget query up to its filter.
macro_rules! select_budgets {
    () => {
        "select b.id, b.name, b.scope, e.name as environment, b.key_id, b.person, \
         b.limit_micro_usd, b.window_kind, b.calendar, b.created_at \
         from budgets b left join environments e on e.id = b.environment_id"
    };
}

fn validate(new: &NewBudget) -> Vec<FieldError> {
    let mut errors = Vec::new();
    check_label(&mut errors, "name", &new.name);
    if let BudgetScope::Person { person, .. } = &new.scope
        && person.trim().is_empty()
    {
        errors.push(FieldError::new("person", "must not be empty"));
    }
    if new.limit.0 < 0 {
        errors.push(FieldError::new("limit", "must not be negative"));
    }
    errors
}

fn window_from_row(row: &PgRow) -> Result<BudgetWindow> {
    let kind: String = row.try_get("window_kind")?;
    Ok(BudgetWindow {
        kind: WindowKind::parse(&kind).ok_or_else(|| bad_column("window kind", &kind))?,
        calendar: row.try_get("calendar")?,
    })
}

fn from_row(row: &PgRow) -> Result<Budget> {
    let scope: String = row.try_get("scope")?;
    let environment = || -> Result<Option<EnvName>> {
        row.try_get::<Option<String>, _>("environment")?
            .map(|e| EnvName::parse(&e).map_err(|_| bad_column("environment name", &e)))
            .transpose()
    };
    let scope = match scope.as_str() {
        "environment" => BudgetScope::Environment(
            environment()?.ok_or_else(|| bad_column("budget environment", "null"))?,
        ),
        "key" => {
            let key: Option<String> = row.try_get("key_id")?;
            let key = key.unwrap_or_default();
            BudgetScope::Key(EnvKeyId::parse(&key).ok_or_else(|| bad_column("budget key", &key))?)
        }
        "person" => BudgetScope::Person {
            person: row
                .try_get::<Option<String>, _>("person")?
                .ok_or_else(|| bad_column("budget person", "null"))?,
            environment: environment()?,
        },
        other => return Err(bad_column("budget scope", other)),
    };
    Ok(Budget {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        scope,
        limit: MicroUsd(row.try_get("limit_micro_usd")?),
        window: window_from_row(row)?,
        created_at: row.try_get("created_at")?,
    })
}

/// The environment's id, locked against deletion until the transaction ends.
async fn environment_id(conn: &mut PgConnection, name: &EnvName) -> Result<Uuid> {
    sqlx::query_scalar("select id from environments where name = $1 for key share")
        .bind(name.as_str())
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| {
            StoreError::Validation(vec![FieldError::new(
                "environment",
                format!("no environment named {name}"),
            )])
        })
}

impl Store {
    pub async fn create_budget(&self, new: NewBudget) -> Result<Budget> {
        check(validate(&new))?;
        let mut tx = self.pool.begin().await?;
        let (scope, environment, key, person) = match &new.scope {
            BudgetScope::Environment(env) => (
                "environment",
                Some(environment_id(&mut tx, env).await?),
                None,
                None,
            ),
            BudgetScope::Key(id) => {
                let found: Option<String> =
                    sqlx::query_scalar("select id from env_keys where id = $1 for key share")
                        .bind(id.as_str())
                        .fetch_optional(&mut *tx)
                        .await?;
                if found.is_none() {
                    return Err(StoreError::Validation(vec![FieldError::new(
                        "key",
                        format!("no environment key {id}"),
                    )]));
                }
                ("key", None, Some(id.as_str()), None)
            }
            BudgetScope::Person {
                person,
                environment,
            } => {
                let environment = match environment {
                    Some(env) => Some(environment_id(&mut tx, env).await?),
                    None => None,
                };
                ("person", environment, None, Some(person.as_str()))
            }
        };
        let row = sqlx::query(
            "insert into budgets (name, scope, environment_id, key_id, person, limit_micro_usd, \
             window_kind, calendar) values ($1, $2, $3, $4, $5, $6, $7, $8) \
             returning id, created_at",
        )
        .bind(&new.name)
        .bind(scope)
        .bind(environment)
        .bind(key)
        .bind(person)
        .bind(new.limit.0)
        .bind(new.window.kind.as_str())
        .bind(new.window.calendar)
        .fetch_one(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Budgets).await?;
        tx.commit().await?;
        Ok(Budget {
            id: row.try_get("id")?,
            name: new.name,
            scope: new.scope,
            limit: new.limit,
            window: new.window,
            created_at: row.try_get("created_at")?,
        })
    }

    /// All budgets, sorted by name.
    pub async fn list_budgets(&self) -> Result<Vec<Budget>> {
        sqlx::query(concat!(select_budgets!(), " order by b.name, b.created_at"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(from_row)
            .collect()
    }

    /// Deletes a budget and its spend history.
    pub async fn delete_budget(&self, id: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from budgets where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Budgets).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The budgets a request through `key` by `person` in `env` counts
    /// against: the environment's, the key's, and the person's in this
    /// environment or across all.
    pub async fn applicable_budgets(
        &self,
        env: &EnvName,
        key: &EnvKeyId,
        person: &str,
    ) -> Result<Vec<Budget>> {
        sqlx::query(concat!(
            select_budgets!(),
            " where (b.scope = 'environment' and e.name = $1) \
             or (b.scope = 'key' and b.key_id = $2) \
             or (b.scope = 'person' and b.person = $3 \
                 and (b.environment_id is null or e.name = $1)) \
             order by b.name, b.created_at"
        ))
        .bind(env.as_str())
        .bind(key.as_str())
        .bind(person)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
    }

    /// Adds `amount` to each budget's spend in the window containing `now`.
    ///
    /// Each budget is one atomic upsert, committed on its own, so concurrent
    /// charges never lose spend or deadlock. Budgets deleted meanwhile are
    /// skipped. A zero amount writes nothing.
    pub async fn charge(
        &self,
        budgets: &[Uuid],
        amount: MicroUsd,
        now: OffsetDateTime,
    ) -> Result<Vec<Charged>> {
        if amount.0 < 0 {
            return Err(StoreError::Validation(vec![FieldError::new(
                "amount",
                "must not be negative",
            )]));
        }
        if amount.0 == 0 || budgets.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "select id, window_kind, calendar, created_at from budgets \
             where id = any($1) order by id",
        )
        .bind(budgets)
        .fetch_all(&self.pool)
        .await?;
        let mut charged = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: Uuid = row.try_get("id")?;
            let created_at: OffsetDateTime = row.try_get("created_at")?;
            let (window_start, _) = window_from_row(row)?.current(now, created_at);
            let after: i64 = match sqlx::query_scalar(
                "insert into spend (budget_id, window_start, spent_micro_usd) values ($1, $2, $3) \
                 on conflict (budget_id, window_start) \
                 do update set spent_micro_usd = spend.spent_micro_usd + excluded.spent_micro_usd \
                 returning spent_micro_usd",
            )
            .bind(id)
            .bind(window_start)
            .bind(amount.0)
            .fetch_one(&self.pool)
            .await
            {
                Ok(after) => after,
                Err(e) if is_foreign_key_violation(&e) => continue, // deleted meanwhile
                Err(e) => return Err(e.into()),
            };
            charged.push(Charged {
                budget: id,
                before: MicroUsd(after - amount.0),
                after: MicroUsd(after),
            });
        }
        Ok(charged)
    }

    /// Each budget's spend in the window containing `now`, in input order.
    pub async fn status(
        &self,
        budgets: &[Budget],
        now: OffsetDateTime,
    ) -> Result<Vec<BudgetStatus>> {
        let windows: Vec<_> = budgets
            .iter()
            .map(|b| b.window.current(now, b.created_at))
            .collect();
        let ids: Vec<Uuid> = budgets.iter().map(|b| b.id).collect();
        let starts: Vec<OffsetDateTime> = windows.iter().map(|w| w.0).collect();
        let spent: HashMap<Uuid, i64> = sqlx::query_as(
            "select s.budget_id, s.spent_micro_usd from spend s \
             join unnest($1::uuid[], $2::timestamptz[]) as current (budget_id, window_start) \
             on s.budget_id = current.budget_id and s.window_start = current.window_start",
        )
        .bind(&ids)
        .bind(&starts)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect();
        Ok(budgets
            .iter()
            .zip(windows)
            .map(|(budget, (_, end))| BudgetStatus {
                spent: MicroUsd(spent.get(&budget.id).copied().unwrap_or(0)),
                budget: budget.clone(),
                resets_at: end,
            })
            .collect())
    }
}
