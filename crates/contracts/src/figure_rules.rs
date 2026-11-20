//! Figure rules that the figure engine, the bridge's review gate and the desktop share: the diagram origin codes, the
//! evidence review states, the visual review id, the image rotation bound and the decimal digit bound. Plain constants;
//! nothing here is serialized.

/// Origin code of a diagram element taken from paper evidence. It needs an evidence reference.
pub const PAPER_EVIDENCE_ORIGIN: &str = "paper_evidence";
/// Origin code of a diagram element the user wrote.
pub const USER_AUTHORED_ORIGIN: &str = "user_authored";
/// Origin code of a diagram element the model proposed. A connector with this origin and no evidence
/// raises UNSOURCED_PROPOSAL.
pub const MODEL_PROPOSAL_ORIGIN: &str = "model_proposal";

/// The origin codes a diagram node or edge may carry. The figure engine refuses any other code.
pub const DIAGRAM_ORIGINS: [&str; 3] = [PAPER_EVIDENCE_ORIGIN, USER_AUTHORED_ORIGIN, MODEL_PROPOSAL_ORIGIN];

/// `Evidence.review_state` of a record no person has reviewed yet.
pub const REVIEW_STATE_UNREVIEWED: &str = "unreviewed";
/// `Evidence.review_state` of a record a person provided.
pub const REVIEW_STATE_USER_PROVIDED: &str = "user_provided";
/// `Evidence.review_state` of a record a person reviewed.
pub const REVIEW_STATE_REVIEWED: &str = "reviewed";
/// The review states an evidence record may carry. The figure engine refuses any other state.
pub const REVIEW_STATES: [&str; 3] = [REVIEW_STATE_UNREVIEWED, REVIEW_STATE_USER_PROVIDED, REVIEW_STATE_REVIEWED];

/// The id the figure engine reports in `changed_ids` for its visual review. The review has no element of its own, so no
/// element may use it.
pub const VISUAL_REVIEW_ID: &str = "visual_review";

/// The bound on an image's rotation magnitude in millidegrees (1/1000 degree). A rotation is valid when its magnitude
/// is below this value, so the valid range lies strictly inside (-360, 360) degrees.
pub const IMAGE_ROTATION_LIMIT_MILLIDEG: i32 = 360_000;

/// The most digits a decimal value may have, integer and fraction digits together. Longer text is refused.
pub const DECIMAL_MAX_DIGITS: usize = 18;
