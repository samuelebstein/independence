//! Security lifecycle and operating-mode ownership.
//!
//! This module deliberately contains no PIN handling, key material,
//! cryptography, disk storage, or authentication implementation.  Milestone
//! 6.1 establishes the state boundary first: production code can create an
//! ephemeral session, but it has no mechanism to create an authenticated
//! private session.

use std::{
    fmt,
    marker::PhantomData,
    time::{Duration, Instant},
};

use thiserror::Error;

/// Whether this device has private-security material enrolled and authorized.
///
/// This is independent of the current operating mode.  For example, a future
/// Locked device may still enter an ephemeral session without becoming
/// Unlocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityState {
    Uninitialized,
    SetupInProgress,
    Locked,
    Unlocked,
    RecoveryRequired,
}

impl SecurityState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::SetupInProgress => "setup in progress",
            Self::Locked => "locked",
            Self::Unlocked => "unlocked",
            Self::RecoveryRequired => "recovery required",
        }
    }
}

impl fmt::Display for SecurityState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The experience currently running inside Independence.
///
/// The mode is intentionally separate from [`SecurityState`].  Ephemeral Mode
/// is usable while a device is uninitialized, locked, or recovery-required;
/// Private Mode requires a future authentication result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatingMode {
    Ephemeral,
    Private,
}

impl OperatingMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::Private => "private",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ephemeral" => Ok(Self::Ephemeral),
            "private" => Ok(Self::Private),
            _ => Err(format!(
                "unknown operating mode `{value}`; use `ephemeral` or `private`"
            )),
        }
    }
}

impl Default for OperatingMode {
    fn default() -> Self {
        Self::Ephemeral
    }
}

impl fmt::Display for OperatingMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Five seconds makes accidental entry from an ordinary Talk press unlikely.
/// The gate consumes that initial press; a fresh press is required to record.
pub(crate) const EPHEMERAL_ENTRY_HOLD: Duration = Duration::from_secs(5);

/// Hardware-independent facts that a physical entry adapter can report.
///
/// `app` maps existing terminal/evdev control events onto these facts.  This
/// type deliberately knows nothing about microphones, terminal input, or GPIO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EphemeralEntryControl {
    TalkPressed,
    TalkReleased,
    Cancelled,
    Other,
}

/// Result of advancing the physical Ephemeral Mode entry gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EphemeralEntryOutcome {
    Waiting,
    TooShort,
    Cancelled,
    Entered,
}

/// A deliberately small hold-gesture recognizer.
///
/// It uses timestamps supplied by its caller, which makes its timing behavior
/// deterministic in tests and keeps it separate from the real input adapter.
#[derive(Debug)]
pub(crate) struct EphemeralEntryGesture {
    pressed_at: Option<Instant>,
}

impl EphemeralEntryGesture {
    pub(crate) fn new() -> Self {
        Self { pressed_at: None }
    }

    pub(crate) fn handle(
        &mut self,
        control: EphemeralEntryControl,
        now: Instant,
    ) -> EphemeralEntryOutcome {
        match control {
            EphemeralEntryControl::TalkPressed => {
                self.pressed_at.get_or_insert(now);
                EphemeralEntryOutcome::Waiting
            }
            EphemeralEntryControl::TalkReleased => {
                let Some(pressed_at) = self.pressed_at.take() else {
                    return EphemeralEntryOutcome::Waiting;
                };
                if now.saturating_duration_since(pressed_at) >= EPHEMERAL_ENTRY_HOLD {
                    EphemeralEntryOutcome::Entered
                } else {
                    EphemeralEntryOutcome::TooShort
                }
            }
            EphemeralEntryControl::Cancelled => {
                self.pressed_at = None;
                EphemeralEntryOutcome::Cancelled
            }
            EphemeralEntryControl::Other => EphemeralEntryOutcome::Waiting,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.pressed_at = None;
    }
}

/// Owns the current security lifecycle and the currently active operating
/// session.  It intentionally starts fresh for each process in Milestone 6.1:
/// persisting security state before real protected storage exists would create
/// a misleading, unauthenticated on-disk protocol.
#[derive(Debug)]
pub(crate) struct SecurityManager {
    state: SecurityState,
    active_mode: Option<ActiveMode>,
    next_session_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveMode {
    mode: OperatingMode,
    id: u64,
}

/// Proof that a currently running session is explicitly ephemeral.
///
/// It carries no secret and grants no private-storage access.  Its opaque
/// identity lets the manager reject accidental attempts to finish a different
/// session.
#[derive(Debug)]
pub(crate) struct EphemeralSession {
    id: u64,
}

/// A future authenticated private session.  There is no production constructor
/// in this milestone because no authenticator exists yet.
#[allow(dead_code)] // Capability shape for Milestones 6.2–6.4; no producer exists yet.
#[derive(Debug)]
pub(crate) struct PrivateSession {
    id: u64,
}

/// A borrow-scoped capability future protected-storage code may require.
///
/// It cannot be constructed outside this module.  Its lifetime prevents a
/// caller from holding it across a mutable lock transition on the manager.
#[allow(dead_code)] // Capability shape for Milestones 6.2–6.4; no producer exists yet.
#[derive(Debug)]
pub(crate) struct PrivateStorageAccess<'manager> {
    _manager: PhantomData<&'manager SecurityManager>,
}

/// A private marker that a later setup/recovery implementation may create only
/// after it has verified real setup material.  It has no production constructor
/// in Milestone 6.1.
#[allow(dead_code)] // Future verified setup proof; intentionally unconstructable in production.
struct VerifiedSetup;

/// A private marker that a later authenticator may create only after actual
/// authorization.  It prevents this milestone from shipping a fake PIN path.
#[allow(dead_code)] // Future authenticator proof; intentionally unconstructable in production.
struct Authenticated;

#[allow(dead_code)] // State-transition surface required by the approved architecture.
impl SecurityManager {
    pub(crate) fn new() -> Self {
        Self {
            state: SecurityState::Uninitialized,
            active_mode: None,
            next_session_id: 1,
        }
    }

    pub(crate) const fn state(&self) -> SecurityState {
        self.state
    }

    pub(crate) fn active_mode(&self) -> Option<OperatingMode> {
        self.active_mode.map(|active| active.mode)
    }

    /// Begins future enrollment.  This alone creates no data, no keys, and no
    /// authority; Milestone 6.2 will supply the actual verified setup work.
    pub(crate) fn begin_setup(&mut self) -> Result<(), SecurityError> {
        self.require_no_active_mode("begin setup")?;
        if self.state != SecurityState::Uninitialized {
            return Err(SecurityError::InvalidTransition {
                operation: "begin setup",
                state: self.state,
            });
        }
        self.state = SecurityState::SetupInProgress;
        Ok(())
    }

    /// Cancels an incomplete setup without manufacturing a locked/private
    /// state.  It is useful for future setup failure handling.
    pub(crate) fn cancel_setup(&mut self) -> Result<(), SecurityError> {
        if self.state != SecurityState::SetupInProgress {
            return Err(SecurityError::InvalidTransition {
                operation: "cancel setup",
                state: self.state,
            });
        }
        self.state = SecurityState::Uninitialized;
        Ok(())
    }

    /// Completes setup only when future setup code presents its internal proof.
    /// No code outside this module can manufacture that proof today.
    fn complete_setup(&mut self, _verified: VerifiedSetup) -> Result<(), SecurityError> {
        if self.state != SecurityState::SetupInProgress {
            return Err(SecurityError::InvalidTransition {
                operation: "complete setup",
                state: self.state,
            });
        }
        self.state = SecurityState::Locked;
        Ok(())
    }

    /// Enters the future recovery-required lifecycle without granting access.
    /// A later envelope/storage verifier will call this after a genuine error.
    pub(crate) fn require_recovery(&mut self) -> Result<(), SecurityError> {
        self.require_no_active_mode("require recovery")?;
        match self.state {
            SecurityState::SetupInProgress | SecurityState::Locked | SecurityState::Unlocked => {
                self.state = SecurityState::RecoveryRequired;
                Ok(())
            }
            SecurityState::Uninitialized | SecurityState::RecoveryRequired => {
                Err(SecurityError::InvalidTransition {
                    operation: "require recovery",
                    state: self.state,
                })
            }
        }
    }

    /// Starts an unauthenticated, memory-only session.  It deliberately does
    /// not change the security lifecycle or create private-storage authority.
    pub(crate) fn enter_ephemeral(&mut self) -> Result<EphemeralSession, SecurityError> {
        self.require_no_active_mode("enter Ephemeral Mode")?;
        match self.state {
            SecurityState::Uninitialized
            | SecurityState::Locked
            | SecurityState::RecoveryRequired => {
                let id = self.next_id();
                self.active_mode = Some(ActiveMode {
                    mode: OperatingMode::Ephemeral,
                    id,
                });
                Ok(EphemeralSession { id })
            }
            SecurityState::SetupInProgress | SecurityState::Unlocked => {
                Err(SecurityError::InvalidTransition {
                    operation: "enter Ephemeral Mode",
                    state: self.state,
                })
            }
        }
    }

    /// Ends one current ephemeral session.  This only revokes the mode lease;
    /// the application runtime owns and clears its in-memory conversation and
    /// media buffers before calling this method.
    pub(crate) fn end_ephemeral(&mut self, session: EphemeralSession) -> Result<(), SecurityError> {
        match self.active_mode {
            Some(ActiveMode {
                mode: OperatingMode::Ephemeral,
                id,
            }) if id == session.id => {
                self.active_mode = None;
                Ok(())
            }
            _ => Err(SecurityError::SessionMismatch {
                operation: "end Ephemeral Mode",
            }),
        }
    }

    /// Attempts to start Private Mode.  It intentionally fails in every
    /// production Milestone 6.1 state because no authenticator can make the
    /// device Unlocked yet.
    pub(crate) fn enter_private(&mut self) -> Result<PrivateSession, SecurityError> {
        self.require_no_active_mode("enter Private Mode")?;
        if self.state != SecurityState::Unlocked {
            return Err(SecurityError::PrivateModeRequiresAuthentication { state: self.state });
        }
        let id = self.next_id();
        self.active_mode = Some(ActiveMode {
            mode: OperatingMode::Private,
            id,
        });
        Ok(PrivateSession { id })
    }

    /// Returns the narrow future storage capability only while the matching
    /// Private Mode session remains authorized and unlocked.
    pub(crate) fn private_storage_access(
        &self,
        session: &PrivateSession,
    ) -> Result<PrivateStorageAccess<'_>, SecurityError> {
        match self.active_mode {
            Some(ActiveMode {
                mode: OperatingMode::Private,
                id,
            }) if id == session.id && self.state == SecurityState::Unlocked => {
                Ok(PrivateStorageAccess {
                    _manager: PhantomData,
                })
            }
            _ => Err(SecurityError::PrivateStorageDenied { state: self.state }),
        }
    }

    /// Locks the future private session and revokes access.  Repeating lock on
    /// an already Locked device is harmless; other lifecycle states reject it
    /// rather than being silently reinterpreted.
    pub(crate) fn lock(&mut self) -> Result<LockOutcome, SecurityError> {
        match self.state {
            SecurityState::Unlocked => {
                self.active_mode = None;
                self.state = SecurityState::Locked;
                Ok(LockOutcome::Locked)
            }
            SecurityState::Locked => Ok(LockOutcome::AlreadyLocked),
            SecurityState::Uninitialized
            | SecurityState::SetupInProgress
            | SecurityState::RecoveryRequired => Err(SecurityError::InvalidTransition {
                operation: "lock",
                state: self.state,
            }),
        }
    }

    fn require_no_active_mode(&self, operation: &'static str) -> Result<(), SecurityError> {
        if let Some(mode) = self.active_mode() {
            Err(SecurityError::OperatingModeAlreadyActive { operation, mode })
        } else {
            Ok(())
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_session_id;
        self.next_session_id = self
            .next_session_id
            .checked_add(1)
            .expect("security session identifiers exhausted");
        id
    }

    /// Test-only support for lifecycle transition coverage.  This method is
    /// not compiled into production artifacts and does not model a credential.
    #[cfg(test)]
    fn unlock_after_test_authentication(&mut self) -> Result<(), SecurityError> {
        if self.state != SecurityState::Locked {
            return Err(SecurityError::InvalidTransition {
                operation: "test authorization",
                state: self.state,
            });
        }
        self.authorize(Authenticated)
    }

    fn authorize(&mut self, _authenticated: Authenticated) -> Result<(), SecurityError> {
        if self.state != SecurityState::Locked {
            return Err(SecurityError::InvalidTransition {
                operation: "authorize",
                state: self.state,
            });
        }
        self.state = SecurityState::Unlocked;
        Ok(())
    }
}

/// Result of a lock request that did not need to expose any private state.
#[allow(dead_code)] // Returned only by future lock/authentication integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockOutcome {
    Locked,
    AlreadyLocked,
}

/// A lifecycle-policy error.  This is public only so public launch errors can
/// describe why a requested mode was denied; its variants do not grant any
/// authority and `SecurityManager` remains internal to the application.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecurityError {
    #[error("cannot {operation} while security lifecycle is {state}")]
    InvalidTransition {
        operation: &'static str,
        state: SecurityState,
    },
    #[error("cannot {operation}: {mode} mode is already active")]
    OperatingModeAlreadyActive {
        operation: &'static str,
        mode: OperatingMode,
    },
    #[error("Private Mode requires authentication; security lifecycle is {state}")]
    PrivateModeRequiresAuthentication { state: SecurityState },
    #[error("private storage access is denied while security lifecycle is {state}")]
    PrivateStorageDenied { state: SecurityState },
    #[error("cannot {operation}: the supplied session is not the active Ephemeral Mode session")]
    SessionMismatch { operation: &'static str },
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        EPHEMERAL_ENTRY_HOLD, EphemeralEntryControl, EphemeralEntryGesture, EphemeralEntryOutcome,
        LockOutcome, OperatingMode, PrivateSession, SecurityError, SecurityManager, SecurityState,
        VerifiedSetup,
    };

    fn locked_manager() -> SecurityManager {
        let mut manager = SecurityManager::new();
        manager.begin_setup().unwrap();
        manager.complete_setup(VerifiedSetup).unwrap();
        manager
    }

    #[test]
    fn fresh_installation_starts_uninitialized() {
        let manager = SecurityManager::new();
        assert_eq!(manager.state(), SecurityState::Uninitialized);
        assert_eq!(manager.active_mode(), None);
    }

    #[test]
    fn setup_transitions_to_locked_only_after_verified_completion() {
        let mut manager = SecurityManager::new();
        manager.begin_setup().unwrap();
        assert_eq!(manager.state(), SecurityState::SetupInProgress);
        manager.complete_setup(VerifiedSetup).unwrap();
        assert_eq!(manager.state(), SecurityState::Locked);
    }

    #[test]
    fn invalid_transitions_are_rejected_and_setup_failure_grants_no_access() {
        let mut manager = SecurityManager::new();
        assert!(matches!(
            manager.complete_setup(VerifiedSetup),
            Err(SecurityError::InvalidTransition { .. })
        ));
        manager.begin_setup().unwrap();
        manager.cancel_setup().unwrap();
        assert_eq!(manager.state(), SecurityState::Uninitialized);
        assert!(matches!(
            manager.enter_private(),
            Err(SecurityError::PrivateModeRequiresAuthentication { .. })
        ));
    }

    #[test]
    fn ephemeral_mode_is_available_without_unlocking_from_all_required_states() {
        let mut uninitialized = SecurityManager::new();
        let session = uninitialized.enter_ephemeral().unwrap();
        assert_eq!(uninitialized.state(), SecurityState::Uninitialized);
        assert_eq!(uninitialized.active_mode(), Some(OperatingMode::Ephemeral));
        uninitialized.end_ephemeral(session).unwrap();

        let mut locked = locked_manager();
        let session = locked.enter_ephemeral().unwrap();
        assert_eq!(locked.state(), SecurityState::Locked);
        locked.end_ephemeral(session).unwrap();

        let mut recovery = locked_manager();
        recovery.require_recovery().unwrap();
        let session = recovery.enter_ephemeral().unwrap();
        assert_eq!(recovery.state(), SecurityState::RecoveryRequired);
        recovery.end_ephemeral(session).unwrap();
    }

    #[test]
    fn recovery_required_never_grants_private_access() {
        let mut manager = locked_manager();
        manager.require_recovery().unwrap();
        assert!(matches!(
            manager.enter_private(),
            Err(SecurityError::PrivateModeRequiresAuthentication {
                state: SecurityState::RecoveryRequired
            })
        ));
    }

    #[test]
    fn ephemeral_mode_cannot_obtain_a_private_storage_capability() {
        let mut manager = SecurityManager::new();
        let ephemeral = manager.enter_ephemeral().unwrap();
        // This struct literal is available only inside this module's tests;
        // production callers cannot construct a private-session claim.
        let fabricated = PrivateSession { id: 999 };
        assert!(matches!(
            manager.private_storage_access(&fabricated),
            Err(SecurityError::PrivateStorageDenied {
                state: SecurityState::Uninitialized
            })
        ));
        manager.end_ephemeral(ephemeral).unwrap();
    }

    #[test]
    fn locking_an_unlocked_test_session_revokes_private_access_and_repeats_safely() {
        let mut manager = locked_manager();
        manager.unlock_after_test_authentication().unwrap();
        let private = manager.enter_private().unwrap();
        {
            let _access = manager.private_storage_access(&private).unwrap();
        }

        assert_eq!(manager.lock().unwrap(), LockOutcome::Locked);
        assert!(matches!(
            manager.private_storage_access(&private),
            Err(SecurityError::PrivateStorageDenied {
                state: SecurityState::Locked
            })
        ));
        assert_eq!(manager.lock().unwrap(), LockOutcome::AlreadyLocked);
    }

    #[test]
    fn physical_ephemeral_entry_requires_one_deliberate_hold_and_release() {
        let started = Instant::now();
        let mut gesture = EphemeralEntryGesture::new();
        assert_eq!(
            gesture.handle(EphemeralEntryControl::TalkPressed, started),
            EphemeralEntryOutcome::Waiting
        );
        assert_eq!(
            gesture.handle(
                EphemeralEntryControl::TalkReleased,
                started + EPHEMERAL_ENTRY_HOLD - std::time::Duration::from_millis(1),
            ),
            EphemeralEntryOutcome::TooShort
        );
        assert_eq!(
            gesture.handle(EphemeralEntryControl::TalkPressed, started),
            EphemeralEntryOutcome::Waiting
        );
        assert_eq!(
            gesture.handle(
                EphemeralEntryControl::TalkReleased,
                started + EPHEMERAL_ENTRY_HOLD,
            ),
            EphemeralEntryOutcome::Entered
        );
    }

    #[test]
    fn cancelling_or_other_buttons_cannot_enter_ephemeral_mode() {
        let now = Instant::now();
        let mut gesture = EphemeralEntryGesture::new();
        assert_eq!(
            gesture.handle(EphemeralEntryControl::Other, now),
            EphemeralEntryOutcome::Waiting
        );
        gesture.handle(EphemeralEntryControl::TalkPressed, now);
        assert_eq!(
            gesture.handle(EphemeralEntryControl::Cancelled, now),
            EphemeralEntryOutcome::Cancelled
        );
        assert_eq!(
            gesture.handle(
                EphemeralEntryControl::TalkReleased,
                now + EPHEMERAL_ENTRY_HOLD,
            ),
            EphemeralEntryOutcome::Waiting
        );
    }
}
