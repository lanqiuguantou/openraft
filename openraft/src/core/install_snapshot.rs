use std::io::SeekFrom;

use anyerror::AnyError;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;

use crate::core::purge_applied_logs;
use crate::core::RaftCore;
use crate::core::SnapshotState;
use crate::core::State;
use crate::error::InstallSnapshotError;
use crate::error::SnapshotMismatch;
use crate::raft::InstallSnapshotRequest;
use crate::raft::InstallSnapshotResponse;
use crate::AppData;
use crate::AppDataResponse;
use crate::ErrorSubject;
use crate::ErrorVerb;
use crate::LogIdOptionExt;
use crate::MessageSummary;
use crate::RaftNetwork;
use crate::RaftStorage;
use crate::SnapshotSegmentId;
use crate::StorageError;
use crate::StorageHelper;
use crate::StorageIOError;
use crate::Update;

impl<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> RaftCore<D, R, N, S> {
    /// Invoked by leader to send chunks of a snapshot to a follower (§7).
    ///
    /// Leaders always send chunks in order. It is important to note that, according to the Raft spec,
    /// a log may only have one snapshot at any time. As snapshot contents are application specific,
    /// the Raft log will only store a pointer to the snapshot file along with the index & term.
    #[tracing::instrument(level = "debug", skip(self, req), fields(req=%req.summary()))]
    pub(super) async fn handle_install_snapshot_request(
        &mut self,
        req: InstallSnapshotRequest,
    ) -> Result<InstallSnapshotResponse, InstallSnapshotError> {
        // If message's term is less than most recent term, then we do not honor the request.
        if req.term < self.current_term {
            return Ok(InstallSnapshotResponse {
                term: self.current_term,
                last_applied: None,
            });
        }

        // Update election timeout.
        self.update_next_election_timeout(true);

        // Update current term if needed.
        let mut report_metrics = false;
        if self.current_term != req.term {
            self.update_current_term(req.term, None);
            self.save_hard_state().await?;
            report_metrics = true;
        }

        // Update current leader if needed.
        if self.current_leader.as_ref() != Some(&req.leader_id) {
            self.current_leader = Some(req.leader_id);
            report_metrics = true;
        }

        // If not follower, become follower.
        if !self.target_state.is_follower() && !self.target_state.is_learner() {
            self.set_target_state(State::Follower); // State update will emit metrics.
        }

        if report_metrics {
            self.report_metrics(Update::AsIs);
        }

        // Compare current snapshot state with received RPC and handle as needed.
        // - Init a new state if it is empty or building a snapshot locally.
        // - Mismatched id with offset=0 indicates a new stream has been sent, the old one should be dropped and start
        //   to receive the new snapshot,
        // - Mismatched id with offset greater than 0 is an out of order message that should be rejected.
        match self.snapshot_state.take() {
            None => self.begin_installing_snapshot(req).await,
            Some(SnapshotState::Snapshotting { handle, .. }) => {
                handle.abort(); // Abort the current compaction in favor of installation from leader.
                self.begin_installing_snapshot(req).await
            }
            Some(SnapshotState::Streaming { snapshot, id, offset }) => {
                if req.meta.snapshot_id == id {
                    return self.continue_installing_snapshot(req, offset, snapshot).await;
                }

                if req.offset == 0 {
                    return self.begin_installing_snapshot(req).await;
                }

                Err(SnapshotMismatch {
                    expect: SnapshotSegmentId { id: id.clone(), offset },
                    got: SnapshotSegmentId {
                        id: req.meta.snapshot_id.clone(),
                        offset: req.offset,
                    },
                }
                .into())
            }
        }
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(req=%req.summary()))]
    async fn begin_installing_snapshot(
        &mut self,
        req: InstallSnapshotRequest,
    ) -> Result<InstallSnapshotResponse, InstallSnapshotError> {
        let id = req.meta.snapshot_id.clone();

        if req.offset > 0 {
            return Err(SnapshotMismatch {
                expect: SnapshotSegmentId {
                    id: id.clone(),
                    offset: 0,
                },
                got: SnapshotSegmentId { id, offset: req.offset },
            }
            .into());
        }

        // Create a new snapshot and begin writing its contents.
        let mut snapshot = self.storage.begin_receiving_snapshot().await?;
        snapshot.as_mut().write_all(&req.data).await.map_err(|e| StorageError::IO {
            source: StorageIOError::new(
                ErrorSubject::Snapshot(req.meta.clone()),
                ErrorVerb::Write,
                AnyError::new(&e),
            ),
        })?;

        // If this was a small snapshot, and it is already done, then finish up.
        if req.done {
            self.finalize_snapshot_installation(req, snapshot).await?;
            return Ok(InstallSnapshotResponse {
                term: self.current_term,
                last_applied: self.last_applied,
            });
        }

        // Else, retain snapshot components for later segments & respond.
        self.snapshot_state = Some(SnapshotState::Streaming {
            offset: req.data.len() as u64,
            id,
            snapshot,
        });
        Ok(InstallSnapshotResponse {
            term: self.current_term,
            last_applied: None,
        })
    }

    #[tracing::instrument(level = "debug", skip(self, req, snapshot), fields(req=%req.summary()))]
    async fn continue_installing_snapshot(
        &mut self,
        req: InstallSnapshotRequest,
        mut offset: u64,
        mut snapshot: Box<S::SnapshotData>,
    ) -> Result<InstallSnapshotResponse, InstallSnapshotError> {
        let id = req.meta.snapshot_id.clone();

        // Always seek to the target offset if not an exact match.
        if req.offset != offset {
            if let Err(err) = snapshot.as_mut().seek(SeekFrom::Start(req.offset)).await {
                self.snapshot_state = Some(SnapshotState::Streaming { offset, id, snapshot });
                return Err(StorageError::from_io_error(
                    ErrorSubject::Snapshot(req.meta.clone()),
                    ErrorVerb::Seek,
                    err,
                )
                .into());
            }
            offset = req.offset;
        }

        // Write the next segment & update offset.
        if let Err(err) = snapshot.as_mut().write_all(&req.data).await {
            self.snapshot_state = Some(SnapshotState::Streaming { offset, id, snapshot });
            return Err(
                StorageError::from_io_error(ErrorSubject::Snapshot(req.meta.clone()), ErrorVerb::Write, err).into(),
            );
        }
        offset += req.data.len() as u64;

        // If the snapshot stream is done, then finalize.
        if req.done {
            self.finalize_snapshot_installation(req, snapshot).await?;
        } else {
            self.snapshot_state = Some(SnapshotState::Streaming { offset, id, snapshot });
        }
        Ok(InstallSnapshotResponse {
            term: self.current_term,
            last_applied: self.last_applied,
        })
    }

    /// Finalize the installation of a new snapshot.
    ///
    /// Any errors which come up from this routine will cause the Raft node to go into shutdown.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn finalize_snapshot_installation(
        &mut self,
        req: InstallSnapshotRequest,
        mut snapshot: Box<S::SnapshotData>,
    ) -> Result<(), StorageError> {
        tracing::info!("finalize_snapshot_installation: req: {:?}", req);

        snapshot.as_mut().shutdown().await.map_err(|e| StorageError::IO {
            source: StorageIOError::new(
                ErrorSubject::Snapshot(req.meta.clone()),
                ErrorVerb::Write,
                AnyError::new(&e),
            ),
        })?;

        // Caveat: All changes to state machine must be serialized
        //
        // If `finalize_snapshot_installation` is run in RaftCore thread,
        // there is chance the last_applied being reset to a previous value:
        //
        // ```
        // RaftCore: -.    install-snapc,            .-> replicate_to_sm_handle.next(),
        //            |    update last_applied=5     |   update last_applied=2
        //            |                              |
        //            v                              |
        // task:      apply 2------------------------'
        // --------------------------------------------------------------------> time
        // ```

        if req.meta.last_log_id < self.last_applied {
            tracing::info!(
                "skip installing snapshot because snapshot_meta.last_log_id({}) <= self.last_applied({})",
                req.meta.last_log_id.summary(),
                self.last_applied.summary(),
            );
            return Ok(());
        }

        if let Some(last) = req.meta.last_log_id {
            let idx = last.index;
            let matches = {
                let logs = self.storage.try_get_log_entries(idx..=idx).await?;
                if let Some(ent) = logs.first() {
                    Some(ent.log_id) == req.meta.last_log_id
                } else {
                    // no log entry, consider it unmatched.
                    false
                }
            };

            // The log entry at snapshot_meta.last_log_id.index conflicts with the leaders'
            // We have to delete all conflicting logs before installing snapshot.
            // See: [snapshot-replication](https://datafuselabs.github.io/openraft/replication.html#snapshot-replication)
            if !matches {
                // Delete all non-committed log entries.
                // It is safe:
                let x = StorageHelper::new(&self.storage).get_log_id(self.last_applied.next_index()).await;
                if let Ok(log_id) = x {
                    self.delete_conflict_logs_since(log_id).await?;
                }
                // else: no next log id, ignore
            }
        }

        let changes = self.storage.install_snapshot(&req.meta, snapshot).await?;

        tracing::info!("update after install-snapshot: {:?}", changes);

        // After installing snapshot, no inconsistent log is removed.
        // This does not affect raft consistency.
        // If you have any question about this, let me know: drdr.xp at gmail.com

        let last_applied = changes.last_applied;

        // Applied logs are not needed. Purge them or there may be a hole in the log.
        if let Some(last) = &last_applied {
            purge_applied_logs(self.storage.clone(), last, 0).await?;
        }

        // snapshot is installed
        self.last_applied = last_applied;

        if self.committed < self.last_applied {
            self.committed = self.last_applied;
        }
        if self.last_log_id < self.last_applied {
            self.last_log_id = self.last_applied;
        }

        // There could be unknown membership in the snapshot.
        let membership = StorageHelper::new(&self.storage).get_membership().await?;
        tracing::info!("re-fetch membership from store: {:?}", membership);

        assert!(membership.is_some());

        let membership = membership.unwrap();

        self.update_membership(membership);

        self.snapshot_last_log_id = self.last_applied;
        self.report_metrics(Update::AsIs);

        Ok(())
    }
}
