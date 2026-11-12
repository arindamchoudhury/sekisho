//! Sekisho's own groups, which can hold any user.

use sekisho_core::names::GroupName;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, is_foreign_key_violation, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

async fn group_id(conn: &mut PgConnection, name: &GroupName) -> Result<Uuid> {
    sqlx::query_scalar("select id from groups where name = $1 for key share")
        .bind(name.as_str())
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound)
}

impl Store {
    pub async fn create_group(&self, name: &GroupName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("insert into groups (name) values ($1)")
            .bind(name.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    StoreError::Conflict(format!("a group named {name} already exists"))
                } else {
                    e.into()
                }
            })?;
        notify(&mut tx, ChangeKind::Users).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes the group and its memberships.
    pub async fn delete_group(&self, name: &GroupName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("delete from groups where name = $1")
            .bind(name.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Users).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every group with its members, by name.
    pub async fn list_groups(&self) -> Result<Vec<(GroupName, Vec<Uuid>)>> {
        let rows: Vec<(String, Vec<Uuid>)> = sqlx::query_as(
            "select g.name, coalesce(array_agg(gm.user_id order by gm.user_id) \
             filter (where gm.user_id is not null), '{}') \
             from groups g left join group_members gm on gm.group_id = g.id \
             group by g.id order by g.name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(name, members)| {
                let name = GroupName::parse(&name).map_err(|_| bad_column("group name", &name))?;
                Ok((name, members))
            })
            .collect()
    }

    /// Adds the user to the group; already being a member is fine.
    pub async fn add_member(&self, group: &GroupName, user: &Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let group_id = group_id(&mut tx, group).await?;
        let added = sqlx::query(
            "insert into group_members (group_id, user_id) values ($1, $2) \
             on conflict do nothing",
        )
        .bind(group_id)
        .bind(user)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_foreign_key_violation(&e) {
                StoreError::Validation(vec![FieldError::new("user", format!("no user {user}"))])
            } else {
                e.into()
            }
        })?
        .rows_affected();
        if added > 0 {
            notify(&mut tx, ChangeKind::Users).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Takes the user out of the group; not being a member is fine.
    pub async fn remove_member(&self, group: &GroupName, user: &Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let group_id = group_id(&mut tx, group).await?;
        let removed = sqlx::query("delete from group_members where group_id = $1 and user_id = $2")
            .bind(group_id)
            .bind(user)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if removed > 0 {
            notify(&mut tx, ChangeKind::Users).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
