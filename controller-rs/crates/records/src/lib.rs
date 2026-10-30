//! The durable records the controller writes about each device (spec §3.5.1 rules 4–5,
//! §3.5.2, §4.11, §5.4 "Database", §5.7 step 4): turn and refused-candidate rows, near
//! misses, the recordings named by their row ids, device logs, the hourly rollups and the
//! shadow-rule statistics, all in the shapes the existing installation stores.
//!
//! - [`turns`] and [`near_misses`] hold the row types, their column encodings (each module's
//!   docs list every column), the mapping from the turn crate's wake and utterance traces,
//!   idempotent inserts by operation key and the continuation update.
//! - [`trace`] composes `turns.decision_trace`.
//! - [`recordings`] names the files, writes WAVs atomically and holds the commands that tie
//!   files to rows; [`service::RecordsService`] publishes them, reconciles them at startup,
//!   applies retention and deletes a device's records.
//! - [`logs`] and [`rollups`] write `device_logs`, `device_metrics`, `wake_counters`,
//!   `wake_shadow` and `wake_shadow_events`.
//! - [`reads`] reads back what the dashboard lists: turns and near misses, the wake clips an
//!   archive holds, response latencies and the shadow-rule statistics.
//!
//! Every write is a [`WriteCommand`](echomuse_db::WriteCommand) and every read a
//! [`ReadQuery`](echomuse_db::ReadQuery) of the database crate, so each applies in its own
//! transaction on the database thread. The device runtime should go through
//! [`RecordsService`], which also coordinates files with rows and tells subscribers of each
//! new turn ([`RecordsService::subscribe_turns`]); the commands are public for the API's
//! reads and for tests.
//!
//! Not here: deleting a single turn or near miss on request, ambient recordings
//! (`ambient/<device>/*.wav.part`, written while they record), the device-metric and
//! wake-counter reads (no dashboard view reads them yet), and the producers of the AFE
//! evidence and series, transcript correction and clip-window JSON, whose objects these
//! records store as given.

pub mod error;
pub mod logs;
pub mod near_misses;
pub mod reads;
pub mod recordings;
pub mod rollups;
pub mod service;
pub mod trace;
pub mod turns;
pub mod values;
pub mod vocab;

pub use error::{MappingError, RecordError};
pub use logs::{DeviceLogEntry, InsertDeviceLog};
pub use near_misses::{
    GetNearMiss, InsertNearMiss, NearMissInsert, NearMissRecord, NearMissRowId, StoredNearMiss,
    VerdictUpdate,
};
pub use reads::{
    AnsweredRequests, ArchiveClip, ConfirmedWakeClips, ListNearMisses, ListScope, ListTurns,
    NearMissListing, REFUSED_WAKE, RecentShadowEvents, ResponseLatencies, ResponseLatency,
    SavedNearMissRecordings, ShadowTotals, ShadowTotalsSince, StoredShadowEvent, TurnListing,
    WakeClipRow, WakeClipRows,
};
pub use recordings::{RecordingKind, RecordingsLayout};
pub use rollups::{
    NetCounters, NetDelta, NetTotals, RttSample, ShadowEvent, ShadowStats, StatsReport,
    WakeStatsReport,
};
pub use service::{
    DRAIN_WAIT, DeletedRecords, FixFollows, NearMissAudio, PersistedNearMiss, PersistedTurn,
    Publication, ReconcileReport, RecordsError, RecordsLifetime, RecordsService, RetentionLimits,
    TurnAudio, TurnStored,
};
pub use trace::{DecisionTrace, TraceDetails, TraceParts, VerificationTiming};
pub use turns::{
    CommitColumns, ContinuationUpdate, GetTurn, InsertTurn, IntentColumns, RecordContinuation,
    StoredTurn, TurnInsert, TurnKey, TurnRecord, TurnRowId, UtteranceColumns, WakeColumns,
    WakeContext,
};
pub use values::{
    AfeEvidence, AfeSeries, CandidateId, ClipWindows, HourStart, JsonObject, NearMissUuid, Pcm16,
    PolicyHash, ReferenceCoverage, ResponseType, Score, SttFix, UnixMillis, WakeModelName,
    WallSeconds,
};
