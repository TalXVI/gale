//! Bounded, observational progress for one Preview or Deploy operation.
//! The overall fraction counts completed phases, never elapsed time. File
//! totals are published only after the set of files is known.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::plan::DeploySelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncOperation {
    Preview,
    Deploy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncPhase {
    FetchingPublication,
    StagingPayload,
    Connecting,
    ReadingState,
    CheckingLease,
    RefreshingState,
    ScanningPayload,
    VerifyingPayload,
    CheckingConfigs,
    BuildingPlan,
    FinalizingPreview,
    RemovingFiles,
    UploadingPayload,
    WritingConfigs,
    PersistingState,
    ApplyingRestart,
    ReleasingLease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgress {
    pub run_id: String,
    pub operation: SyncOperation,
    pub status: ProgressStatus,
    pub phase: SyncPhase,
    pub completed_phases: usize,
    pub total_phases: usize,
    pub completed: usize,
    pub total: Option<usize>,
    pub completed_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub item: Option<String>,
}

/// The reporter owns only one snapshot. Its sink must never make progress
/// delivery a precondition for the remote operation.
pub struct ProgressReporter {
    phases: Vec<SyncPhase>,
    current: SyncProgress,
    sink: Arc<dyn Fn(SyncProgress) + Send + Sync>,
}

impl ProgressReporter {
    pub fn new(
        run_id: String,
        operation: SyncOperation,
        selection: &DeploySelection,
        sink: impl Fn(SyncProgress) + Send + Sync + 'static,
    ) -> Self {
        let mut phases = vec![SyncPhase::FetchingPublication];
        if selection.include_mods {
            phases.push(SyncPhase::StagingPayload);
        }
        phases.extend([
            SyncPhase::Connecting,
            SyncPhase::ReadingState,
            SyncPhase::CheckingLease,
            SyncPhase::RefreshingState,
        ]);
        if selection.include_mods {
            phases.extend([SyncPhase::ScanningPayload, SyncPhase::VerifyingPayload]);
        }
        if selection.include_configs {
            phases.push(SyncPhase::CheckingConfigs);
        }
        phases.push(SyncPhase::BuildingPlan);
        match operation {
            SyncOperation::Preview => phases.push(SyncPhase::FinalizingPreview),
            SyncOperation::Deploy => {
                if selection.include_mods {
                    phases.extend([SyncPhase::RemovingFiles, SyncPhase::UploadingPayload]);
                }
                if selection.include_configs {
                    phases.push(SyncPhase::WritingConfigs);
                }
                phases.extend([
                    SyncPhase::PersistingState,
                    SyncPhase::ApplyingRestart,
                    SyncPhase::ReleasingLease,
                ]);
            }
        }
        let current = SyncProgress {
            run_id,
            operation,
            status: ProgressStatus::Running,
            phase: phases[0],
            completed_phases: 0,
            total_phases: phases.len(),
            completed: 0,
            total: None,
            completed_bytes: None,
            total_bytes: None,
            item: None,
        };
        let reporter = Self {
            phases,
            current,
            sink: Arc::new(sink),
        };
        reporter.emit();
        reporter
    }

    pub fn silent(operation: SyncOperation, selection: &DeploySelection) -> Self {
        Self::new(String::new(), operation, selection, |_| {})
    }

    fn emit(&self) {
        (self.sink)(self.current.clone());
    }

    pub fn phase(&mut self, phase: SyncPhase) {
        let Some(index) = self.phases.iter().position(|candidate| *candidate == phase) else {
            return;
        };
        if index <= self.current.completed_phases {
            return;
        }
        self.current.phase = phase;
        self.current.completed_phases = index;
        self.current.completed = 0;
        self.current.total = None;
        self.current.completed_bytes = None;
        self.current.total_bytes = None;
        self.current.item = None;
        self.emit();
    }

    pub fn work(&mut self, total: usize, total_bytes: Option<u64>) {
        self.current.total = Some(total.max(self.current.completed));
        if let Some(bytes) = total_bytes {
            self.current.total_bytes = Some(bytes.max(self.current.completed_bytes.unwrap_or(0)));
        }
        if self.current.total_bytes.is_some() && self.current.completed_bytes.is_none() {
            self.current.completed_bytes = Some(0);
        }
        self.emit();
    }

    /// Called before a potentially slow remote operation. The completed
    /// count stays unchanged until the operation actually succeeds.
    pub fn item(&mut self, item: impl Into<String>) {
        self.current.item = Some(item.into());
        self.emit();
    }

    pub fn advance(&mut self, completed: usize, completed_bytes: Option<u64>) {
        self.current.completed = self
            .current
            .completed
            .max(completed)
            .min(self.current.total.unwrap_or(usize::MAX));
        if let Some(bytes) = completed_bytes {
            self.current.completed_bytes = Some(
                self.current
                    .completed_bytes
                    .unwrap_or(0)
                    .max(bytes)
                    .min(self.current.total_bytes.unwrap_or(u64::MAX)),
            );
        }
        self.emit();
    }

    pub fn succeeded(&mut self) {
        self.current.completed_phases = self.current.total_phases;
        self.current.status = ProgressStatus::Succeeded;
        self.emit();
    }

    pub fn failed(&mut self) {
        self.current.status = ProgressStatus::Failed;
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{ProgressReporter, SyncOperation, SyncPhase};
    use crate::profile::server::plan::DeploySelection;

    #[test]
    fn repeated_phase_and_late_total_do_not_regress_completed_work() {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let sink = snapshots.clone();
        let selection = DeploySelection {
            include_mods: true,
            ..Default::default()
        };
        let mut reporter = ProgressReporter::new(
            "run".to_owned(),
            SyncOperation::Preview,
            &selection,
            move |snapshot| sink.lock().unwrap().push(snapshot),
        );

        reporter.phase(SyncPhase::StagingPayload);
        reporter.advance(2, None);
        reporter.work(5, Some(100));
        reporter.advance(3, Some(60));
        reporter.phase(SyncPhase::StagingPayload);
        reporter.work(2, Some(40));
        reporter.advance(1, Some(10));

        let snapshots = snapshots.lock().unwrap();
        let final_snapshot = snapshots.last().unwrap();
        assert_eq!(final_snapshot.completed, 3);
        assert_eq!(final_snapshot.total, Some(3));
        assert_eq!(final_snapshot.completed_bytes, Some(60));
        assert_eq!(final_snapshot.total_bytes, Some(60));
        assert!(snapshots.windows(2).all(|pair| {
            pair[1].completed_phases >= pair[0].completed_phases
                && (pair[1].completed_phases != pair[0].completed_phases
                    || pair[1].completed >= pair[0].completed)
        }));
    }
}
