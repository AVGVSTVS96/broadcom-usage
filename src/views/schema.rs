use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema};

use super::{CreditsView, LiveSession, SessionUsage, UsageView};

/// Every field of every answer is always present, and nothing else is.
fn exact(schema: &mut Schema) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let Some(properties) = object.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    let required: Vec<serde_json::Value> = properties.keys().map(|k| k.clone().into()).collect();
    object.insert("required".into(), required.into());
    object.insert("additionalProperties".into(), false.into());
}

fn schema_of<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .with_transform(RecursiveTransform(exact))
        .into_generator()
        .into_root_schema_for::<T>()
}

/// The JSON Schema of each answer, by the file name it's published under in api/.
pub fn schemas() -> [(&'static str, Schema); 4] {
    [
        ("usage", schema_of::<UsageView>()),
        ("sessions", schema_of::<SessionUsage>()),
        ("credits", schema_of::<CreditsView>()),
        ("live-session", schema_of::<LiveSession>()),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use super::*;

    fn api() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/api"))
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// `UPDATE_SCHEMAS=1 cargo test schemas` rewrites them.
    #[test]
    fn committed_schemas_match_the_types() {
        for (name, schema) in schemas() {
            let path = api().join(format!("{name}.schema.json"));
            let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
            if std::env::var_os("UPDATE_SCHEMAS").is_some() {
                std::fs::write(&path, text).unwrap();
            } else {
                assert!(
                    std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()),
                    "{} drifted from the types; run UPDATE_SCHEMAS=1 cargo test schemas",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn examples_follow_their_schemas() {
        let validator = |name: &str| {
            let schema = schemas().into_iter().find(|(n, _)| *n == name).unwrap().1;
            jsonschema::validator_for(&schema.to_value()).unwrap()
        };
        let check = |name: &str, example: &str, value: &Value| {
            let errors: Vec<String> = validator(name)
                .iter_errors(value)
                .map(|e| format!("{e} at {}", e.instance_path()))
                .collect();
            assert!(errors.is_empty(), "{example} against {name}: {errors:#?}");
        };
        let examples = api().join("examples");
        for (name, file) in [
            ("usage", "usage.json"),
            ("sessions", "sessions.json"),
            ("credits", "credits.json"),
        ] {
            check(name, file, &read(&examples.join(file)));
        }
        for file in ["events-snapshot.json", "events-patch.json"] {
            let frame = read(&examples.join(file));
            for (key, value) in frame["live"].as_object().unwrap() {
                if key.starts_with("s:") && !value.is_null() {
                    check("live-session", &format!("{file} {key}"), value);
                }
            }
        }
    }
}
