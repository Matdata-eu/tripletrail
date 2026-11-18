//! Row-level policies are a TripleTrail Enterprise feature (#202, #818). The enterprise build
//! enforces them whatever its licence says: a lapsed licence never shows data a policy hides.
//! What the licence turns on is changing them: the access pages refuse a change to a policy
//! file that has row rules, before or after, unless the licence covers `row-policies`. The
//! open build cannot enforce them, and refuses policy files that have them.

use crate::Options;
use crate::policy::SG;
#[cfg(feature = "enterprise")]
use serde_json::{Value, json};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};

/// How a refusal to change row-level policies starts.
pub(crate) const REFUSED: &str = "Row-level policies";

/// Whether a policy file's Turtle has row rules (`sg:visibleIf`, `sg:hideProperties`,
/// `sg:editableIf`). A text that does not parse has none here; the policy check refuses it.
pub(crate) fn has_row_rules(text: &str) -> bool {
    let row = ["visibleIf", "hideProperties", "editableIf"].map(|l| format!("{SG}{l}"));
    RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(text.as_bytes())
        .filter_map(Result::ok)
        .any(|q| row.iter().any(|r| q.predicate.as_str() == r))
}

/// Whether this server may change row-level policies now, or why not.
pub(crate) fn editable(options: &Options) -> Result<(), String> {
    #[cfg(feature = "enterprise")]
    {
        use tripletrail_enterprise::features::ROW_POLICIES;
        let status = options.license_status();
        if status.allows(ROW_POLICIES) {
            return Ok(());
        }
        Err(format!(
            "Row-level policies stay enforced, but changing them needs a TripleTrail Enterprise \
             licence that covers {ROW_POLICIES}: {}",
            reason(&status)
        ))
    }
    #[cfg(not(feature = "enterprise"))]
    {
        let _ = options;
        Err(tripletrail::NEEDS_ENTERPRISE.to_owned())
    }
}

#[cfg(feature = "enterprise")]
fn reason(status: &tripletrail_enterprise::license::Status) -> String {
    match (&status.license, status.warning()) {
        (Some(_), Some(warning)) if !status.active() => warning,
        (Some(l), _) => format!("licence {} does not list it", l.id),
        (None, Some(warning)) => warning,
        (None, None) => "no licence is installed".to_owned(),
    }
}

/// Refuses changing a policy file from `before` to `after` when either has row rules and
/// they may not be changed now.
pub(crate) fn check_change(options: &Options, before: &str, after: &str) -> Result<(), String> {
    if before != after && (has_row_rules(before) || has_row_rules(after)) {
        editable(options)?;
    }
    Ok(())
}

/// The state of row-level policies, for `/$/license`.
#[cfg(feature = "enterprise")]
pub(crate) fn to_json(options: &Options, in_use: bool) -> Value {
    let editable = editable(options);
    json!({
        "inUse": in_use,
        "enforced": tripletrail::row_policies_available(),
        "editable": editable.is_ok(),
        "reason": editable.err(),
    })
}
