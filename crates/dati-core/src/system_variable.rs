//! The variables the server fills in itself: who is calling, and what time it is.
//!
//! The Java `SystemVariableResolver` read them from the request context and the system clock, and
//! the set is the one the frontend offers in its template completions. A custom tool's parameter
//! list never has to declare them, and `prompts/extract` does not report them either.

use chrono::Local;
use serde_json::{Map, Value};

use crate::auth::AuthUser;

pub const SYSTEM_VARIABLES: [&str; 5] = [
    "_user.id",
    "_user.name",
    "_user.display_name",
    "_now",
    "_date",
];

pub fn is_system_variable(name: &str) -> bool {
    SYSTEM_VARIABLES.contains(&name)
}

/// The five variables as a JSON object. Without a caller the three user entries are still present,
/// holding `null`, which is what made an `{{#if _user.id}}` branch go falsy in the Java renderer.
pub fn resolve(caller: Option<&AuthUser>) -> Map<String, Value> {
    let text = |value: Option<String>| match value.filter(|v| !v.is_empty()) {
        Some(value) => Value::String(value),
        None => Value::Null,
    };
    let mut vars = Map::new();
    vars.insert("_user.id".into(), text(caller.map(|c| c.id.clone())));
    vars.insert("_user.name".into(), text(caller.map(|c| c.name.clone())));
    vars.insert(
        "_user.display_name".into(),
        text(caller.and_then(|c| c.display_name.clone())),
    );
    // Local wall-clock time, not the UTC instants the metadata store keeps: a template renders these
    // for a person, and the Java formatter was `yyyy-MM-dd HH:mm:ss`.
    let now = Local::now();
    vars.insert(
        "_now".into(),
        Value::String(now.format("%Y-%m-%d %H:%M:%S").to_string()),
    );
    vars.insert(
        "_date".into(),
        Value::String(now.format("%Y-%m-%d").to_string()),
    );
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_builtins_are_recognised_and_nothing_else_is() {
        for name in SYSTEM_VARIABLES {
            assert!(is_system_variable(name), "{name} is a system variable");
        }
        for name in ["city", "user.id", "_now2", "", "_USER.ID"] {
            assert!(!is_system_variable(name), "{name} is not a system variable");
        }
    }

    #[test]
    fn a_caller_fills_the_user_variables_and_the_clock_fills_the_rest() {
        let caller = AuthUser {
            id: "u-1".into(),
            name: "alice".into(),
            display_name: Some("Alice".into()),
        };
        let vars = resolve(Some(&caller));
        assert_eq!(vars["_user.id"], Value::String("u-1".into()));
        assert_eq!(vars["_user.name"], Value::String("alice".into()));
        assert_eq!(vars["_user.display_name"], Value::String("Alice".into()));
        assert_eq!(vars.len(), 5);

        let now = vars["_now"].as_str().unwrap();
        assert_eq!(now.len(), 19, "{now} reads as `yyyy-MM-dd HH:mm:ss`");
        assert_eq!(&now[..10], vars["_date"].as_str().unwrap());

        // An anonymous caller still gets the keys, so a template branch resolves rather than misses.
        let vars = resolve(None);
        assert_eq!(vars["_user.id"], Value::Null);
        assert_eq!(vars["_user.display_name"], Value::Null);
        assert!(vars.contains_key("_now"));
    }
}
