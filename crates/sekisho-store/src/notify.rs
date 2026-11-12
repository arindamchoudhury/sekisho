//! Change notifications between replicas, over PostgreSQL `LISTEN/NOTIFY`.

use std::time::Duration;

use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::{Result, Store};

/// The PostgreSQL channel every change is announced on.
pub const CHANNEL: &str = "sekisho_changes";

/// What changed. Listeners reload that kind of data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Providers,
    Models,
    Environments,
    Keys,
    Budgets,
    /// Users, their groups, admin marks, disabling and refusals.
    Users,
    /// A session ended.
    Sessions,
}

impl ChangeKind {
    pub const ALL: [ChangeKind; 7] = [
        ChangeKind::Providers,
        ChangeKind::Models,
        ChangeKind::Environments,
        ChangeKind::Keys,
        ChangeKind::Budgets,
        ChangeKind::Users,
        ChangeKind::Sessions,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Providers => "providers",
            ChangeKind::Models => "models",
            ChangeKind::Environments => "environments",
            ChangeKind::Keys => "keys",
            ChangeKind::Budgets => "budgets",
            ChangeKind::Users => "users",
            ChangeKind::Sessions => "sessions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Announces `kind` from inside a transaction. PostgreSQL delivers it only if
/// the transaction commits, so a rolled-back write announces nothing.
pub async fn notify(conn: &mut PgConnection, kind: ChangeKind) -> Result<()> {
    sqlx::query("select pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(kind.as_str())
        .execute(conn)
        .await?;
    Ok(())
}

impl Store {
    /// Streams changes committed by any replica. Notifications sent while the
    /// listening connection is down are lost, so after it reconnects every
    /// kind is reported once and listeners reload everything.
    ///
    /// The stream holds one of the pool's connections while it is alive, and
    /// ends when the pool is closed.
    pub async fn listen(&self) -> Result<impl Stream<Item = ChangeKind> + Send + Unpin + 'static> {
        let listener = subscribe(&self.pool).await?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(forward(self.pool.clone(), listener, tx));
        Ok(ReceiverStream::new(rx))
    }
}

async fn subscribe(pool: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

/// Runs until the stream is dropped or the pool is closed.
async fn forward(pool: PgPool, mut listener: PgListener, tx: mpsc::Sender<ChangeKind>) {
    loop {
        let received = tokio::select! {
            () = tx.closed() => return,
            received = listener.try_recv() => received,
        };
        let kinds = match received {
            Ok(Some(n)) => ChangeKind::parse(n.payload()).into_iter().collect(),
            // PgListener lost its connection and has reconnected.
            Ok(None) => ChangeKind::ALL.to_vec(),
            Err(error) => {
                tracing::warn!(%error, "change listener failed; reconnecting");
                listener = loop {
                    if pool.is_closed() {
                        return;
                    }
                    tokio::select! {
                        () = tx.closed() => return,
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    match subscribe(&pool).await {
                        Ok(listener) => break listener,
                        Err(error) => tracing::warn!(%error, "change listener reconnect failed"),
                    }
                };
                ChangeKind::ALL.to_vec()
            }
        };
        for kind in kinds {
            if tx.send(kind).await.is_err() {
                return;
            }
        }
    }
}
