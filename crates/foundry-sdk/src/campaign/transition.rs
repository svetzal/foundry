//! The campaign status state machine.
//!
//! This is the single home for every rule governing when a [`Campaign`] may
//! move between [`CampaignStatus`] values. Before this module existed, the
//! daemon's gRPC handlers (`foundryd::service::campaign_ops`), the CLI's
//! `--offline` recovery path, and the `AdvanceCampaign` task block each
//! re-implemented these rules independently, and the copies had already
//! diverged: `--offline complete` on a `Cancelled` campaign silently flipped
//! it to `Completed`, writing a false evidence claim into the store, while
//! the online path had always rejected it. Every caller — daemon, CLI
//! offline path, and formation block alike — must go through the methods
//! here so the legality of a transition never depends on which door the
//! caller walked through.

use chrono::{DateTime, Utc};

use super::{Campaign, CampaignStatus, OwnerDecision};

/// The outcome of an idempotent transition request.
///
/// Some operations (`complete`, `cancel`) are safe to call again once they
/// have already succeeded — the caller asked for a state the campaign is
/// already in. `AlreadySettled` distinguishes "nothing changed because it was
/// already done" from `Applied`, so callers can render the current detail
/// without re-emitting terminal events or owner-decision records.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The transition mutated the campaign.
    Applied,
    /// The campaign was already in the requested terminal state; nothing
    /// changed.
    AlreadySettled,
}

/// Why a requested campaign status transition is not legal.
///
/// Every message here is the literal wording the daemon's gRPC handlers have
/// always returned, preserved verbatim so this refactor changes no
/// operator-visible behaviour on the online path — see
/// `book/src/guide/campaigns.md` for the status transition table these
/// variants encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// The campaign has no `authorized_by` owner, and the requested operation
    /// requires one.
    Unauthorized { name: String, detail: &'static str },
    /// The campaign is not in a status the requested operation accepts.
    WrongStatus {
        name: String,
        status: CampaignStatus,
        op: &'static str,
        requires: &'static str,
    },
    /// `complete` was requested on a `Cancelled` campaign. Cancellation is
    /// terminal and distinct from completion: recording it as complete would
    /// be a false evidence claim.
    CancelledCannotComplete { name: String },
    /// The campaign is already `Completed`, and the requested operation (only
    /// `cancel` reaches this — `complete` on an already-`Completed` campaign
    /// is `Transition::AlreadySettled`, not an error) has nothing left to act
    /// on.
    AlreadyCompleted { name: String },
    /// `resume` was requested with `add_cycles == 0` on a campaign that has
    /// already used its full budget.
    BudgetExhausted { name: String },
    /// Applying `add_cycles` to `budget.max_cycles` would overflow.
    BudgetOverflow { name: String, add_cycles: u64 },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { name, detail } => {
                write!(f, "campaign '{name}' has not been authorized; {detail}")
            }
            Self::WrongStatus {
                name,
                status,
                op,
                requires,
            } => write!(f, "campaign '{name}' is '{status}'; {op} requires {requires} status"),
            Self::CancelledCannotComplete { name } => write!(
                f,
                "campaign '{name}' was cancelled; a cancelled campaign cannot be recorded as complete"
            ),
            Self::AlreadyCompleted { name } => write!(
                f,
                "campaign '{name}' is already completed; there is nothing in flight to cancel"
            ),
            Self::BudgetExhausted { name } => write!(
                f,
                "campaign '{name}' exhausted its cycle budget; pass add_cycles > 0 to authorize more work"
            ),
            Self::BudgetOverflow { name, add_cycles } => {
                write!(
                    f,
                    "add_cycles ({add_cycles}) would overflow max_cycles for campaign '{name}'"
                )
            }
        }
    }
}

impl std::error::Error for TransitionError {}

impl Campaign {
    /// Pause a campaign, preserving any stored `pending_run_result`.
    ///
    /// Unconditional today: every status, including `Cancelled`, accepts
    /// `pause`. That this can resurrect a cancelled campaign into `Paused` is
    /// a known open question — see the `TODO(campaign-status)` on this
    /// method — not a decision made here.
    // TODO(campaign-status): pause is unconditional, so it can move a
    // `Cancelled` campaign to `Paused`, resurrecting a terminal status. This
    // is the same bug class as the defect this module fixes for `complete`
    // and `resume`. Left unchanged pending an explicit decision: making
    // `pause` reject `Cancelled` (and `Completed`) changes public gRPC
    // semantics and needs sign-off, not a silent fix bundled into a
    // refactor.
    pub fn pause(&mut self) {
        self.status = CampaignStatus::Paused;
        // pending_run_result is intentionally left untouched.
    }

    /// Resume a paused or escalated campaign, optionally extending its cycle
    /// budget.
    ///
    /// Requires `authorized_by` to be set. Valid only when the campaign
    /// status is `Paused` or `Escalated`. When `add_cycles == 0` and
    /// `cycles_completed >= budget.max_cycles` the budget is exhausted; the
    /// caller must pass a positive `add_cycles` to explicitly authorize more
    /// work. `add_cycles` is applied to `budget.max_cycles` via checked
    /// addition; overflow is rejected.
    ///
    /// `pending_run_result` is intentionally left untouched — it remains
    /// pending for the next manual advance to consume.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when the campaign is unauthorized, not
    /// `Paused` or `Escalated`, has exhausted its budget with no
    /// `add_cycles` requested, or `add_cycles` would overflow.
    pub fn resume(&mut self, add_cycles: u64) -> Result<(), TransitionError> {
        if self.authorized_by.is_none() {
            return Err(TransitionError::Unauthorized {
                name: self.name.clone(),
                detail: "resume requires an authorized_by owner",
            });
        }
        if self.status != CampaignStatus::Paused && self.status != CampaignStatus::Escalated {
            return Err(TransitionError::WrongStatus {
                name: self.name.clone(),
                status: self.status,
                op: "resume",
                requires: "Paused or Escalated",
            });
        }
        if add_cycles == 0 && self.cycles_completed >= self.budget.max_cycles {
            return Err(TransitionError::BudgetExhausted {
                name: self.name.clone(),
            });
        }
        if add_cycles > 0 {
            self.budget.max_cycles =
                self.budget.max_cycles.checked_add(add_cycles).ok_or_else(|| {
                    TransitionError::BudgetOverflow {
                        name: self.name.clone(),
                        add_cycles,
                    }
                })?;
        }
        self.status = CampaignStatus::Active;
        // pending_run_result is intentionally left untouched.
        Ok(())
    }

    /// Record an owner decision on an escalated campaign and return it to
    /// active state.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when the campaign is unauthorized or not
    /// `Escalated`.
    pub fn record_owner_decision(
        &mut self,
        decision: String,
        at: DateTime<Utc>,
    ) -> Result<(), TransitionError> {
        let authorized_by =
            self.authorized_by.clone().ok_or_else(|| TransitionError::Unauthorized {
                name: self.name.clone(),
                detail: "decide requires authorized_by",
            })?;
        if self.status != CampaignStatus::Escalated {
            return Err(TransitionError::WrongStatus {
                name: self.name.clone(),
                status: self.status,
                op: "decide",
                requires: "Escalated",
            });
        }
        self.owner_decisions.push(OwnerDecision {
            decision,
            authorized_by,
            decided_at: at,
        });
        self.status = CampaignStatus::Active;
        Ok(())
    }

    /// Mark an authorized campaign complete outside the formation loop.
    ///
    /// Guard ordering is load-bearing: `authorized_by` is checked first, then
    /// `Completed` (idempotent — returns `AlreadySettled`), then `Cancelled`
    /// (rejected — cancellation is terminal and distinct from completion).
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when the campaign is unauthorized or
    /// `Cancelled`.
    pub fn complete(
        &mut self,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<Transition, TransitionError> {
        let authorized_by =
            self.authorized_by.clone().ok_or_else(|| TransitionError::Unauthorized {
                name: self.name.clone(),
                detail: "complete requires authorized_by",
            })?;
        if self.status == CampaignStatus::Completed {
            return Ok(Transition::AlreadySettled);
        }
        if self.status == CampaignStatus::Cancelled {
            return Err(TransitionError::CancelledCannotComplete {
                name: self.name.clone(),
            });
        }
        self.owner_decisions.push(OwnerDecision {
            decision: format!("Completed externally: {reason}"),
            authorized_by,
            decided_at: at,
        });
        self.status = CampaignStatus::Completed;
        self.pending_run_result = None;
        Ok(Transition::Applied)
    }

    /// Fast, read-only precheck for whether `cancel` has anything to do.
    ///
    /// Exists so the daemon can fail fast before its blocking abort call
    /// (`WorkflowTracker::abort_campaign`) without holding the exclusive
    /// store lock across it — see `campaign_ops::cancel` for why that
    /// ordering matters. Carries the same rule `cancel` re-checks under the
    /// lock, so this is a pure optimization, not a second copy of the
    /// decision.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::AlreadyCompleted`] when the campaign is
    /// already `Completed`.
    pub fn check_cancellable(&self) -> Result<Transition, TransitionError> {
        if self.status == CampaignStatus::Completed {
            return Err(TransitionError::AlreadyCompleted {
                name: self.name.clone(),
            });
        }
        if self.status == CampaignStatus::Cancelled {
            return Ok(Transition::AlreadySettled);
        }
        Ok(Transition::Applied)
    }

    /// Stop a campaign permanently.
    ///
    /// Deliberately does not require `authorized_by` — see the rationale on
    /// the daemon's `cancel` handler: refusing to stop an unauthorized
    /// campaign would strand it, since it can never be advanced to
    /// completion either. The reason and event still carry regardless; only
    /// the owner-decision record needs an owner to attach itself to.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::AlreadyCompleted`] when the campaign is
    /// already `Completed`.
    pub fn cancel(
        &mut self,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<Transition, TransitionError> {
        match self.check_cancellable()? {
            Transition::AlreadySettled => return Ok(Transition::AlreadySettled),
            Transition::Applied => {}
        }
        if let Some(authorized_by) = self.authorized_by.clone() {
            self.owner_decisions.push(OwnerDecision {
                decision: format!("Cancelled externally: {reason}"),
                authorized_by,
                decided_at: at,
            });
        }
        self.status = CampaignStatus::Cancelled;
        self.pending_run_result = None;
        Ok(Transition::Applied)
    }

    /// Whether `advance` may run against this campaign's current status.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::WrongStatus`] when the campaign is not
    /// `Active` or `Staged`.
    pub fn check_advanceable(&self) -> Result<(), TransitionError> {
        match self.status {
            CampaignStatus::Active | CampaignStatus::Staged => Ok(()),
            status => Err(TransitionError::WrongStatus {
                name: self.name.clone(),
                status,
                op: "advance",
                requires: "Active or Staged",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campaign::{CampaignBudget, DoneEvidence};

    fn campaign(status: CampaignStatus, authorized: bool) -> Campaign {
        Campaign {
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 2 },
            escalation: vec![],
            status,
            cycles_completed: 0,
            cycles_landed: 0,
            authorized_by: authorized.then(|| "owner".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        }
    }

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    const ALL_STATUSES: [CampaignStatus; 6] = [
        CampaignStatus::Staged,
        CampaignStatus::Active,
        CampaignStatus::Paused,
        CampaignStatus::Escalated,
        CampaignStatus::Completed,
        CampaignStatus::Cancelled,
    ];

    /// This table is the state machine specification. Every status is tried
    /// against every operation; the outcome is either a specific error, a
    /// `Transition` result, or (for `pause`, infallible) always applied.
    #[test]
    fn resume_table() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            let result = c.resume(0);
            match status {
                CampaignStatus::Paused | CampaignStatus::Escalated => {
                    // budget not exhausted (0 completed, max 2) -> Ok
                    assert!(result.is_ok(), "{status:?} + resume(0) should apply");
                    assert_eq!(c.status, CampaignStatus::Active);
                }
                _ => {
                    assert_eq!(
                        result,
                        Err(TransitionError::WrongStatus {
                            name: "c".to_string(),
                            status,
                            op: "resume",
                            requires: "Paused or Escalated",
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn resume_requires_authorization() {
        let mut c = campaign(CampaignStatus::Paused, false);
        assert_eq!(
            c.resume(0),
            Err(TransitionError::Unauthorized {
                name: "c".to_string(),
                detail: "resume requires an authorized_by owner",
            })
        );
    }

    #[test]
    fn resume_rejects_exhausted_budget_without_add_cycles() {
        let mut c = campaign(CampaignStatus::Paused, true);
        c.cycles_completed = 2;
        assert_eq!(
            c.resume(0),
            Err(TransitionError::BudgetExhausted {
                name: "c".to_string()
            })
        );
    }

    #[test]
    fn resume_rejects_overflowing_add_cycles() {
        let mut c = campaign(CampaignStatus::Paused, true);
        c.budget.max_cycles = u64::MAX;
        assert_eq!(
            c.resume(1),
            Err(TransitionError::BudgetOverflow {
                name: "c".to_string(),
                add_cycles: 1,
            })
        );
    }

    #[test]
    fn decide_table() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            let result = c.record_owner_decision("proceed".to_string(), at());
            if status == CampaignStatus::Escalated {
                assert!(result.is_ok());
                assert_eq!(c.status, CampaignStatus::Active);
                assert_eq!(c.owner_decisions.len(), 1);
            } else {
                assert_eq!(
                    result,
                    Err(TransitionError::WrongStatus {
                        name: "c".to_string(),
                        status,
                        op: "decide",
                        requires: "Escalated",
                    })
                );
            }
        }
    }

    #[test]
    fn decide_requires_authorization() {
        let mut c = campaign(CampaignStatus::Escalated, false);
        assert_eq!(
            c.record_owner_decision("proceed".to_string(), at()),
            Err(TransitionError::Unauthorized {
                name: "c".to_string(),
                detail: "decide requires authorized_by",
            })
        );
    }

    /// The state machine specification for `complete`, including the exact
    /// defect this refactor fixes: `Cancelled` must be rejected, not silently
    /// flipped to `Completed`.
    #[test]
    fn complete_table() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            let result = c.complete("done", at());
            match status {
                CampaignStatus::Completed => assert_eq!(result, Ok(Transition::AlreadySettled)),
                CampaignStatus::Cancelled => assert_eq!(
                    result,
                    Err(TransitionError::CancelledCannotComplete {
                        name: "c".to_string()
                    })
                ),
                _ => {
                    assert_eq!(result, Ok(Transition::Applied));
                    assert_eq!(c.status, CampaignStatus::Completed);
                    assert!(c.pending_run_result.is_none());
                }
            }
        }
    }

    #[test]
    fn complete_requires_authorization() {
        let mut c = campaign(CampaignStatus::Active, false);
        assert_eq!(
            c.complete("done", at()),
            Err(TransitionError::Unauthorized {
                name: "c".to_string(),
                detail: "complete requires authorized_by",
            })
        );
    }

    #[test]
    fn cancel_table() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            let result = c.cancel("stop", at());
            match status {
                CampaignStatus::Completed => assert_eq!(
                    result,
                    Err(TransitionError::AlreadyCompleted {
                        name: "c".to_string()
                    })
                ),
                CampaignStatus::Cancelled => assert_eq!(result, Ok(Transition::AlreadySettled)),
                _ => {
                    assert_eq!(result, Ok(Transition::Applied));
                    assert_eq!(c.status, CampaignStatus::Cancelled);
                }
            }
        }
    }

    /// Cancellation does not require an owner: an unauthorized campaign must
    /// still be stoppable, or it would be permanently stranded.
    #[test]
    fn cancel_does_not_require_authorization() {
        let mut c = campaign(CampaignStatus::Active, false);
        assert_eq!(c.cancel("stop", at()), Ok(Transition::Applied));
        assert_eq!(c.status, CampaignStatus::Cancelled);
        assert!(c.owner_decisions.is_empty());
    }

    #[test]
    fn advance_table() {
        for status in ALL_STATUSES {
            let c = campaign(status, true);
            let result = c.check_advanceable();
            match status {
                CampaignStatus::Active | CampaignStatus::Staged => assert!(result.is_ok()),
                _ => assert_eq!(
                    result,
                    Err(TransitionError::WrongStatus {
                        name: "c".to_string(),
                        status,
                        op: "advance",
                        requires: "Active or Staged",
                    })
                ),
            }
        }
    }

    #[test]
    fn pause_is_unconditional() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            c.pause();
            assert_eq!(c.status, CampaignStatus::Paused);
        }
    }

    #[test]
    fn transition_error_messages_match_the_daemon_wording_lifted_verbatim() {
        assert_eq!(
            TransitionError::Unauthorized {
                name: "c".to_string(),
                detail: "resume requires an authorized_by owner"
            }
            .to_string(),
            "campaign 'c' has not been authorized; resume requires an authorized_by owner"
        );
        assert_eq!(
            TransitionError::WrongStatus {
                name: "c".to_string(),
                status: CampaignStatus::Active,
                op: "resume",
                requires: "Paused or Escalated"
            }
            .to_string(),
            "campaign 'c' is 'active'; resume requires Paused or Escalated status"
        );
        assert_eq!(
            TransitionError::CancelledCannotComplete {
                name: "c".to_string()
            }
            .to_string(),
            "campaign 'c' was cancelled; a cancelled campaign cannot be recorded as complete"
        );
        assert_eq!(
            TransitionError::AlreadyCompleted {
                name: "c".to_string()
            }
            .to_string(),
            "campaign 'c' is already completed; there is nothing in flight to cancel"
        );
        assert_eq!(
            TransitionError::BudgetExhausted {
                name: "c".to_string()
            }
            .to_string(),
            "campaign 'c' exhausted its cycle budget; pass add_cycles > 0 to authorize more work"
        );
        assert_eq!(
            TransitionError::BudgetOverflow {
                name: "c".to_string(),
                add_cycles: 5
            }
            .to_string(),
            "add_cycles (5) would overflow max_cycles for campaign 'c'"
        );
    }
}
