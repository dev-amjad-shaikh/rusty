//! Editorial governance for automated skill producers (EP-07-S03): the
//! typed patch-before-create preference, the provenance every automated
//! mutation records, and the rung distribution the platform reports as the
//! taxonomy's standing health metric.
//!
//! A thousand background edits must accrete a taxonomy, not a junk drawer.
//! The governance here is structural, not advisory:
//!
//! - **[`PatchPreference`]** — the four rungs every skill-drafting prompt
//!   must prefer, in order: patch the loaded skill, patch an existing
//!   umbrella, add a support file, and only then create new. The prompt
//!   section is *generated from the enum* ([`render_patch_preference_section`])
//!   — there is no template to drift, so a change to the type is a change
//!   to every prompt the review fork and the hunting loop render.
//! - **The session-artifact name ban** — a skill named after a date, a
//!   ticket, or a one-off task dies with the session. The ban is typed
//!   ([`SessionArtifactNameClass`]) so the prompt text and the fail-closed
//!   classifier ([`classify_session_artifact_name`]) share one vocabulary:
//!   the prose the model reads and the check the producer runs never
//!   disagree about what is banned.
//! - **[`EditorialProvenance`]** — every automated `Create`/`Patch`
//!   mutation records which rung the producer landed on, and a
//!   `CreateNew` landing is impossible without the producer's stated
//!   reason the three patch rungs did not apply. The provenance travels
//!   beside the candidate's content address (attribution is not identity —
//!   the learn plane's rule), never inside it.
//! - **[`rung_distribution`]** — the health metric: how many automated
//!   mutations landed on each rung over a window. A distribution that
//!   drifts toward `CreateNew` is taxonomy pressure made visible.
//!
//! What this module does not do: render the rest of the review or hunting
//! prompt (the fork's and the loop's own slices), or schedule the curator
//! (idle-time scheduling is the gateway's; retention scoring and the
//! curator pass live in [`crate::skill_retention`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The most bytes a `CreateNew` justification may carry. Bounded so a
/// producer cannot smuggle a document into a ledger field — the reason is
/// a sentence, not a report.
pub const MAX_CREATE_NEW_JUSTIFICATION_LEN: usize = 512;

/// The typed patch-before-create preference (`contracts:skill`). The
/// declaration order *is* the preference order — [`PatchPreference::ALL`]
/// and the rendered prompt both walk it, so reordering the enum reorders
/// the prompt with no template edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchPreference {
    /// Patch the skill already loaded in this session — the draft extends
    /// what the run was using.
    PatchLoadedSkill,
    /// Patch an existing umbrella skill in the library — new material
    /// belongs under a general skill, not beside it.
    PatchExistingUmbrella,
    /// Add a support file (reference or asset) to an existing skill — new
    /// content without a new skill.
    AddSupportFile,
    /// Create a new skill — only when no patch rung applies, and only with
    /// a stated reason the three patch rungs did not apply.
    CreateNew,
}

impl PatchPreference {
    /// Every rung, in preference order. The prompt renderer and the rung
    /// distribution both walk this — one order, two consumers, no drift.
    pub const ALL: [PatchPreference; 4] = [
        PatchPreference::PatchLoadedSkill,
        PatchPreference::PatchExistingUmbrella,
        PatchPreference::AddSupportFile,
        PatchPreference::CreateNew,
    ];

    /// The wire name (`patch_loaded_skill`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            PatchPreference::PatchLoadedSkill => "patch_loaded_skill",
            PatchPreference::PatchExistingUmbrella => "patch_existing_umbrella",
            PatchPreference::AddSupportFile => "add_support_file",
            PatchPreference::CreateNew => "create_new",
        }
    }

    /// The 1-based position in the preference order.
    pub fn rung(&self) -> u64 {
        PatchPreference::ALL
            .iter()
            .position(|rung| rung == self)
            .expect("ALL is exhaustive") as u64
            + 1
    }

    /// The prompt instruction for this rung. The renderer calls this — the
    /// text lives on the variant, so the prompt can never describe a rung
    /// the enum does not have.
    pub fn instruction(&self) -> &'static str {
        match self {
            PatchPreference::PatchLoadedSkill => {
                "Patch the loaded skill — extend the skill this session already used \
                 before reaching for anything else."
            }
            PatchPreference::PatchExistingUmbrella => {
                "Patch an existing umbrella skill — new material belongs under a general \
                 skill in the library, not beside one."
            }
            PatchPreference::AddSupportFile => {
                "Add a support file to an existing skill — a reference or asset carries \
                 new content without minting a new skill."
            }
            PatchPreference::CreateNew => {
                "Create a new skill — only when no patch rung applies, and only with a \
                 stated reason the three patch rungs did not apply."
            }
        }
    }
}

/// A class of session artifact that must never appear in a skill name.
/// Closed enum: the prompt's ban text and the classifier both render from
/// [`SessionArtifactNameClass::ALL`], so the prose and the check share one
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionArtifactNameClass {
    /// Date stamps — a skill named after a day dies with the day
    /// (`fix-login-2026-09-01`).
    DateStamp,
    /// Ticket numbers — a tracker id is a session artifact, not a
    /// capability (`proj-1234-workaround`).
    TicketNumber,
    /// One-off task titles — the name names the task, not the skill
    /// (`tmp-migrate-users`).
    OneOffTaskTitle,
}

impl SessionArtifactNameClass {
    /// Every banned class, in check order (the classifier reports the
    /// first that matches).
    pub const ALL: [SessionArtifactNameClass; 3] = [
        SessionArtifactNameClass::DateStamp,
        SessionArtifactNameClass::TicketNumber,
        SessionArtifactNameClass::OneOffTaskTitle,
    ];

    /// The wire name (`date_stamp`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionArtifactNameClass::DateStamp => "date_stamp",
            SessionArtifactNameClass::TicketNumber => "ticket_number",
            SessionArtifactNameClass::OneOffTaskTitle => "one_off_task_title",
        }
    }

    /// The ban line the prompt renders for this class.
    pub fn ban_text(&self) -> &'static str {
        match self {
            SessionArtifactNameClass::DateStamp => {
                "date stamps — a skill named after a day dies with the day \
                 (`fix-login-2026-09-01`)"
            }
            SessionArtifactNameClass::TicketNumber => {
                "ticket numbers — a tracker id is a session artifact, not a capability \
                 (`proj-1234-workaround`)"
            }
            SessionArtifactNameClass::OneOffTaskTitle => {
                "one-off task titles — the name names the task, not the skill \
                 (`tmp-migrate-users`)"
            }
        }
    }
}

impl std::fmt::Display for PatchPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for SessionArtifactNameClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `true` when every character is an ASCII digit.
fn all_digits(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

/// `true` when every character is an ASCII letter.
fn all_letters(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The markers that open a one-off task title, as token sequences — a
/// skill named `tmp-…` or `one-off-…` names the task, not the capability.
const ONE_OFF_OPENERS: &[&[&str]] = &[
    &["tmp"],
    &["temp"],
    &["scratch"],
    &["adhoc"],
    &["ad", "hoc"],
    &["oneoff"],
    &["one", "off"],
    &["wip"],
    &["todo"],
    &["fixme"],
    &["hotfix"],
];

/// Classify a skill name against the session-artifact ban: `Some(class)`
/// when the name cites a session artifact, `None` when it is durable.
///
/// Deterministic and conservative: the name is lowercased and split on
/// non-alphanumerics, then the three classes check in declaration order —
/// a date-shaped token run (`2026-09-01`, or one token of six-plus
/// digits), then a ticket shape (a `#` anywhere, or a letters token
/// followed by a three-to-five-digit token), then a one-off opener. The
/// check is a floor, not a judge: a clean answer here means "not
/// obviously a session artifact", and the prompt ban remains the
/// authoritative instruction.
pub fn classify_session_artifact_name(name: &str) -> Option<SessionArtifactNameClass> {
    let lowered = name.to_ascii_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();

    // Date stamps: one long digit run (20260901, 202601), or a
    // year-month-day token triple (2026, 09, 01).
    for (index, token) in tokens.iter().enumerate() {
        if all_digits(token) && token.len() >= 6 {
            return Some(SessionArtifactNameClass::DateStamp);
        }
        if index + 2 < tokens.len()
            && token.len() == 4
            && all_digits(token)
            && tokens[index + 1].len() == 2
            && all_digits(tokens[index + 1])
            && tokens[index + 2].len() == 2
            && all_digits(tokens[index + 2])
        {
            return Some(SessionArtifactNameClass::DateStamp);
        }
    }

    // Ticket numbers: a `#` marker, or a letters token followed by a
    // three-to-five-digit token (`proj-1234`). Short digit runs stay free
    // — `oauth-2` and `utf-8` are versions, not tickets.
    if lowered.contains('#') {
        return Some(SessionArtifactNameClass::TicketNumber);
    }
    for pair in tokens.windows(2) {
        if all_letters(pair[0]) && all_digits(pair[1]) && (3..=5).contains(&pair[1].len()) {
            return Some(SessionArtifactNameClass::TicketNumber);
        }
    }

    // One-off task titles: the name opens with a task marker.
    for opener in ONE_OFF_OPENERS {
        if tokens.len() >= opener.len() && tokens[..opener.len()] == opener[..] {
            return Some(SessionArtifactNameClass::OneOffTaskTitle);
        }
    }

    None
}

/// Render the patch-before-create section of a review-fork or hunting
/// prompt. The section is a pure function of the two enums: rungs walk
/// [`PatchPreference::ALL`], each carrying its own instruction; the ban
/// walks [`SessionArtifactNameClass::ALL`]. There is no template — a
/// change to either type changes the prompt with no prompt edit.
pub fn render_patch_preference_section() -> String {
    let mut out = String::from(
        "## Skill authorship preference\n\n\
         When this review produces a skill draft, prefer the existing library in this order:\n",
    );
    for rung in PatchPreference::ALL {
        out.push_str(&format!("{}. {}\n", rung.rung(), rung.instruction()));
    }
    out.push_str(
        "\nSkill names must outlive the session that minted them. Names that cite session \
         artifacts are banned:\n",
    );
    for class in SessionArtifactNameClass::ALL {
        out.push_str(&format!("- {}\n", class.ban_text()));
    }
    out
}

/// Every way editorial governance can refuse a mutation. Module-local,
/// mirroring [`crate::skill::SkillError`]: the refused rule is named in
/// the type.
#[derive(Debug, Error)]
pub enum EditorialError {
    /// A `CreateNew` landing without the stated reason. The justification
    /// is what separates a considered creation from a junk-drawer habit —
    /// structurally mandatory, never optional.
    #[error(
        "a `create_new` landing must record why the three patch rungs did not apply — \
         the justification is structurally mandatory"
    )]
    MissingCreateNewJustification,

    /// A justification recorded against a patch rung. Justifications exist
    /// to explain *not* patching; carrying one on a patch rung is a
    /// contradiction the ledger refuses.
    #[error(
        "justifications belong to `create_new` landings only — rung `{rung}` patches, \
         and a patch needs no reason it did not patch"
    )]
    UnexpectedJustification {
        /// The rung the justification was recorded against.
        rung: PatchPreference,
    },

    /// The justification was empty or all whitespace.
    #[error("the `create_new` justification must be a non-empty, trimmed sentence")]
    EmptyJustification,

    /// The justification exceeded [`MAX_CREATE_NEW_JUSTIFICATION_LEN`].
    #[error(
        "the `create_new` justification exceeds the {MAX_CREATE_NEW_JUSTIFICATION_LEN}-byte \
         ceiling — the reason is a sentence, not a report"
    )]
    JustificationTooLong,

    /// Editorial provenance was attached to a non-skill candidate. The
    /// preference governs skill authorship; a memory-set candidate has no
    /// rung to land on.
    #[error(
        "editorial provenance attaches to skill candidates only — the patch-before-create \
         preference governs skill authorship"
    )]
    NotSkillCandidate,

    /// The drafted name cites a session artifact. Fail-closed: the class
    /// the classifier matched is named, so the producer can rename and
    /// retry.
    #[error(
        "skill name `{name}` cites a session artifact ({class}) — skill names must outlive \
         the session that minted them"
    )]
    SessionArtifactName {
        /// The offending name.
        name: String,
        /// The class the classifier matched.
        class: SessionArtifactNameClass,
    },

    /// A distribution window whose `since` is not before its `until` — an
    /// empty or inverted window is a caller bug, named rather than
    /// silently answered.
    #[error("invalid distribution window: `since` must be strictly before `until`")]
    InvalidWindow,
}

/// Which preference rung an automated `Create`/`Patch` mutation landed on,
/// with the justification a `CreateNew` landing owes. Recorded on the
/// mutation's ledger carrier (the learn plane's candidate), outside the
/// content address — attribution is not identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorialProvenance {
    /// The rung the producer landed on.
    pub rung: PatchPreference,

    /// Why the three patch rungs did not apply — present exactly when
    /// `rung` is [`PatchPreference::CreateNew`], absent otherwise (the
    /// invariant [`EditorialProvenance::validate`] enforces).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_new_justification: Option<String>,
}

impl EditorialProvenance {
    /// A patch-rung landing (no justification — patches need none).
    pub fn patch(rung: PatchPreference) -> Result<Self, EditorialError> {
        let provenance = Self {
            rung,
            create_new_justification: None,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    /// A `CreateNew` landing, with the producer's stated reason the three
    /// patch rungs did not apply.
    pub fn create_new(justification: impl Into<String>) -> Result<Self, EditorialError> {
        let provenance = Self {
            rung: PatchPreference::CreateNew,
            create_new_justification: Some(justification.into()),
        };
        provenance.validate()?;
        Ok(provenance)
    }

    /// The rung/justification invariant: a `CreateNew` landing carries a
    /// non-empty, bounded justification; a patch landing carries none.
    pub fn validate(&self) -> Result<(), EditorialError> {
        match self.rung {
            PatchPreference::CreateNew => {
                let reason = self
                    .create_new_justification
                    .as_ref()
                    .ok_or(EditorialError::MissingCreateNewJustification)?;
                if reason.is_empty() || reason != reason.trim() {
                    return Err(EditorialError::EmptyJustification);
                }
                if reason.len() > MAX_CREATE_NEW_JUSTIFICATION_LEN {
                    return Err(EditorialError::JustificationTooLong);
                }
                Ok(())
            }
            rung => {
                if self.create_new_justification.is_some() {
                    return Err(EditorialError::UnexpectedJustification { rung });
                }
                Ok(())
            }
        }
    }
}

/// The window a rung distribution covers. Both bounds optional: `since`
/// includes, `until` excludes (the half-open interval, so adjacent windows
/// never double-count a boundary mutation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistributionWindow {
    /// Include mutations at or after this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,

    /// Exclude mutations at or after this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
}

impl DistributionWindow {
    /// `since < until` when both are set — an empty or inverted window is
    /// a caller bug, named rather than silently answered.
    pub fn validate(&self) -> Result<(), EditorialError> {
        if let (Some(since), Some(until)) = (self.since, self.until) {
            if since >= until {
                return Err(EditorialError::InvalidWindow);
            }
        }
        Ok(())
    }

    /// `true` when `at` falls inside the window.
    fn admits(&self, at: DateTime<Utc>) -> bool {
        if let Some(since) = self.since {
            if at < since {
                return false;
            }
        }
        if let Some(until) = self.until {
            if at >= until {
                return false;
            }
        }
        true
    }
}

/// One rung's count in a distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RungCount {
    /// The rung.
    pub rung: PatchPreference,
    /// How many automated mutations landed on it inside the window.
    pub mutations: u64,
}

/// The taxonomy's standing health metric: how many automated mutations
/// landed on each preference rung over a window. Every rung is present in
/// preference order, zero-filled — a missing rung row is a rendering bug,
/// never data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RungDistribution {
    /// The window the distribution covers.
    pub window: DistributionWindow,
    /// One count per rung, in [`PatchPreference::ALL`] order.
    pub rungs: Vec<RungCount>,
    /// The total mutations inside the window.
    pub total: u64,
}

/// Aggregate editorial provenance over a window: one count per rung, in
/// preference order. The input is `(mutation time, rung)` pairs — callers
/// read their own ledger (the server's candidate store reads candidate
/// created-at plus provenance); the aggregation is pure and total.
pub fn rung_distribution<I>(
    mutations: I,
    window: &DistributionWindow,
) -> Result<RungDistribution, EditorialError>
where
    I: IntoIterator<Item = (DateTime<Utc>, PatchPreference)>,
{
    window.validate()?;
    let mut counts = [0u64; PatchPreference::ALL.len()];
    let mut total = 0u64;
    for (at, rung) in mutations {
        if window.admits(at) {
            counts[rung.rung() as usize - 1] += 1;
            total += 1;
        }
    }
    Ok(RungDistribution {
        window: *window,
        rungs: PatchPreference::ALL
            .iter()
            .enumerate()
            .map(|(index, rung)| RungCount {
                rung: *rung,
                mutations: counts[index],
            })
            .collect(),
        total,
    })
}
