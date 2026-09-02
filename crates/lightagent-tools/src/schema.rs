//! A small, dependency-free JSON-Schema validator.
//!
//! It is a *guard rail for local execution*, not a spec-complete implementation.
//! It supports exactly the keywords the built-in tool schemas use — enough to
//! reject a malformed argument object before a tool runs — and **ignores**
//! every keyword it does not know (`$ref`, `allOf`/`anyOf`/`oneOf`, `format`,
//! `pattern`, tuple `items`) rather than failing on it. That mirrors the wire
//! layer's stance of carrying a schema untouched: a keyword this validator does
//! not enforce is not an error, it is simply not enforced here.
//!
//! Supported: `type` (object/array/string/number/integer/boolean/null),
//! `properties` + `required` + `additionalProperties` (bool), `items` (single
//! schema) + `minItems`/`maxItems`, `enum`, `minimum`/`maximum`,
//! `minLength`/`maxLength`. Every error names the JSON-pointer path at fault.

use serde_json::Value;

/// One reason an instance did not satisfy a schema.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{pointer}: {message}")]
pub struct SchemaError {
    /// The JSON-pointer path to the offending value (`""` is the root).
    pub pointer: String,
    /// What was wrong there.
    pub message: String,
}

impl SchemaError {
    fn new(pointer: &str, message: impl Into<String>) -> Self {
        Self {
            pointer: if pointer.is_empty() {
                "/".into()
            } else {
                pointer.into()
            },
            message: message.into(),
        }
    }
}

/// Validate `instance` against `schema`, collecting every violation.
///
/// Returns `Ok(())` when the instance satisfies every supported keyword. A
/// non-object schema (e.g. `true`) imposes no constraints and always passes.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<SchemaError>> {
    let mut errors = Vec::new();
    check(schema, instance, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check(schema: &Value, instance: &Value, pointer: &str, errors: &mut Vec<SchemaError>) {
    let Some(schema) = schema.as_object() else {
        // A boolean or otherwise non-object schema constrains nothing here.
        return;
    };

    if let Some(type_value) = schema.get("type") {
        check_type(type_value, instance, pointer, errors);
    }

    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.iter().any(|candidate| candidate == instance)
    {
        errors.push(SchemaError::new(
            pointer,
            "value is not one of the permitted `enum` values",
        ));
    }

    match instance {
        Value::Object(map) => check_object(schema, map, pointer, errors),
        Value::Array(items) => check_array(schema, items, pointer, errors),
        Value::String(text) => check_string(schema, text, pointer, errors),
        Value::Number(number) => check_number(schema, number.as_f64(), pointer, errors),
        _ => {}
    }
}

fn check_type(type_value: &Value, instance: &Value, pointer: &str, errors: &mut Vec<SchemaError>) {
    let Some(expected) = type_value.as_str() else {
        // Only the single-string form of `type` is supported; anything else is
        // not enforced rather than treated as a failure.
        return;
    };
    let ok = match expected {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        "integer" => is_integer(instance),
        _ => true, // an unknown type name is not enforced
    };
    if !ok {
        errors.push(SchemaError::new(
            pointer,
            format!("expected type `{expected}`"),
        ));
    }
}

fn is_integer(instance: &Value) -> bool {
    match instance {
        Value::Number(number) => {
            number.is_i64()
                || number.is_u64()
                || number.as_f64().is_some_and(|value| value.fract() == 0.0)
        }
        _ => false,
    }
}

fn check_object(
    schema: &serde_json::Map<String, Value>,
    map: &serde_json::Map<String, Value>,
    pointer: &str,
    errors: &mut Vec<SchemaError>,
) {
    if let Some(Value::Array(required)) = schema.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(name) {
                errors.push(SchemaError::new(
                    pointer,
                    format!("missing required property `{name}`"),
                ));
            }
        }
    }

    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(properties) = properties {
        for (name, subschema) in properties {
            if let Some(child) = map.get(name) {
                check(subschema, child, &child_pointer(pointer, name), errors);
            }
        }
    }

    // `additionalProperties: false` forbids any property not named in
    // `properties`. Only the boolean form is enforced.
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        for name in map.keys() {
            let declared = properties.is_some_and(|properties| properties.contains_key(name));
            if !declared {
                errors.push(SchemaError::new(
                    pointer,
                    format!("property `{name}` is not permitted"),
                ));
            }
        }
    }
}

fn check_array(
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    pointer: &str,
    errors: &mut Vec<SchemaError>,
) {
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && (items.len() as u64) < min
    {
        errors.push(SchemaError::new(
            pointer,
            format!("array has fewer than {min} items"),
        ));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && (items.len() as u64) > max
    {
        errors.push(SchemaError::new(
            pointer,
            format!("array has more than {max} items"),
        ));
    }
    // Only the single-schema form of `items` is enforced (not tuple `items`).
    if let Some(subschema @ Value::Object(_)) = schema.get("items") {
        for (index, item) in items.iter().enumerate() {
            check(subschema, item, &format!("{pointer}/{index}"), errors);
        }
    }
}

fn check_string(
    schema: &serde_json::Map<String, Value>,
    text: &str,
    pointer: &str,
    errors: &mut Vec<SchemaError>,
) {
    let length = text.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && length < min
    {
        errors.push(SchemaError::new(
            pointer,
            format!("string is shorter than {min} characters"),
        ));
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
        && length > max
    {
        errors.push(SchemaError::new(
            pointer,
            format!("string is longer than {max} characters"),
        ));
    }
}

fn check_number(
    schema: &serde_json::Map<String, Value>,
    value: Option<f64>,
    pointer: &str,
    errors: &mut Vec<SchemaError>,
) {
    let Some(value) = value else { return };
    if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
        && value < min
    {
        errors.push(SchemaError::new(
            pointer,
            format!("value is below the minimum {min}"),
        ));
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
        && value > max
    {
        errors.push(SchemaError::new(
            pointer,
            format!("value is above the maximum {max}"),
        ));
    }
}

/// A JSON pointer for `name` under `parent`, escaping `~` and `/` per RFC 6901.
fn child_pointer(parent: &str, name: &str) -> String {
    let escaped = name.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "tz": { "enum": ["utc"] },
                "count": { "type": "integer", "minimum": 1 },
            },
            "required": ["tz"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn accepts_a_valid_instance() {
        assert!(validate(&object_schema(), &json!({ "tz": "utc", "count": 3 })).is_ok());
    }

    #[test]
    fn missing_required_property_names_it() {
        let errors = validate(&object_schema(), &json!({ "count": 1 })).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("required property `tz`"))
        );
    }

    #[test]
    fn wrong_type_is_rejected() {
        let errors = validate(&json!({ "type": "string" }), &json!(7)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("expected type `string`"));
    }

    #[test]
    fn enum_mismatch_is_rejected() {
        let errors = validate(&object_schema(), &json!({ "tz": "pst" })).unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("enum")));
    }

    #[test]
    fn integer_rejects_a_fraction() {
        let errors = validate(&json!({ "type": "integer" }), &json!(1.5)).unwrap_err();
        assert!(errors[0].message.contains("expected type `integer`"));
    }

    #[test]
    fn additional_property_is_rejected() {
        let errors = validate(&object_schema(), &json!({ "tz": "utc", "extra": 1 })).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("`extra` is not permitted"))
        );
    }

    #[test]
    fn unknown_keyword_is_ignored() {
        // `$ref`, `format` and `pattern` are not enforced, so they never fail.
        let schema = json!({ "type": "string", "format": "email", "$ref": "#/x", "pattern": "^a" });
        assert!(validate(&schema, &json!("anything")).is_ok());
    }

    #[test]
    fn error_pointer_locates_a_nested_field() {
        let schema = json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } },
        });
        let errors = validate(&schema, &json!({ "count": "no" })).unwrap_err();
        assert_eq!(errors[0].pointer, "/count");
    }
}
