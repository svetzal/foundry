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
    /// The initiative owns this campaign's cycle budget.
    MemberBudgetOwned { name: String, initiative: String },
    /// An initiative member has spent its budget and no `add_cycles` was
    /// requested. Only the initiative can authorize more cycles, so the owner
    /// is pointed there rather than at the member.
    MemberBudgetSpent { name: String, initiative: String },
    /// Applying `add_cycles` to `budget.max_cycles` would overflow.
    BudgetOverflow { name: String, add_cycles: u64 },
    /// `decide` with `add_cycles > 0` on an `Active` or `Paused` campaign.
    /// Budget extension through `decide` answers an escalation; a paused
    /// campaign takes more cycles through `resume`.
    AddCyclesNeedEscalation {
        name: String,
        status: CampaignStatus,
        add_cycles: u64,
    },
    /// `decide` repeats the campaign's latest owner decision, with the same
    /// text and owner, on a campaign that is not waiting on a new escalation.
    /// This is how a retried decide is refused once the first one succeeded.
    DuplicateDecision { name: String },
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
            Self::MemberBudgetOwned { name, initiative } => write!(
                f,
                "campaign '{name}' belongs to initiative '{initiative}'; its cycle budget is owned by the initiative"
            ),
            Self::MemberBudgetSpent { name, initiative } => write!(
                f,
                "campaign '{name}' belongs to initiative '{initiative}', whose cycle budget is spent; decide on the initiative with add_cycles > 0"
            ),
            Self::BudgetOverflow { name, add_cycles } => {
                write!(
                    f,
                    "add_cycles ({add_cycles}) would overflow max_cycles for campaign '{name}'"
                )
            }
            Self::AddCyclesNeedEscalation {
                name,
                status,
                add_cycles,
            } => {
                write!(
                    f,
                    "campaign '{name}' is '{status}'; decide adds cycles only to an escalated campaign; "
                )?;
                if *status == CampaignStatus::Paused {
                    write!(
                        f,
                        "run `foundry campaign resume {name} --add-cycles {add_cycles}` to extend a paused campaign's budget"
                    )
                } else {
                    write!(f, "record the decision without add_cycles")
                }
            }
            Self::DuplicateDecision { name } => write!(
                f,
                "campaign '{name}' already records this owner decision as its latest; nothing to apply"
            ),
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
        self.open_escalation = None;
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
    /// Members of an initiative reject positive `add_cycles`: the initiative
    /// owns their cycle budget. A spent member's refusal names the initiative.
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
        self.budget.max_cycles = self.extended_budget(add_cycles)?;
        self.status = CampaignStatus::Active;
        self.open_escalation = None;
        // pending_run_result is intentionally left untouched.
        Ok(())
    }

    /// Record a binding owner decision on a campaign.
    ///
    /// The decision is appended to `owner_decisions`, which every later
    /// formation prompt treats as binding. What else happens depends on the
    /// status:
    ///
    /// - `Active` or `Paused`: nothing else changes. The status, budget,
    ///   `open_escalation` and `pending_run_result` stay as they are, so an
    ///   owner can steer the next formation without stopping the campaign.
    ///   The spent-budget rule does not apply, because no status changes and
    ///   nothing is stranded. `add_cycles > 0` is refused: extending the
    ///   budget through `decide` answers an escalation, and a paused campaign
    ///   takes more cycles through `resume`.
    /// - `Escalated`: the decision answers the escalation. The campaign
    ///   returns to `Active`, `open_escalation` is cleared, and `add_cycles`
    ///   extends the budget in the same transition. When `cycles_completed >=
    ///   budget.max_cycles` the budget is spent and `add_cycles` must be
    ///   positive: an active campaign with no cycle left would escalate on
    ///   budget at its next advance. `add_cycles` is applied through checked
    ///   addition; overflow is rejected.
    /// - `Staged`, `Completed` or `Cancelled`: refused.
    ///
    /// A retried decide is refused rather than recording the decision or
    /// adding the cycles twice. On an `Active` or `Paused` campaign, a decision
    /// whose text and owner equal the latest recorded decision is refused as
    /// [`TransitionError::DuplicateDecision`]. A successful escalated decide
    /// leaves the campaign `Active`, so its retry meets this check before the
    /// `add_cycles` refusal. An `Escalated` campaign is exempt: it has stopped
    /// on a new question since its latest decision, and the same words may
    /// answer it.
    ///
    /// Initiative members reject positive `add_cycles` because their budget
    /// belongs to the initiative. A spent escalated member refuses a decision
    /// without cycles too, and the refusal names the initiative: deciding
    /// there with `add_cycles > 0` is the only control that authorizes more
    /// work. Every check runs before anything is written, so a refusal records
    /// nothing. `pending_run_result` is never touched.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when the campaign is unauthorized, not
    /// `Active`, `Paused` or `Escalated`, repeats its latest decision, asks for
    /// cycles while not escalated, has spent its budget while escalated with
    /// no `add_cycles` requested, or `add_cycles` would overflow.
    pub fn record_owner_decision(
        &mut self,
        decision: String,
        at: DateTime<Utc>,
        add_cycles: u64,
    ) -> Result<(), TransitionError> {
        let authorized_by =
            self.authorized_by.clone().ok_or_else(|| TransitionError::Unauthorized {
                name: self.name.clone(),
                detail: "decide requires authorized_by",
            })?;
        let record = |decision| OwnerDecision {
            decision,
            authorized_by: authorized_by.clone(),
            decided_at: at,
        };
        match self.status {
            CampaignStatus::Active | CampaignStatus::Paused => {
                if self.latest_decision_is(&decision, &authorized_by) {
                    return Err(TransitionError::DuplicateDecision {
                        name: self.name.clone(),
                    });
                }
                if add_cycles > 0 {
                    return Err(match &self.initiative {
                        Some(initiative) => TransitionError::MemberBudgetOwned {
                            name: self.name.clone(),
                            initiative: initiative.clone(),
                        },
                        None => TransitionError::AddCyclesNeedEscalation {
                            name: self.name.clone(),
                            status: self.status,
                            add_cycles,
                        },
                    });
                }
                self.owner_decisions.push(record(decision));
            }
            CampaignStatus::Escalated => {
                let max_cycles = self.extended_budget(add_cycles)?;
                self.owner_decisions.push(record(decision));
                self.budget.max_cycles = max_cycles;
                self.status = CampaignStatus::Active;
                self.open_escalation = None;
            }
            status @ (CampaignStatus::Staged
            | CampaignStatus::Completed
            | CampaignStatus::Cancelled) => {
                return Err(TransitionError::WrongStatus {
                    name: self.name.clone(),
                    status,
                    op: "decide",
                    requires: "Active, Paused or Escalated",
                });
            }
        }
        Ok(())
    }

    /// Whether the latest recorded owner decision has this text, ignoring
    /// surrounding whitespace as the callers do, and this owner.
    fn latest_decision_is(&self, decision: &str, authorized_by: &str) -> bool {
        self.owner_decisions.last().is_some_and(|latest| {
            latest.decision.trim() == decision.trim() && latest.authorized_by == authorized_by
        })
    }

    /// The cycle budget after an owner authorizes `add_cycles` more, without
    /// applying it.
    ///
    /// Shared by `resume` and an escalated `decide`, the two controls that
    /// return a stopped campaign to active: a spent budget needs a positive
    /// `add_cycles`, and the addition is checked.
    fn extended_budget(&self, add_cycles: u64) -> Result<u64, TransitionError> {
        if add_cycles > 0
            && let Some(initiative) = &self.initiative
        {
            return Err(TransitionError::MemberBudgetOwned {
                name: self.name.clone(),
                initiative: initiative.clone(),
            });
        }
        if add_cycles == 0 && self.cycles_completed >= self.budget.max_cycles {
            let name = self.name.clone();
            return Err(match &self.initiative {
                Some(initiative) => TransitionError::MemberBudgetSpent {
                    name,
                    initiative: initiative.clone(),
                },
                None => TransitionError::BudgetExhausted { name },
            });
        }
        self.budget.max_cycles.checked_add(add_cycles).ok_or_else(|| {
            TransitionError::BudgetOverflow {
                name: self.name.clone(),
                add_cycles,
            }
        })
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
        self.open_escalation = None;
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
        self.open_escalation = None;
        self.pending_run_result = None;
        Ok(Transition::Applied)
    }

    /// The controls the owner can run on this campaign now; see
    /// [`owner_options`].
    #[must_use]
    pub fn owner_options(&self) -> Vec<OwnerOption> {
        let mut options = owner_options(
            &self.name,
            self.status,
            self.authorized_by.is_some(),
            self.cycles_completed >= self.budget.max_cycles,
        );
        if let Some(initiative) = &self.initiative {
            options.retain(|option| !option.command.contains("--add-cycles"));
            if !self.status.is_terminal() {
                options.push(OwnerOption {
                    control: "show".into(),
                    command: format!("foundry initiative show {initiative}"),
                    effect: "review the initiative's shared budget and owner questions".into(),
                });
            }
        }
        options
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

/// One control the owner can run on a campaign now: the command line and
/// what it does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwnerOption {
    /// The control the command runs: `advance`, `pause`, `resume`, `decide`,
    /// `complete` or `cancel`. Absent from a report an older daemon wrote.
    #[serde(default)]
    pub control: String,
    /// The command line, with `<text>`-style placeholders for owner input.
    pub command: String,
    /// What running it does, in a few words.
    pub effect: String,
}

/// The controls the owner can run on a campaign in `status`, as command lines.
///
/// The verbs per status are the "Valid next control" column of the status
/// table in `book/src/guide/campaigns.md`, and every one is legal under the
/// [`Campaign`] transition methods (a test holds the two together). `resume`,
/// `decide` and `complete` need an `authorized_by` owner, so they are left out
/// for an unauthorized campaign rather than offered and refused. When the cycle
/// budget is spent, `resume` and an escalated `decide` are offered with
/// `--add-cycles`, the only form the engine accepts. An active or paused
/// `decide` is always offered without `--add-cycles`, the only form the engine
/// accepts there: it records the decision and changes nothing else.
#[must_use]
pub fn owner_options(
    name: &str,
    status: CampaignStatus,
    authorized: bool,
    budget_exhausted: bool,
) -> Vec<OwnerOption> {
    let option = |command: String, effect: &str| OwnerOption {
        control: command.split_whitespace().nth(2).unwrap_or_default().to_string(),
        command,
        effect: effect.to_string(),
    };
    let advance = || {
        option(
            format!("foundry campaign advance {name}"),
            "derive and dispatch the next objective",
        )
    };
    let pause = || option(format!("foundry campaign pause {name}"), "stop automatic advancement");
    let resume = || {
        if budget_exhausted {
            option(
                format!("foundry campaign resume {name} --add-cycles <n>"),
                "authorize n more cycles and return to active; advance continues it",
            )
        } else {
            option(
                format!("foundry campaign resume {name}"),
                "return to active without a decision; advance continues it",
            )
        }
    };
    let steer = || {
        option(
            format!("foundry campaign decide {name} --decision \"<text>\""),
            "record a binding owner decision for the next formation; the status does not change",
        )
    };
    let decide = || {
        if budget_exhausted {
            option(
                format!("foundry campaign decide {name} --decision \"<text>\" --add-cycles <n>"),
                "record a binding owner decision, authorize n more cycles and return to active; advance continues it",
            )
        } else {
            option(
                format!("foundry campaign decide {name} --decision \"<text>\""),
                "record a binding owner decision and return to active; advance continues it",
            )
        }
    };
    let complete = || {
        option(
            format!("foundry campaign complete {name} --reason \"<text>\""),
            "close it: owner-verified evidence shows the mission shipped",
        )
    };
    let cancel = || {
        option(
            format!("foundry campaign cancel {name} --reason \"<text>\""),
            "stop it permanently",
        )
    };
    let mut options = Vec::new();
    match status {
        CampaignStatus::Staged => {
            options.extend([advance(), pause()]);
        }
        CampaignStatus::Active => {
            options.extend([advance(), pause()]);
            if authorized {
                options.push(steer());
            }
        }
        CampaignStatus::Paused => {
            if authorized {
                options.extend([resume(), steer(), complete()]);
            }
        }
        CampaignStatus::Escalated => {
            if authorized {
                options.extend([decide(), resume(), complete()]);
            }
        }
        CampaignStatus::Completed | CampaignStatus::Cancelled => return options,
    }
    options.push(cancel());
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campaign::{CampaignBudget, DoneEvidence};

    fn campaign(status: CampaignStatus, authorized: bool) -> Campaign {
        Campaign {
            recovery: None,
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            initiative: None,
            providers: crate::gateway::StageProviders::default(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget {
                max_cycles: 2,
                ..Default::default()
            },
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
            open_escalation: None,
            writable_repositories: vec![],
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
    fn member_owner_options_offer_the_initiative_instead_of_member_budget_extensions() {
        let mut campaign = campaign(CampaignStatus::Escalated, true);
        campaign.initiative = Some("shared".into());
        campaign.cycles_completed = campaign.budget.max_cycles;
        let options = campaign.owner_options();
        assert!(options.iter().all(|option| !option.command.contains("--add-cycles")));
        assert!(options.iter().any(|option| option.command == "foundry initiative show shared"));
    }

    #[test]
    fn members_cannot_extend_their_budget_through_resume_or_decide() {
        for status in [CampaignStatus::Paused, CampaignStatus::Escalated] {
            let mut campaign = campaign(status, true);
            campaign.initiative = Some("shared".into());
            let before = serde_json::to_string(&campaign).unwrap();
            assert_eq!(
                campaign.resume(1),
                Err(TransitionError::MemberBudgetOwned {
                    name: "c".into(),
                    initiative: "shared".into()
                })
            );
            assert_eq!(serde_json::to_string(&campaign).unwrap(), before);
            if status == CampaignStatus::Escalated {
                assert_eq!(
                    campaign.record_owner_decision("Continue".into(), at(), 1),
                    Err(TransitionError::MemberBudgetOwned {
                        name: "c".into(),
                        initiative: "shared".into()
                    })
                );
                assert_eq!(serde_json::to_string(&campaign).unwrap(), before);
            }
            assert!(campaign.resume(0).is_ok());
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

    /// The state machine specification for `decide`. Active and paused
    /// campaigns record the decision and keep their status; an escalated one
    /// records it and returns to active; staged and terminal campaigns refuse
    /// and record nothing.
    #[test]
    fn decide_table() {
        for status in ALL_STATUSES {
            let mut c = campaign(status, true);
            let before = serde_json::to_value(&c).unwrap();
            let result = c.record_owner_decision("proceed".to_string(), at(), 0);
            match status {
                CampaignStatus::Active | CampaignStatus::Paused => {
                    assert_eq!(result, Ok(()), "{status:?}");
                    assert_eq!(c.status, status, "decide keeps an {status:?} campaign {status:?}");
                    assert_eq!(c.owner_decisions.len(), 1);
                }
                CampaignStatus::Escalated => {
                    assert_eq!(result, Ok(()));
                    assert_eq!(c.status, CampaignStatus::Active);
                    assert_eq!(c.owner_decisions.len(), 1);
                }
                CampaignStatus::Staged | CampaignStatus::Completed | CampaignStatus::Cancelled => {
                    assert_eq!(
                        result,
                        Err(TransitionError::WrongStatus {
                            name: "c".to_string(),
                            status,
                            op: "decide",
                            requires: "Active, Paused or Escalated",
                        })
                    );
                    assert_eq!(
                        serde_json::to_value(&c).unwrap(),
                        before,
                        "a refusal records nothing"
                    );
                }
            }
        }
    }

    /// An active or paused campaign with a spent budget, an open escalation
    /// record and a pending run result: everything the escalated decide would
    /// touch.
    fn running_with_everything_decide_could_touch(status: CampaignStatus) -> Campaign {
        let mut c = escalated_with_record();
        c.status = status;
        c.cycles_completed = c.budget.max_cycles;
        c.pending_run_result = Some(landed_run_result());
        c
    }

    /// Recording a decision on an active or paused campaign appends exactly one
    /// owner decision and changes nothing else on the record, even with a
    /// spent budget: no status changes, so nothing is stranded.
    #[test]
    fn decide_on_an_active_or_paused_campaign_records_and_changes_nothing_else() {
        for status in [CampaignStatus::Active, CampaignStatus::Paused] {
            let mut c = running_with_everything_decide_could_touch(status);
            let mut expected = serde_json::to_value(&c).unwrap();

            c.record_owner_decision("keep the v1 endpoint".to_string(), at(), 0).unwrap();

            expected["owner_decisions"] = serde_json::json!([{
                "decision": "keep the v1 endpoint",
                "authorized_by": "owner",
                "decided_at": at(),
            }]);
            assert_eq!(serde_json::to_value(&c).unwrap(), expected, "{status:?}");
        }
    }

    /// Budget extension stays bound to the escalated decide and to `resume`.
    /// The refusal tells the owner of a paused campaign which command adds the
    /// cycles, and records nothing.
    #[test]
    fn decide_with_add_cycles_on_an_active_or_paused_campaign_is_refused_and_records_nothing() {
        for status in [CampaignStatus::Active, CampaignStatus::Paused] {
            let mut c = running_with_everything_decide_could_touch(status);
            let before = serde_json::to_value(&c).unwrap();

            assert_eq!(
                c.record_owner_decision("keep going".to_string(), at(), 2),
                Err(TransitionError::AddCyclesNeedEscalation {
                    name: "c".to_string(),
                    status,
                    add_cycles: 2,
                })
            );
            assert_eq!(
                serde_json::to_value(&c).unwrap(),
                before,
                "{status:?}: a refusal records nothing"
            );
        }
    }

    /// A member's budget belongs to its initiative, so the member refusal is
    /// the one that applies whatever the member's status.
    #[test]
    fn a_member_decision_with_add_cycles_names_the_initiative_in_every_decidable_status() {
        for status in [
            CampaignStatus::Active,
            CampaignStatus::Paused,
            CampaignStatus::Escalated,
        ] {
            let mut c = campaign(status, true);
            c.initiative = Some("shared".to_string());
            let before = serde_json::to_value(&c).unwrap();
            assert_eq!(
                c.record_owner_decision("keep going".to_string(), at(), 1),
                Err(TransitionError::MemberBudgetOwned {
                    name: "c".to_string(),
                    initiative: "shared".to_string(),
                }),
                "{status:?}"
            );
            assert_eq!(serde_json::to_value(&c).unwrap(), before);
            c.record_owner_decision("keep going".to_string(), at(), 0).unwrap();
            assert_eq!(c.owner_decisions.len(), 1, "{status:?}: a member records its decision");
        }
    }

    /// Repeating the latest decision on an active or paused campaign is a
    /// retry, and is refused. A different decision, or the same words from an
    /// earlier decision that is no longer the latest, is recorded.
    #[test]
    fn only_a_repeat_of_the_latest_decision_is_refused_as_a_retry() {
        for status in [CampaignStatus::Active, CampaignStatus::Paused] {
            let mut c = campaign(status, true);
            c.record_owner_decision("prefer gRPC".to_string(), at(), 0).unwrap();
            let after_first = serde_json::to_value(&c).unwrap();

            assert_eq!(
                c.record_owner_decision("  prefer gRPC ".to_string(), at(), 0),
                Err(TransitionError::DuplicateDecision {
                    name: "c".to_string()
                }),
                "{status:?}"
            );
            assert_eq!(serde_json::to_value(&c).unwrap(), after_first);

            c.record_owner_decision("drop the JSON shim".to_string(), at(), 0).unwrap();
            c.record_owner_decision("prefer gRPC".to_string(), at(), 0).unwrap();
            assert_eq!(c.owner_decisions.len(), 3, "{status:?}");
            assert_eq!(c.status, status);
        }
    }

    /// An escalated campaign stopped on a new question after its latest
    /// decision, so the same words answer it and are recorded.
    #[test]
    fn an_escalated_campaign_accepts_the_same_words_as_its_latest_decision() {
        let mut c = escalated_with_record();
        c.owner_decisions.push(OwnerDecision {
            decision: "keep going".to_string(),
            authorized_by: "owner".to_string(),
            decided_at: at(),
        });
        c.record_owner_decision("keep going".to_string(), at(), 0).unwrap();
        assert_eq!(c.status, CampaignStatus::Active);
        assert_eq!(c.owner_decisions.len(), 2);
    }

    /// A budget-exhausted escalation answered with a decision and more cycles:
    /// one transition records the decision, extends the budget and returns the
    /// campaign to active. Nothing else on the record moves.
    #[test]
    fn decide_with_add_cycles_records_extends_and_activates() {
        let mut c = escalated_with_record();
        c.cycles_completed = 2;
        c.pending_run_result = Some(landed_run_result());
        let pending_before = serde_json::to_string(&c.pending_run_result).unwrap();

        c.record_owner_decision("keep going".to_string(), at(), 1).unwrap();

        assert_eq!(c.status, CampaignStatus::Active);
        assert_eq!(c.budget.max_cycles, 3);
        assert_eq!(c.owner_decisions.len(), 1);
        assert_eq!(c.owner_decisions[0].decision, "keep going");
        assert!(c.open_escalation.is_none());
        assert_eq!(serde_json::to_string(&c.pending_run_result).unwrap(), pending_before);
    }

    /// Without more cycles, a decision on a spent budget would leave an active
    /// campaign whose next advance escalates on budget again, and `resume`
    /// refuses an active campaign. So it is refused, and nothing is recorded.
    #[test]
    fn decide_on_a_spent_budget_without_add_cycles_is_refused_and_records_nothing() {
        let mut c = escalated_with_record();
        c.cycles_completed = 2;
        let before = serde_json::to_value(&c).unwrap();

        assert_eq!(
            c.record_owner_decision("keep going".to_string(), at(), 0),
            Err(TransitionError::BudgetExhausted {
                name: "c".to_string()
            })
        );
        assert_eq!(serde_json::to_value(&c).unwrap(), before, "a refusal changes nothing");
    }

    /// A spent member cannot take more cycles itself, so telling the owner to
    /// pass `add_cycles` here would only lead to a second refusal. The refusal
    /// names the initiative that owns the budget instead.
    #[test]
    fn spent_member_decision_without_cycles_points_at_the_initiative() {
        let mut c = escalated_with_record();
        c.initiative = Some("shared".to_string());
        c.cycles_completed = 2;
        let before = serde_json::to_value(&c).unwrap();

        let refused = TransitionError::MemberBudgetSpent {
            name: "c".to_string(),
            initiative: "shared".to_string(),
        };
        assert_eq!(
            c.record_owner_decision("keep going".to_string(), at(), 0),
            Err(refused.clone())
        );
        assert_eq!(serde_json::to_value(&c).unwrap(), before, "a refusal changes nothing");

        c.status = CampaignStatus::Paused;
        let before = serde_json::to_value(&c).unwrap();
        assert_eq!(c.resume(0), Err(refused));
        assert_eq!(serde_json::to_value(&c).unwrap(), before, "a refusal changes nothing");
    }

    #[test]
    fn decide_rejects_overflowing_add_cycles_and_records_nothing() {
        let mut c = escalated_with_record();
        c.budget.max_cycles = u64::MAX;
        let before = serde_json::to_value(&c).unwrap();

        assert_eq!(
            c.record_owner_decision("keep going".to_string(), at(), 1),
            Err(TransitionError::BudgetOverflow {
                name: "c".to_string(),
                add_cycles: 1,
            })
        );
        assert_eq!(serde_json::to_value(&c).unwrap(), before, "a refusal changes nothing");
    }

    /// A successful escalated decide leaves the campaign active, so a retry
    /// of the same command lands on an active campaign and is refused as a
    /// repeat of its latest decision, before the `add_cycles` refusal: the
    /// decision is not recorded twice and the cycles are not added twice.
    #[test]
    fn a_retried_decide_after_success_is_refused_and_applies_nothing() {
        let mut c = escalated_with_record();
        c.cycles_completed = 2;
        c.record_owner_decision("keep going".to_string(), at(), 1).unwrap();
        let after_first = serde_json::to_value(&c).unwrap();

        assert_eq!(
            c.record_owner_decision("keep going".to_string(), at(), 1),
            Err(TransitionError::DuplicateDecision {
                name: "c".to_string()
            })
        );
        assert_eq!(serde_json::to_value(&c).unwrap(), after_first);
        assert_eq!(c.budget.max_cycles, 3);
        assert_eq!(c.owner_decisions.len(), 1);
    }

    fn landed_run_result() -> crate::payload::TaskRunCompletedPayload {
        crate::payload::TaskRunCompletedPayload {
            project: "p".to_string(),
            success: true,
            landed: true,
            summary: "landed".to_string(),
            preservation_ref: None,
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict: crate::payload::TaskVerdict::Complete,
            context: crate::payload::LoopContext::default(),
        }
    }

    #[test]
    fn decide_requires_authorization() {
        let mut c = campaign(CampaignStatus::Escalated, false);
        assert_eq!(
            c.record_owner_decision("proceed".to_string(), at(), 0),
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
        assert!(
            c.owner_decisions.is_empty(),
            "expected `c.owner_decisions` to be empty: {:?}",
            c.owner_decisions
        );
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
            TransitionError::MemberBudgetSpent {
                name: "c".to_string(),
                initiative: "shared".to_string()
            }
            .to_string(),
            "campaign 'c' belongs to initiative 'shared', whose cycle budget is spent; decide on the initiative with add_cycles > 0"
        );
        assert_eq!(
            TransitionError::BudgetOverflow {
                name: "c".to_string(),
                add_cycles: 5
            }
            .to_string(),
            "add_cycles (5) would overflow max_cycles for campaign 'c'"
        );
        assert_eq!(
            TransitionError::WrongStatus {
                name: "c".to_string(),
                status: CampaignStatus::Completed,
                op: "decide",
                requires: "Active, Paused or Escalated"
            }
            .to_string(),
            "campaign 'c' is 'completed'; decide requires Active, Paused or Escalated status"
        );
        assert_eq!(
            TransitionError::AddCyclesNeedEscalation {
                name: "c".to_string(),
                status: CampaignStatus::Paused,
                add_cycles: 2
            }
            .to_string(),
            "campaign 'c' is 'paused'; decide adds cycles only to an escalated campaign; run `foundry campaign resume c --add-cycles 2` to extend a paused campaign's budget"
        );
        assert_eq!(
            TransitionError::AddCyclesNeedEscalation {
                name: "c".to_string(),
                status: CampaignStatus::Active,
                add_cycles: 2
            }
            .to_string(),
            "campaign 'c' is 'active'; decide adds cycles only to an escalated campaign; record the decision without add_cycles"
        );
        assert_eq!(
            TransitionError::DuplicateDecision {
                name: "c".to_string()
            }
            .to_string(),
            "campaign 'c' already records this owner decision as its latest; nothing to apply"
        );
    }

    fn escalated_with_record() -> Campaign {
        let mut c = campaign(CampaignStatus::Escalated, true);
        c.open_escalation = Some(crate::campaign::CampaignEscalation {
            reason: "owner decision required".to_string(),
            escalated_at: at(),
            cycles_completed: 0,
            cycles_landed: 0,
            question: None,
            choices: vec![],
            from_event_log: false,
        });
        c
    }

    /// The record describes the question the owner has not answered yet, so
    /// every control that moves the campaign out of `escalated` clears it.
    #[test]
    fn every_transition_out_of_escalated_clears_the_open_escalation() {
        let mut c = escalated_with_record();
        c.pause();
        assert!(c.open_escalation.is_none(), "pause");

        let mut c = escalated_with_record();
        c.resume(0).unwrap();
        assert!(c.open_escalation.is_none(), "resume");

        let mut c = escalated_with_record();
        c.record_owner_decision("keep going".to_string(), at(), 0).unwrap();
        assert!(c.open_escalation.is_none(), "decide");

        let mut c = escalated_with_record();
        assert_eq!(c.complete("shipped", at()).unwrap(), Transition::Applied);
        assert!(c.open_escalation.is_none(), "complete");

        let mut c = escalated_with_record();
        assert_eq!(c.cancel("abandoned", at()).unwrap(), Transition::Applied);
        assert!(c.open_escalation.is_none(), "cancel");
    }

    /// Apply the control a command line names, the way the CLI would.
    fn apply_option(c: &mut Campaign, option: &OwnerOption) -> Result<(), TransitionError> {
        let command = option.command.as_str();
        match option.control.as_str() {
            "advance" => c.check_advanceable(),
            "pause" => {
                c.pause();
                Ok(())
            }
            "resume" => c.resume(u64::from(command.contains("--add-cycles"))),
            "decide" => c.record_owner_decision(
                "d".to_string(),
                at(),
                u64::from(command.contains("--add-cycles")),
            ),
            "complete" => c.complete("r", at()).map(|_| ()),
            "cancel" => c.cancel("r", at()).map(|_| ()),
            other => panic!("unknown verb {other}"),
        }
    }

    /// Every option offered is a control the transition methods accept, for
    /// every status, authorization and budget state. Offering a command the
    /// engine then refuses would send the owner in a circle.
    #[test]
    fn every_offered_owner_option_is_legal_for_its_campaign() {
        for status in CampaignStatus::ALL {
            for authorized in [true, false] {
                for exhausted in [true, false] {
                    let mut template = campaign(status, authorized);
                    if exhausted {
                        template.cycles_completed = template.budget.max_cycles;
                    }
                    for option in owner_options("c", status, authorized, exhausted) {
                        let mut c = template.clone();
                        assert!(
                            apply_option(&mut c, &option).is_ok(),
                            "{status} authorized={authorized} exhausted={exhausted}: `{}` is refused",
                            option.command
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn escalated_options_follow_the_status_table_with_real_command_lines() {
        let commands: Vec<String> =
            owner_options("mojentic-x", CampaignStatus::Escalated, true, false)
                .into_iter()
                .map(|o| o.command)
                .collect();
        assert_eq!(
            commands,
            [
                "foundry campaign decide mojentic-x --decision \"<text>\"",
                "foundry campaign resume mojentic-x",
                "foundry campaign complete mojentic-x --reason \"<text>\"",
                "foundry campaign cancel mojentic-x --reason \"<text>\"",
            ]
        );
        let exhausted = owner_options("mojentic-x", CampaignStatus::Escalated, true, true);
        assert_eq!(
            exhausted[0].command,
            "foundry campaign decide mojentic-x --decision \"<text>\" --add-cycles <n>",
            "on a spent budget, decide is offered only in the form the engine accepts"
        );
        assert_eq!(exhausted[0].control, "decide");
        assert_eq!(exhausted[1].command, "foundry campaign resume mojentic-x --add-cycles <n>");
        for (status, expected) in [
            (
                CampaignStatus::Active,
                vec![
                    "foundry campaign advance mojentic-x",
                    "foundry campaign pause mojentic-x",
                    "foundry campaign decide mojentic-x --decision \"<text>\"",
                    "foundry campaign cancel mojentic-x --reason \"<text>\"",
                ],
            ),
            (
                CampaignStatus::Paused,
                vec![
                    "foundry campaign resume mojentic-x --add-cycles <n>",
                    "foundry campaign decide mojentic-x --decision \"<text>\"",
                    "foundry campaign complete mojentic-x --reason \"<text>\"",
                    "foundry campaign cancel mojentic-x --reason \"<text>\"",
                ],
            ),
        ] {
            let commands: Vec<String> = owner_options("mojentic-x", status, true, true)
                .into_iter()
                .map(|o| o.command)
                .collect();
            assert_eq!(
                commands, expected,
                "{status}: decide records without --add-cycles even on a spent budget"
            );
        }
        assert!(
            owner_options("c", CampaignStatus::Active, false, false)
                .iter()
                .all(|option| option.control != "decide"),
            "decide needs an owner"
        );
        let unauthorized = owner_options("mojentic-x", CampaignStatus::Escalated, false, false);
        assert_eq!(unauthorized.len(), 1, "only cancel needs no owner");
        assert!(
            owner_options("c", CampaignStatus::Completed, true, false).is_empty(),
            "expected `owner_options(\"c\", CampaignStatus::Completed, true, false)` to be empty: {:?}",
            owner_options("c", CampaignStatus::Completed, true, false)
        );
        assert!(
            owner_options("c", CampaignStatus::Cancelled, true, false).is_empty(),
            "expected `owner_options(\"c\", CampaignStatus::Cancelled, true, false)` to be empty: {:?}",
            owner_options("c", CampaignStatus::Cancelled, true, false)
        );
    }

    #[test]
    fn status_tags_round_trip() {
        for status in CampaignStatus::ALL {
            assert_eq!(CampaignStatus::from_tag(&status.to_string()), Some(status));
        }
        assert_eq!(CampaignStatus::from_tag("bogus"), None);
    }
}
