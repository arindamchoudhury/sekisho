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

const LABEL_MAX_CHARS: usize = 100;

/// Checks a human-readable name such as a key's or a budget's: 1–100
/// characters, not only spaces, no control characters.
pub(crate) fn check_label(errors: &mut Vec<FieldError>, field: &str, value: &str) {
    if value.trim().is_empty() {
        errors.push(FieldError::new(field, "must not be empty"));
    } else if value.chars().count() > LABEL_MAX_CHARS {
        errors.push(FieldError::new(
            field,
            format!("use at most {LABEL_MAX_CHARS} characters"),
        ));
    } else if value.chars().any(char::is_control) {
        errors.push(FieldError::new(
            field,
            "must not contain control characters",
        ));
    }
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

/// Whether `e` breaks the named constraint.
pub(crate) fn violates(e: &sqlx::Error, constraint: &str) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.constraint() == Some(constraint))
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
