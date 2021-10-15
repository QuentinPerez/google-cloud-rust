use crate::apiv1::spanner_client::Client;
use crate::client::ReadWriteTransactionOption;
use crate::session_pool::{ManagedSession, SessionHandle, SessionManager};
use crate::statement::Statement;
use crate::transaction::{CallOptions, QueryOptions, Transaction};
use async_trait::async_trait;
use chrono::NaiveDateTime;
use google_cloud_gax::call_option::{RetrySettings, BackoffRetrySettings};
use google_cloud_gax::invoke::AsTonicStatus;
use google_cloud_googleapis::spanner::v1::commit_request::Transaction::TransactionId;
use google_cloud_googleapis::spanner::v1::spanner_client::SpannerClient;
use google_cloud_googleapis::spanner::v1::transaction_options::Mode::ReadWrite;
use google_cloud_googleapis::spanner::v1::{
    commit_request, execute_batch_dml_request, execute_sql_request::QueryMode, request_options,
    result_set_stats, transaction_options, transaction_selector, BeginTransactionRequest,
    CommitRequest, CommitResponse, ExecuteBatchDmlRequest, ExecuteSqlRequest, Mutation,
    RequestOptions, ResultSet, ResultSetStats, RollbackRequest, Session, TransactionOptions,
    TransactionSelector,
};
use prost_types::Struct;
use std::future::Future;
use std::net::Shutdown::Read;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct CommitOptions {
    pub return_commit_stats: bool,
    pub call_options: CallOptions,
}

impl Default for CommitOptions {
    fn default() -> Self {
        CommitOptions {
            return_commit_stats: false,
            call_options: CallOptions::default(),
        }
    }
}

/// ReadWriteTransaction provides a locking read-write transaction.
///
/// This type of transaction is the only way to write data into Cloud Spanner;
/// Client::apply, Client::apply_at_least_once, Client::partitioned_update use
/// transactions internally. These transactions rely on pessimistic locking and,
/// if necessary, two-phase commit. Locking read-write transactions may abort,
/// requiring the application to retry. However, the interface exposed by
/// Client:run_with_retry eliminates the need for applications to write
/// retry loops explicitly.
///
/// Locking transactions may be used to atomically read-modify-write data
/// anywhere in a database. This type of transaction is externally consistent.
///
/// Clients should attempt to minimize the amount of time a transaction is
/// active. Faster transactions commit with higher probability and cause less
/// contention. Cloud Spanner attempts to keep read locks active as long as the
/// transaction continues to do reads.  Long periods of inactivity at the client
/// may cause Cloud Spanner to release a transaction's locks and abort it.
///
/// Reads performed within a transaction acquire locks on the data being
/// read. Writes can only be done at commit time, after all reads have been
/// completed. Conceptually, a read-write transaction consists of zero or more
/// reads or SQL queries followed by a commit.
///
/// See Client::run_with_retry for an example.
///
/// Semantics
///
/// Cloud Spanner can commit the transaction if all read locks it acquired are
/// still valid at commit time, and it is able to acquire write locks for all
/// writes. Cloud Spanner can abort the transaction for any reason. If a commit
/// attempt returns ABORTED, Cloud Spanner guarantees that the transaction has
/// not modified any user data in Cloud Spanner.
///
/// Unless the transaction commits, Cloud Spanner makes no guarantees about how
/// long the transaction's locks were held for. It is an error to use Cloud
/// Spanner locks for any sort of mutual exclusion other than between Cloud
/// Spanner transactions themselves.
///
/// Aborted transactions
///
/// Application code does not need to retry explicitly; RunInTransaction will
/// automatically retry a transaction if an attempt results in an abort. The lock
/// priority of a transaction increases after each prior aborted transaction,
/// meaning that the next attempt has a slightly better chance of success than
/// before.
///
/// Under some circumstances (e.g., many transactions attempting to modify the
/// same row(s)), a transaction can abort many times in a short period before
/// successfully committing. Thus, it is not a good idea to cap the number of
/// retries a transaction can attempt; instead, it is better to limit the total
/// amount of wall time spent retrying.
pub struct ReadWriteTransaction {
    base_tx: Transaction,
    tx_id: Vec<u8>,
    pub wb: Vec<Mutation>,
}

impl Deref for ReadWriteTransaction {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target {
        return &self.base_tx;
    }
}

impl DerefMut for ReadWriteTransaction {
    fn deref_mut(&mut self) -> &mut Transaction {
        return &mut self.base_tx;
    }
}

pub struct BeginError {
    pub status: tonic::Status,
    pub session: ManagedSession,
}

impl ReadWriteTransaction {
    pub async fn begin(
        mut session: ManagedSession,
        options: CallOptions,
    ) -> Result<ReadWriteTransaction, BeginError> {
        return ReadWriteTransaction::begin_internal(
            session,
            transaction_options::Mode::ReadWrite(transaction_options::ReadWrite {}),
            options,
        )
        .await;
    }

    pub async fn begin_partitioned_dml(
        mut session: ManagedSession,
        options: CallOptions,
    ) -> Result<ReadWriteTransaction, BeginError> {
        return ReadWriteTransaction::begin_internal(
            session,
            transaction_options::Mode::PartitionedDml(transaction_options::PartitionedDml {}),
            options,
        )
        .await;
    }

    async fn begin_internal(
        mut session: ManagedSession,
        mode: transaction_options::Mode,
        options: CallOptions,
    ) -> Result<ReadWriteTransaction, BeginError> {
        let request = BeginTransactionRequest {
            session: session.session.name.to_string(),
            options: Some(TransactionOptions { mode: Some(mode) }),
            request_options: Transaction::create_request_options(options.priority),
        };
        let result = session
            .spanner_client
            .begin_transaction(request, options.call_setting)
            .await;
        let response = match session.invalidate_if_needed(result).await {
            Ok(response) => response,
            Err(err) => {
                return Err(BeginError {
                    status: err,
                    session,
                })
            }
        };
        let tx = response.into_inner();
        Ok(ReadWriteTransaction {
            base_tx: Transaction {
                session: Some(session),
                sequence_number: AtomicI64::new(0),
                transaction_selector: TransactionSelector {
                    selector: Some(transaction_selector::Selector::Id(tx.id.clone())),
                },
            },
            tx_id: tx.id,
            wb: vec![],
        })
    }

    pub fn buffer_write(&mut self, ms: Vec<Mutation>) {
        self.wb.extend_from_slice(&ms)
    }

    pub async fn update(
        &mut self,
        stmt: Statement,
        options: Option<QueryOptions>,
    ) -> Result<i64, tonic::Status> {
        let opt = match options {
            Some(o) => o,
            None => QueryOptions::default(),
        };

        let request = ExecuteSqlRequest {
            session: self.get_session_name(),
            transaction: Some(self.transaction_selector.clone()),
            sql: stmt.sql.to_string(),
            params: Some(prost_types::Struct {
                fields: stmt.params,
            }),
            param_types: stmt.param_types,
            resume_token: vec![],
            query_mode: opt.mode.into(),
            partition_token: vec![],
            seqno: self.sequence_number.fetch_add(1, Ordering::Relaxed),
            query_options: opt.optimizer_options,
            request_options: Transaction::create_request_options(opt.call_options.priority),
        };

        let session = self.as_mut_session();
        let result = session
            .spanner_client
            .execute_sql(request, opt.call_options.call_setting)
            .await;
        let response = session.invalidate_if_needed(result).await?;
        Ok(extract_row_count(response.into_inner().stats))
    }

    pub async fn batch_update(
        &mut self,
        stmt: Vec<Statement>,
        options: Option<QueryOptions>,
    ) -> Result<Vec<i64>, tonic::Status> {
        let opt = match options {
            Some(o) => o,
            None => QueryOptions::default(),
        };

        let request = ExecuteBatchDmlRequest {
            session: self.get_session_name(),
            transaction: Some(self.transaction_selector.clone()),
            seqno: self.sequence_number.fetch_add(1, Ordering::Relaxed),
            request_options: Transaction::create_request_options(opt.call_options.priority),
            statements: stmt
                .into_iter()
                .map(|x| execute_batch_dml_request::Statement {
                    sql: x.sql,
                    params: Some(Struct { fields: x.params }),
                    param_types: x.param_types,
                })
                .collect(),
        };

        let session = self.as_mut_session();
        let result = session
            .spanner_client
            .execute_batch_dml(request, opt.call_options.call_setting)
            .await;
        let response = session.invalidate_if_needed(result).await?;
        Ok(response
            .into_inner()
            .result_sets
            .into_iter()
            .map(|x| extract_row_count(x.stats))
            .collect())
    }

    pub async fn finish<T, E>(
        &mut self,
        result: Result<T, E>,
        options: Option<CommitOptions>,
    ) -> Result<(Option<prost_types::Timestamp>, T), E>
    where
        E: AsTonicStatus + From<tonic::Status>,
    {
        let opt = match options {
            Some(o) => o,
            None => CommitOptions::default(),
        };

        return match result {
            Ok(s) => match self.commit(opt).await {
                Ok(c) => Ok((c.commit_timestamp, s)),
                // Retry the transaction using the same session on ABORT error.
                // Cloud Spanner will create the new transaction with the previous
                // one's wound-wait priority.
                Err(e) => Err(E::from(e)),
            },

            // Rollback the transaction unless the error occurred during the
            // commit. Executing a rollback after a commit has failed will
            // otherwise cause an error. Note that transient errors, such as
            // UNAVAILABLE, are already handled in the gRPC layer and do not show
            // up here. Context errors (deadline exceeded / canceled) during
            // commits are also not rolled back.
            Err(err) => {
                let status = match err.as_tonic_status() {
                    Some(status) => status,
                    None => {
                        self.rollback(opt.call_options.call_setting).await;
                        return Err(err);
                    }
                };
                match status.code() {
                    tonic::Code::Aborted => Err(err),
                    tonic::Code::NotFound => Err(err),
                    _ => {
                        self.rollback(opt.call_options.call_setting).await;
                        return Err(err);
                    }
                }
            }
        };
    }

    pub async fn commit(
        &mut self,
        options: CommitOptions,
    ) -> Result<CommitResponse, tonic::Status> {
        let tx_id = self.tx_id.clone();
        let mutations = self.wb.to_vec();
        let session = self.as_mut_session();
        return commit(session, mutations, TransactionId(tx_id), options).await;
    }

    pub async fn rollback(&mut self, setting: Option<BackoffRetrySettings>) -> Result<(), tonic::Status> {
        let request = RollbackRequest {
            transaction_id: self.tx_id.clone(),
            session: self.get_session_name(),
        };
        let session = self.as_mut_session();
        let result = session.spanner_client.rollback(request, setting).await;
        let response = session.invalidate_if_needed(result).await;
        match response {
            Ok(r) => Ok(r.into_inner()),
            Err(e) => Err(e),
        }
    }
}

pub async fn commit(
    session: &mut ManagedSession,
    ms: Vec<Mutation>,
    tx: commit_request::Transaction,
    commit_options: CommitOptions,
) -> Result<CommitResponse, tonic::Status> {
    let request = CommitRequest {
        session: session.session.name.to_string(),
        mutations: ms,
        transaction: Some(tx),
        request_options: Transaction::create_request_options(commit_options.call_options.priority),
        return_commit_stats: commit_options.return_commit_stats,
    };
    let result = session
        .spanner_client
        .commit(request, commit_options.call_options.call_setting)
        .await;
    let response = session.invalidate_if_needed(result).await;
    match response {
        Ok(r) => Ok(r.into_inner()),
        Err(s) => Err(s),
    }
}

fn extract_row_count(rs: Option<ResultSetStats>) -> i64 {
    match rs {
        Some(o) => match o.row_count {
            Some(o) => match o {
                result_set_stats::RowCount::RowCountExact(v) => v,
                result_set_stats::RowCount::RowCountLowerBound(v) => v,
            },
            None => 0,
        },
        None => 0,
    }
}
