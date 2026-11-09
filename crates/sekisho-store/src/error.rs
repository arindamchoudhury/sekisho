use sekisho_core::secret_box::SecretError;

/// One invalid input field, reported to admins as `{field, message}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    /// The write clashes with existing data, such as a name already taken.
    #[error("{0}")]
    Conflict(String),
    /// Something else still refers to what was to be deleted, such as
    /// `models: claude-sonnet-5-5`.
    #[error("still in use by {0}")]
    InUse(String),
    #[error("invalid input: {}", describe(.0))]
    Validation(Vec<FieldError>),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

fn describe(errors: &[FieldError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.field, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `Ok` when there are no field errors, otherwise `Validation` with all of them.
pub(crate) fn check(errors: Vec<FieldError>) -> Result<(), StoreError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(StoreError::Validation(errors))
    }
}

pub(crate) fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

pub(crate) fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_foreign_key_violation())
}

/// A stored value that no longer parses: the database CHECKs should make
/// this impossible, so it is reported as a database error.
pub(crate) fn bad_column(column: &str, value: &str) -> StoreError {
    StoreError::Db(sqlx::Error::Decode(
        format!("invalid {column} in the database: {value:?}").into(),
    ))
}
