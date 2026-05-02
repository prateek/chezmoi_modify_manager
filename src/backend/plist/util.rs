//! Shared helpers for the plist backend submodules.

/// Human-readable name for a `plist::Value` variant. Used in error messages.
pub(super) fn value_kind(v: &plist::Value) -> &'static str {
    match v {
        plist::Value::Array(_) => "array",
        plist::Value::Dictionary(_) => "dictionary",
        plist::Value::Boolean(_) => "boolean",
        plist::Value::Data(_) => "data",
        plist::Value::Date(_) => "date",
        plist::Value::Real(_) => "real",
        plist::Value::Integer(_) => "integer",
        plist::Value::String(_) => "string",
        plist::Value::Uid(_) => "uid",
        _ => "unknown",
    }
}
