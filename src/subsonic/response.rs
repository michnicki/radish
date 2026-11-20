//! Subsonic responses: the `subsonic-response` envelope in JSON or XML.
//!
//! A method's payload is a serde value. JSON writes it as it is. XML follows
//! the Subsonic convention: a scalar field becomes an attribute, an object a
//! child element, and an array repeated elements of the field's name.

use crate::output::Clean;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value, json};

/// The Subsonic API version served.
pub(crate) const API_VERSION: &str = "1.16.1";

/// How a response is written: JSON for `f=json`, XML for anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Json,
    Xml,
}

impl Format {
    /// The format a request asks for.
    pub(crate) fn of(params: &super::Params) -> Self {
        if params.get("f") == Some("json") {
            Self::Json
        } else {
            Self::Xml
        }
    }
}

/// The fields a method adds to the envelope.
#[derive(Debug, Default)]
pub(crate) struct Payload(Map<String, Value>);

impl Payload {
    /// No fields: a bare `status="ok"`.
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// One field, `key`, holding `value`.
    pub(crate) fn new(key: &str, value: impl Serialize) -> Self {
        let mut fields = Map::new();
        fields.insert(key.to_string(), to_value(value));
        Self(fields)
    }
}

/// A serde value of an API object. These are plain structs with string keys,
/// which always serialize.
pub(crate) fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("API objects serialize to JSON")
}

/// A failed call, answered with `status="failed"` and HTTP 200, as Subsonic
/// clients expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    pub(crate) code: u16,
    pub(crate) message: String,
}

impl ApiError {
    fn with(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Code 0: the server doesn't offer `method`.
    pub(crate) fn unsupported(method: &str) -> Self {
        Self::with(0, format!("{method} is not supported"))
    }

    /// Code 70: nothing has the ID asked for.
    pub(crate) fn not_found(what: &str) -> Self {
        Self::with(70, format!("{what} not found"))
    }

    /// Code 0: `name` has a value the method can't use.
    pub(crate) fn invalid(name: &str) -> Self {
        Self::with(0, format!("Invalid value for parameter {name}"))
    }

    /// Code 10: the method needs `name`.
    pub(crate) fn missing(name: &str) -> Self {
        Self::with(10, format!("Required parameter is missing: {name}"))
    }

    /// Code 40.
    pub(crate) fn wrong_credentials() -> Self {
        Self::with(40, "Wrong username or password")
    }

    /// Code 42: API keys are not supported.
    pub(crate) fn unsupported_auth() -> Self {
        Self::with(42, "API keys are not supported; use a password or a token")
    }

    /// Code 43: a password and a token in one request.
    pub(crate) fn conflicting_auth() -> Self {
        Self::with(43, "Send a password or a token and salt, not both")
    }
}

impl From<anyhow::Error> for ApiError {
    /// Logs the cause, which may name paths, and answers with a generic
    /// message.
    fn from(error: anyhow::Error) -> Self {
        eprintln!("error: {}", Clean(&format!("{error:#}")));
        Self::with(0, "Internal error; the server log says why")
    }
}

/// A successful response carrying `payload`.
pub(crate) fn ok(format: Format, payload: Payload) -> Response {
    render(format, envelope("ok", payload.0))
}

/// A failed response.
pub(crate) fn failed(format: Format, error: &ApiError) -> Response {
    let mut fields = Map::new();
    fields.insert(
        "error".to_string(),
        json!({"code": error.code, "message": error.message}),
    );
    render(format, envelope("failed", fields))
}

/// `fields` with the attributes every response carries.
fn envelope(status: &str, mut fields: Map<String, Value>) -> Map<String, Value> {
    fields.insert("status".to_string(), status.into());
    fields.insert("version".to_string(), API_VERSION.into());
    fields.insert("type".to_string(), "radish".into());
    fields.insert(
        "serverVersion".to_string(),
        env!("CARGO_PKG_VERSION").into(),
    );
    fields.insert("openSubsonic".to_string(), true.into());
    fields
}

fn render(format: Format, mut fields: Map<String, Value>) -> Response {
    match format {
        Format::Json => {
            let body = json!({ "subsonic-response": fields }).to_string();
            ([(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Format::Xml => {
            fields.insert("xmlns".to_string(), "http://subsonic.org/restapi".into());
            let mut body = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            write_element(&mut body, "subsonic-response", &Value::Object(fields));
            ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], body).into_response()
        }
    }
}

/// Writes `value` as the element `name`. In an object, scalar fields become
/// attributes, objects child elements and arrays repeated child elements; a
/// scalar in an array becomes an element with that text. Nulls are left out,
/// as fields and as array items.
/// Fields come in key order, since serde_json keeps keys sorted.
fn write_element(out: &mut String, name: &str, value: &Value) {
    out.push('<');
    out.push_str(name);
    let Value::Object(fields) = value else {
        out.push('>');
        push_escaped(out, &scalar(value).unwrap_or_default(), false);
        out.push_str("</");
        out.push_str(name);
        out.push('>');
        return;
    };
    for (key, field) in fields {
        if let Some(text) = scalar(field) {
            out.push(' ');
            out.push_str(key);
            out.push_str("=\"");
            push_escaped(out, &text, true);
            out.push('"');
        }
    }
    let mut children = fields
        .iter()
        .filter(|(_, field)| field.is_object() || field.is_array())
        .peekable();
    if children.peek().is_none() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for (key, child) in children {
        match child {
            Value::Array(items) => {
                for item in items.iter().filter(|item| !item.is_null()) {
                    write_element(out, key, item);
                }
            }
            _ => write_element(out, key, child),
        }
    }
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

/// The text of a string, number or boolean; `None` for anything else.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Appends `text` escaped for XML.
///
/// Tags can hold control characters that XML 1.0 can't carry even escaped, and
/// one of them would break the whole response, so they become U+FFFD. In an
/// attribute, tabs and line breaks are written as character references so
/// parsers keep them. A carriage return always is, because parsers would
/// otherwise turn it into a line feed.
fn push_escaped(out: &mut String, text: &str, attribute: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\t' if attribute => out.push_str("&#9;"),
            '\n' if attribute => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' | '\n' => out.push(c),
            c if is_xml_char(c) => out.push(c),
            _ => out.push('\u{fffd}'),
        }
    }
}

/// Whether XML 1.0 allows `c` (apart from tab and line breaks, handled above).
fn is_xml_char(c: char) -> bool {
    matches!(c, '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn xml(value: Value) -> String {
        let mut out = String::new();
        write_element(&mut out, "r", &value);
        out
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn xml_puts_scalars_in_attributes_and_repeats_array_elements() {
        let value = json!({
            "name": "A", "count": 2, "ok": true, "missing": null,
            "child": {"id": "x"}, "item": [{"id": 1}, {"id": 2}], "text": ["One", null, "Two"],
            "none": [], "empty": {}
        });
        assert_eq!(
            xml(value),
            "<r count=\"2\" name=\"A\" ok=\"true\"><child id=\"x\"/><empty/>\
             <item id=\"1\"/><item id=\"2\"/><text>One</text><text>Two</text></r>"
        );
    }

    #[test]
    fn xml_escapes_markup_and_replaces_characters_it_cannot_carry() {
        assert_eq!(
            xml(json!({"title": "a<b>&\"c\u{1}"})),
            "<r title=\"a&lt;b&gt;&amp;&quot;c\u{fffd}\"/>"
        );
        assert_eq!(
            xml(json!({"title": "tab\there\nline\r"})),
            "<r title=\"tab&#9;here&#10;line&#13;\"/>"
        );
        assert_eq!(
            xml(json!({"t": ["x\ty\u{fffe}"]})),
            "<r><t>x\ty\u{fffd}</t></r>"
        );
    }

    #[tokio::test]
    async fn json_envelopes_carry_the_opensubsonic_fields() {
        let response = ok(
            Format::Json,
            Payload::new("license", json!({"valid": true})),
        );
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let value: Value = serde_json::from_str(&body(response).await).unwrap();
        let envelope = &value["subsonic-response"];
        assert_eq!(envelope["status"], "ok");
        assert_eq!(envelope["version"], API_VERSION);
        assert_eq!(envelope["type"], "radish");
        assert_eq!(envelope["serverVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(envelope["openSubsonic"], true);
        assert_eq!(envelope["license"]["valid"], true);
    }

    #[tokio::test]
    async fn errors_are_failed_xml_envelopes_with_http_200() {
        let response = failed(Format::Xml, &ApiError::unsupported("getVideos"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/xml; charset=utf-8"
        );
        let text = body(response).await;
        assert!(
            text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<subsonic-response ")
        );
        assert!(text.contains(" status=\"failed\""));
        assert!(text.contains(" xmlns=\"http://subsonic.org/restapi\""));
        assert!(text.contains("<error code=\"0\" message=\"getVideos is not supported\"/>"));
    }
}
